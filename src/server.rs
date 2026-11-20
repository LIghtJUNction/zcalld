//! Fixed worker gateway. A generation occupies one worker until it completes.
use crate::{
    config::{Auth, Config, Provider},
    curl::{self, Client, Sink},
    http::{self, Chunked, Request},
    plugins::{Plugins, ProviderResponse},
    protocol::{self, Protocol},
    system::{self, STOPPING},
    transport::{Listeners, Stream},
};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

const MAX_GROUP_ATTEMPTS: usize = 4;
const RETRY_BODY_LIMIT: usize = 64 * 1024;

pub fn serve(config: Config) -> Result<(), String> {
    system::setup_signals().map_err(|e| e.to_string())?;
    curl::init()?;
    let mut listeners = Listeners::bind(&config).map_err(|e| format!("listen: {e}"))?;
    let address = listeners.description().to_owned();
    let (sender, receiver) = mpsc::sync_channel::<Stream>(config.queue);
    let receiver = Arc::new(Mutex::new(receiver));
    let config = Arc::new(config);
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut workers = Vec::new();
    for index in 0..config.workers {
        let receiver = receiver.clone();
        let config = config.clone();
        let ready = ready_tx.clone();
        let worker = std::thread::Builder::new()
            .name(format!("zcalld-{index}"))
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let resources = Plugins::load(&config.plugins)
                    .and_then(|plugins| Client::new().map(|client| (plugins, client)));
                match resources {
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                    Ok((mut plugins, mut client)) => {
                        let _ = ready.send(Ok(()));
                        loop {
                            let next = match receiver.lock() {
                                Ok(lock) => lock.recv(),
                                Err(_) => break,
                            };
                            let Ok(mut socket) = next else {
                                break;
                            };
                            if STOPPING.load(Ordering::Relaxed) {
                                break;
                            }
                            let _ = socket.set_nodelay(true);
                            let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
                            handle(&mut socket, &config, &mut plugins, &mut client);
                        }
                    }
                }
            })
            .map_err(|e| format!("start worker: {e}"))?;
        workers.push(worker);
    }
    drop(ready_tx);
    let mut startup_error = None;
    for _ in 0..config.workers {
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                startup_error = Some(error);
                break;
            }
            Err(_) => {
                startup_error = Some("worker initialization failed".into());
                break;
            }
        }
    }
    let result = if let Some(error) = startup_error {
        Err(error)
    } else {
        system::notify(&format!("READY=1\nSTATUS=Listening on {address}"))
            .map_err(|e| format!("systemd notification: {e}"))
            .and_then(|()| {
                eprintln!(
                    "zcalld {} listening on {address} ({} workers, {} plugins)",
                    env!("CARGO_PKG_VERSION"),
                    config.workers,
                    config.plugins.len()
                );
                accept(&mut listeners, &sender)
            })
    };
    STOPPING.store(true, Ordering::Relaxed);
    let _ = system::notify("STOPPING=1\nSTATUS=Stopping workers");
    drop(listeners);
    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
    result
}

fn accept(listeners: &mut Listeners, sender: &mpsc::SyncSender<Stream>) -> Result<(), String> {
    while !STOPPING.load(Ordering::Relaxed) {
        let Some(socket) = listeners.accept().map_err(|e| format!("accept: {e}"))? else {
            continue;
        };
        match sender.try_send(socket) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(mut socket)) => {
                let _ = socket.set_write_timeout(Some(Duration::from_millis(100)));
                let _ = http::error(
                    &mut socket,
                    503,
                    "all gateway workers and queue slots are busy",
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err("all gateway workers exited".into());
            }
        }
    }
    Ok(())
}

fn handle(socket: &mut Stream, config: &Config, plugins: &mut Plugins, client: &mut Client) {
    let request = match http::read_request(
        socket,
        config.max_body,
        Duration::from_millis(config.request_timeout_ms),
    ) {
        Ok(request) => request,
        Err(error) => {
            http::reject_and_close(socket, error.status, error.message);
            return;
        }
    };
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if socket.requires_loopback_host()
        && request
            .header("host")
            .is_some_and(|host| !loopback_host(host))
    {
        let _ = http::error(socket, 403, "loopback gateway requires a loopback Host");
        return;
    }
    if request.method == "GET" && path == "/healthz" {
        let body = format!(
            "{{\"status\":\"ok\",\"version\":{}}}",
            http::quote(env!("CARGO_PKG_VERSION"))
        );
        let _ = http::respond(socket, 200, "application/json", body.as_bytes());
        return;
    }
    if !authenticated(&request, config, socket) {
        let _ = http::error(socket, 401, "invalid gateway token");
        return;
    }
    if request.method == "GET" && path == "/v1/models" {
        let body = models_json(config);
        let _ = http::respond(socket, 200, "application/json", body.as_bytes());
        return;
    }
    let caller_key_id = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| request.header("x-api-key"))
        .or_else(|| request.header("x-goog-api-key"))
        .and_then(|value| {
            config
                .caller_keys
                .as_ref()
                .and_then(|keys| keys.id(value, socket.is_loopback_peer()))
        });
    if crate::plugin_http::try_handle(socket, &request, config, plugins, caller_key_id) {
        return;
    }
    if request.method != "POST" {
        let _ = http::error(socket, 405, "use POST for generation or GET /v1/models");
        return;
    }
    let (source, path_model, path_stream) = match ingress(path) {
        Ok(route) => route,
        Err(message) => {
            let _ = http::error(socket, 404, &message);
            return;
        }
    };
    let inspect = match protocol::inspect(source, &request.body, path_model.as_deref()) {
        Ok(value) => value,
        Err(error) => {
            let _ = http::error(socket, 400, &error);
            return;
        }
    };
    if plugins.has_context() {
        let context = match request_context(
            &request,
            source,
            &inspect,
            path,
            inspect.stream || path_stream,
            caller_workspace(&request, config, socket),
            &socket.routing_peer(),
        ) {
            Ok(context) => context,
            Err(error) => {
                let _ = http::error(socket, 400, &error);
                return;
            }
        };
        if let Err(error) = plugins.context(&context) {
            let _ = http::error(socket, 503, &error);
            return;
        }
    }
    let patched = match plugins.request(protocol_id(source), &request.body, config.max_body) {
        Ok(value) => value,
        Err(error) => {
            let _ = http::error(socket, request_plugin_status(&error), &error);
            return;
        }
    };
    let original = patched.as_deref().unwrap_or(&request.body);
    let patched_inspection = if patched.is_some() {
        match protocol::inspect(source, original, path_model.as_deref()) {
            Ok(value) => Some(value),
            Err(error) => {
                let _ = http::error(socket, 400, &error);
                return;
            }
        }
    } else {
        None
    };
    let effective = patched_inspection.as_ref().unwrap_or(&inspect);
    let stream = effective.stream || path_stream;
    let mut selected = match plugins.route(&effective.model) {
        Ok(model) => model,
        Err(error) => {
            let _ = http::error(socket, 403, &error);
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_millis(config.upstream_timeout_ms);
    let mut attempted = Vec::with_capacity(MAX_GROUP_ATTEMPTS);
    let mut retry_body: Option<Vec<u8>> = None;
    for attempt in 0..MAX_GROUP_ATTEMPTS {
        let Some(route) = config.route(&selected) else {
            let _ = http::error(
                socket,
                404,
                "unknown model; list configured models with GET /v1/models",
            );
            return;
        };
        let provider = &config.providers[route.provider];
        let (target_protocol, target_url, target_auth) = provider.for_protocol(source);
        attempted.push((route.provider, target_protocol, route.model.clone()));
        let input = retry_body.as_deref().unwrap_or(original);
        let body = match protocol::prepare_request(
            source,
            target_protocol,
            input,
            &route.model,
            stream,
            config.default_max_tokens,
        ) {
            Ok(body) if body.len() <= config.max_body => body,
            Ok(_) => {
                let _ = http::error(socket, 413, "converted request exceeds max_body_bytes");
                return;
            }
            Err(error) => {
                let _ = http::error(socket, 422, &error);
                return;
            }
        };
        let url = match protocol::endpoint(target_url, target_protocol, &route.model, stream) {
            Ok(url) => url,
            Err(error) => {
                let _ = http::error(socket, 500, &error);
                return;
            }
        };
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u64::MAX as u128) as u64;
        if remaining == 0 || STOPPING.load(Ordering::Relaxed) {
            let _ = http::error(
                socket,
                503,
                "gateway request deadline reached or service is stopping",
            );
            return;
        }
        let headers = upstream_headers(provider, &request, stream, target_protocol, target_auth);
        let options = curl::Options {
            url: &url,
            body: &body,
            headers: &headers,
            timeout_ms: remaining,
            connect_timeout_ms: config.connect_timeout_ms.min(remaining),
        };
        let forward = ForwardOptions {
            source: target_protocol,
            target: source,
            model: &inspect.model,
            stream,
            maximum: config.max_response,
            defer_rejections: plugins.has_retry() && attempt + 1 < MAX_GROUP_ATTEMPTS,
        };
        let upstream = Upstream {
            client,
            plugins,
            provider,
            model: &route.model,
        };
        let attempt_started = Instant::now();
        let mut outcome = if stream && source != target_protocol {
            match protocol::with_stream_converter(
                target_protocol,
                source,
                &inspect.model,
                |transform| execute(upstream, socket, options, forward, Some(transform)),
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    let _ = http::error(socket, 422, &error);
                    Outcome {
                        status: 422,
                        bytes: 0,
                        rejected: None,
                    }
                }
            }
        } else {
            execute(upstream, socket, options, forward, None)
        };
        // Observers see each real upstream attempt before retry selection. The
        // router can cool the refused member without re-running classification.
        if let Err(error) = plugins.observe(
            outcome.status as u32,
            attempt_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            request.body.len() as u64,
            outcome.bytes as u64,
        ) {
            eprintln!("plugin observer: {error}");
            if outcome.rejected.is_some() {
                let _ = http::error(socket, 503, "routing observer failed; request held back");
            }
            return;
        }
        let Some(rejected) = outcome.rejected.take() else {
            return;
        };
        if STOPPING.load(Ordering::Relaxed) || Instant::now() >= deadline {
            send_rejection(socket, plugins, rejected, config.max_response);
            return;
        }
        let next = match plugins.retry(
            protocol_id(source),
            outcome.status as u32,
            &selected,
            input,
            config.max_body,
        ) {
            Ok(Some(next)) => next,
            Ok(None) => {
                send_rejection(socket, plugins, rejected, config.max_response);
                return;
            }
            Err(error) => {
                let _ = http::error(socket, 503, &error);
                return;
            }
        };
        let Some(next_route) = config.route(&next.model) else {
            let _ = http::error(socket, 503, "retry plugin selected an unknown model");
            return;
        };
        let (next_protocol, _, _) = config.providers[next_route.provider].for_protocol(source);
        if attempted.iter().any(|(provider, protocol, model)| {
            *provider == next_route.provider
                && *protocol == next_protocol
                && *model == next_route.model
        }) || Instant::now() >= deadline
        {
            send_rejection(socket, plugins, rejected, config.max_response);
            return;
        }
        if let Some(body) = next.body {
            match protocol::inspect(source, &body, path_model.as_deref()) {
                Ok(next_inspection) if (next_inspection.stream || path_stream) == stream => {}
                _ => {
                    let _ = http::error(
                        socket,
                        503,
                        "retry plugin returned an invalid request or changed stream mode",
                    );
                    return;
                }
            }
            retry_body = Some(body);
        }
        selected = next.model;
    }
}

fn request_context(
    request: &Request,
    protocol: Protocol,
    inspection: &protocol::Inspection,
    path: &str,
    stream: bool,
    trusted_workspace: Option<&str>,
    peer: &str,
) -> Result<String, String> {
    let first = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| request.header(name))
            .unwrap_or("")
    };
    let agent = first(&["x-agent-name", "user-agent"]);
    let session = first(&[
        "session-id",
        "session_id",
        "x-session-id",
        "x-codex-session-id",
    ]);
    let cwd = first(&["x-opencode-directory", "x-cwd"]);
    for (name, value, maximum) in [
        ("agent", agent, 512),
        ("session", session, 1024),
        ("cwd", cwd, 4096),
    ] {
        if value.len() > maximum {
            return Err(format!("{name} context exceeds {maximum} bytes"));
        }
    }
    let subagent = !first(&["x-claude-code-agent-id"]).trim().is_empty()
        || first(&["x-openai-subagent"]).trim() == "collab_spawn";
    let compact = first(&["x-openai-subagent"]).trim() == "compact"
        || !matches!(
            first(&["x-deepseek-harness-compact"])
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "" | "0" | "false" | "no" | "off"
        );
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let context = format!(
        "{{\"protocol\":{},\"model\":{},\"stream\":{stream},\"path\":{},\"agent\":{},\"session\":{},\"cwd\":{},\"origin\":\"local\",\"kind\":\"generation\",\"phase\":\"request\",\"routing_agent\":{},\"subagent\":{subagent},\"compact\":{compact},\"at\":{at},\"trusted_workspace\":{},\"peer\":{peer}}}",
        http::quote(protocol::protocol_name(protocol)),
        http::quote(&inspection.model),
        http::quote(path),
        http::quote(agent),
        http::quote(session),
        http::quote(cwd),
        http::quote(&routing_agent(agent)),
        http::quote(trusted_workspace.unwrap_or(""))
    );
    if context.len() > 16 * 1024 {
        return Err("request context exceeds 16 KiB".into());
    }
    Ok(context)
}

fn routing_agent(value: &str) -> String {
    let product = value
        .trim()
        .split(['/', ' '])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if product.starts_with("codex") {
        "codex".into()
    } else if product.starts_with("claude-code") || product == "claude-cli" {
        "claude".into()
    } else if product.starts_with("opencode") {
        "opencode".into()
    } else {
        product
    }
}

fn caller_workspace<'a>(request: &Request, config: &'a Config, socket: &Stream) -> Option<&'a str> {
    let supplied = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| request.header("x-api-key"))
        .or_else(|| request.header("x-goog-api-key"))?;
    config
        .caller_keys
        .as_ref()?
        .workspace(supplied, socket.is_loopback_peer())
}

fn request_plugin_status(error: &str) -> u16 {
    if [
        "privacy filter unavailable; request held back",
        "community plugin unavailable; request held back",
        "all group members unavailable; request held back",
        "routing control or observer failed; request held back",
    ]
    .iter()
    .any(|message| error.contains(message))
    {
        503
    } else if error.contains("request blocked by privacy policy") {
        400
    } else {
        403
    }
}

fn authenticated(request: &Request, config: &Config, socket: &Stream) -> bool {
    let loopback = socket.is_loopback_peer();
    if config.trust_loopback && loopback {
        return true;
    }
    if config.token.is_none() && config.caller_keys.is_none() {
        return true;
    }
    let supplied = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| request.header("x-api-key"))
        .or_else(|| request.header("x-goog-api-key"));
    supplied.is_some_and(|value| {
        let mut accepted = config
            .token
            .as_ref()
            .is_some_and(|token| http::constant_eq(value.as_bytes(), token.as_bytes()));
        if let Some(keys) = &config.caller_keys {
            accepted |= keys.permits(value, loopback);
        }
        accepted
    })
}

fn loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        let Some((name, tail)) = rest.split_once(']') else {
            return false;
        };
        if !tail.is_empty()
            && !tail
                .strip_prefix(':')
                .is_some_and(|port| port.parse::<u16>().is_ok())
        {
            return false;
        }
        name
    } else {
        match host.split_once(':') {
            Some((name, port)) if port.parse::<u16>().is_ok() => name,
            Some(_) => return false,
            None => host,
        }
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn upstream_headers(
    provider: &Provider,
    request: &Request,
    stream: bool,
    protocol: Protocol,
    auth: Auth,
) -> Vec<String> {
    let mut headers = vec![
        "Content-Type: application/json".into(),
        "Expect:".into(),
        format!(
            "Accept: {}",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
        ),
    ];
    if let Some(key) = &provider.key {
        match auth {
            Auth::Bearer => headers.push(format!("Authorization: Bearer {key}")),
            Auth::ApiKey => headers.push(format!("x-api-key: {key}")),
            Auth::GoogleKey => headers.push(format!("x-goog-api-key: {key}")),
            Auth::None => {}
        }
    }
    let configured = |name: &str| provider.headers.iter().any(|(key, _)| key == name);
    if protocol == Protocol::AnthropicMessages {
        if !configured("anthropic-version") {
            headers.push(format!(
                "anthropic-version: {}",
                request.header("anthropic-version").unwrap_or("2023-06-01")
            ));
        }
        if !configured("anthropic-beta")
            && let Some(beta) = request.header("anthropic-beta")
        {
            headers.push(format!("anthropic-beta: {beta}"));
        }
    }
    if matches!(protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses)
        && !configured("openai-beta")
        && let Some(beta) = request.header("openai-beta")
    {
        headers.push(format!("openai-beta: {beta}"));
    }
    headers.extend(
        provider
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}")),
    );
    headers
}

pub fn models_json(config: &Config) -> &str {
    &config.models_json
}

fn ingress(path: &str) -> Result<(Protocol, Option<String>, bool), String> {
    let protocol = match path {
        "/v1/chat/completions" | "/chat/completions" => Some(Protocol::OpenAiChat),
        // Codex's built-in ChatGPT provider keeps this prefix when its authority
        // points to the local gateway. Run it through the same Responses pipeline.
        "/v1/responses" | "/responses" | "/backend-api/codex/responses" => {
            Some(Protocol::OpenAiResponses)
        }
        "/v1/messages" | "/messages" => Some(Protocol::AnthropicMessages),
        "/v1/completions" | "/completions" => Some(Protocol::OpenAiCompletions),
        "/v1beta/interactions" | "/v1/interactions" | "/interactions" => {
            Some(Protocol::GeminiInteractions)
        }
        _ => None,
    };
    if let Some(protocol) = protocol {
        return Ok((protocol, None, false));
    }
    let tail = path
        .strip_prefix("/v1beta/models/")
        .or_else(|| path.strip_prefix("/v1/models/"));
    if let Some(tail) = tail {
        let (model, stream) = if let Some(model) = tail.strip_suffix(":streamGenerateContent") {
            (model, true)
        } else if let Some(model) = tail.strip_suffix(":generateContent") {
            (model, false)
        } else {
            return Err("unknown Gemini endpoint".into());
        };
        let bytes = model.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                let value = bytes
                    .get(index + 1..index + 3)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    .ok_or("invalid model URL escape")?;
                decoded.push(value);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        let model = String::from_utf8(decoded).map_err(|_| "model must be UTF-8")?;
        if model.is_empty() || model.chars().any(char::is_control) {
            return Err("invalid model".into());
        }
        return Ok((Protocol::GeminiGenerateContent, Some(model), stream));
    }
    Err("unknown gateway endpoint".into())
}

fn protocol_id(protocol: Protocol) -> u32 {
    use zcalld_plugin_api::protocol as ids;
    match protocol {
        Protocol::OpenAiChat => ids::OPENAI_CHAT,
        Protocol::OpenAiResponses => ids::OPENAI_RESPONSES,
        Protocol::AnthropicMessages => ids::ANTHROPIC_MESSAGES,
        Protocol::GeminiGenerateContent => ids::GEMINI_GENERATE_CONTENT,
        Protocol::OpenAiCompletions => ids::OPENAI_COMPLETIONS,
        Protocol::GeminiInteractions => ids::GEMINI_INTERACTIONS,
    }
}

struct Outcome {
    status: u16,
    bytes: usize,
    rejected: Option<RejectedResponse>,
}
struct RejectedResponse {
    status: u16,
    content_type: String,
    extra: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Upstream<'a> {
    client: &'a mut Client,
    plugins: &'a mut Plugins,
    provider: &'a Provider,
    model: &'a str,
}

fn provider_envelope(
    provider: &Provider,
    model: &str,
    options: &curl::Options<'_>,
) -> Result<String, String> {
    let body = std::str::from_utf8(options.body).map_err(|_| "provider body is not UTF-8")?;
    let headers = options
        .headers
        .iter()
        .filter_map(|header| {
            let (name, value) = header.split_once(':')?;
            let value = value.trim();
            // CURLOPT's empty Expect entry disables 100-continue; it is not an HTTP header.
            if value.is_empty() {
                return None;
            }
            Some(format!("{}:{}", http::quote(name), http::quote(value)))
        })
        .collect::<Vec<_>>()
        .join(",");
    let optional = |value: Option<&str>| value.map(http::quote).unwrap_or_else(|| "null".into());
    Ok(format!(
        "{{\"provider\":{},\"account\":{},\"model\":{},\"npm\":{},\"url\":{},\"method\":\"POST\",\"headers\":{{{headers}}},\"body\":{},\"window\":524288,\"timeout_ms\":{},\"connect_timeout_ms\":{}}}",
        http::quote(
            provider
                .plugin
                .as_deref()
                .ok_or("provider plugin ID is missing")?
        ),
        optional(provider.plugin_account.as_deref()),
        http::quote(model),
        optional(provider.plugin_npm.as_deref()),
        http::quote(options.url),
        http::quote(body),
        options.timeout_ms,
        options.connect_timeout_ms,
    ))
}

fn plugin_perform(
    response: &mut ProviderResponse,
    sink: &mut dyn Sink,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let head = loop {
        if STOPPING.load(Ordering::Relaxed) {
            return Err("gateway is stopping".into());
        }
        if Instant::now() >= deadline {
            return Err("plugin provider request timed out".into());
        }
        if let Some(head) = response.poll_head()? {
            break head;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    sink.header(format!("HTTP/1.1 {} {}\r\n", head.status, http::reason(head.status)).as_bytes())?;
    for (name, value) in head.headers {
        sink.header(format!("{name}: {value}\r\n").as_bytes())?;
    }
    sink.header(b"\r\n")?;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        if STOPPING.load(Ordering::Relaxed) {
            return Err("gateway is stopping".into());
        }
        if Instant::now() >= deadline {
            return Err("plugin provider request timed out".into());
        }
        match response.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => sink.body(&buffer[..count])?,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(format!("plugin provider stream: {error}")),
        }
    }
}

/// Restore response bytes after zcall conversion, with one bounded output allocation
/// only when a plugin supplies a transform. Empty output can hold a split placeholder.
struct ResponseWriter<'a, 's> {
    writer: &'a mut Chunked<'s>,
    plugins: &'a mut Plugins,
}

impl Write for ResponseWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let maximum = self.writer.maximum.saturating_sub(self.writer.bytes);
        match self
            .plugins
            .response_write(bytes, false, maximum)
            .map_err(std::io::Error::other)?
        {
            Some(output) => self.writer.write_all(&output)?,
            None => self.writer.write_all(bytes)?,
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

struct ForwardOptions<'a> {
    source: Protocol,
    target: Protocol,
    model: &'a str,
    stream: bool,
    maximum: usize,
    defer_rejections: bool,
}
struct Forward<'s, 't, 'f, 'p> {
    writer: Chunked<'s>,
    plugins: &'p mut Plugins,
    response_begun: bool,
    transform: Option<&'t mut protocol::StreamTransform<'f>>,
    buffered: Vec<u8>,
    convert: bool,
    header_bytes: usize,
    ready: bool,
    defer_rejections: bool,
    deferred_rejection: bool,
}
impl Forward<'_, '_, '_, '_> {
    fn begin_response(&mut self) -> Result<(), String> {
        let content_type = if self.convert
            && (200..300).contains(&self.writer.status)
            && self.writer.status != 204
        {
            "application/json"
        } else {
            &self.writer.content_type
        };
        self.plugins
            .response_begin(self.writer.status as u32, content_type)?;
        self.response_begun = true;
        Ok(())
    }
}
// Codex discovers per-model limits through named header families. Forward only
// the documented window/name fields; credits remain shared, fixed-name fields.
fn response_header_allowed(name: &str) -> bool {
    if [
        "retry-after",
        "request-id",
        "x-request-id",
        "x-codex-credits-has-credits",
        "x-codex-credits-unlimited",
        "x-codex-credits-balance",
        "x-codex-rate-limit-reached-type",
    ]
    .iter()
    .any(|expected| name.eq_ignore_ascii_case(expected))
    {
        return true;
    }
    let Some(prefix) = name.get(..7) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case("x-codex") {
        return false;
    }
    let rest = &name[7..];
    for suffix in [
        "-primary-used-percent",
        "-primary-window-minutes",
        "-primary-reset-at",
        "-secondary-used-percent",
        "-secondary-window-minutes",
        "-secondary-reset-at",
        "-limit-name",
    ] {
        let Some(split) = rest.len().checked_sub(suffix.len()) else {
            continue;
        };
        let Some((family, field)) = rest.split_at_checked(split) else {
            continue;
        };
        if field.eq_ignore_ascii_case(suffix) {
            return family.is_empty()
                || family.strip_prefix('-').is_some_and(|family| {
                    family.split('-').all(|part| {
                        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
                    })
                });
        }
    }
    false
}

impl Sink for Forward<'_, '_, '_, '_> {
    fn header(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.header_bytes += bytes.len();
        if self.header_bytes > http::HEADER_LIMIT {
            return Err("upstream headers exceed 16 KiB".into());
        }
        let line = std::str::from_utf8(bytes)
            .map_err(|_| "invalid upstream header encoding")?
            .trim_end_matches(['\r', '\n']);
        if line.starts_with("HTTP/") {
            if self.writer.started || self.response_begun {
                return Err("unexpected upstream response block".into());
            }
            self.writer.status = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .filter(|status: &u16| (100..=599).contains(status))
                .ok_or("invalid upstream status")?;
            self.writer.content_type = "application/json".into();
            self.writer.extra.clear();
            self.ready = false;
            self.deferred_rejection = false;
        } else if line.is_empty() {
            self.ready = self.writer.status >= 200;
            self.deferred_rejection =
                self.ready && self.defer_rejections && matches!(self.writer.status, 429 | 503);
            if self.ready && !self.deferred_rejection {
                self.begin_response()?;
            }
        } else {
            let (key, value) = line.split_once(':').ok_or("malformed upstream header")?;
            let value = value.trim();
            if value.bytes().any(|b| b.is_ascii_control()) {
                return Err("invalid upstream header value".into());
            }
            if key.eq_ignore_ascii_case("content-type") {
                self.writer.content_type = value.to_string();
            }
            if response_header_allowed(key) {
                self.writer.extra.push((key.to_string(), value.to_string()));
            }
        }
        Ok(())
    }
    fn body(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.ready {
            return Err("upstream body arrived before final headers".into());
        }
        if self.deferred_rejection {
            let maximum = RETRY_BODY_LIMIT.min(self.writer.maximum);
            if bytes.len() <= maximum.saturating_sub(self.buffered.len()) {
                self.buffered.extend_from_slice(bytes);
                return Ok(());
            }
            // Large rejection bodies retain their original streaming behavior;
            // bounded failover is abandoned before writing any of their bytes.
            self.deferred_rejection = false;
            self.begin_response()?;
            let buffered = std::mem::take(&mut self.buffered);
            ResponseWriter {
                writer: &mut self.writer,
                plugins: self.plugins,
            }
            .write_all(&buffered)
            .map_err(|e| e.to_string())?;
        }
        if self.convert && (200..300).contains(&self.writer.status) && self.writer.status != 204 {
            if bytes.len() > self.writer.maximum.saturating_sub(self.buffered.len()) {
                return Err("upstream response exceeds max_response_bytes".into());
            }
            self.buffered.extend_from_slice(bytes);
            return Ok(());
        }
        if (200..300).contains(&self.writer.status)
            && self.writer.status != 204
            && let Some(transform) = &mut self.transform
        {
            if !self.writer.content_type.starts_with("text/event-stream") {
                return Err("upstream did not return an SSE stream".into());
            }
            let mut output = ResponseWriter {
                writer: &mut self.writer,
                plugins: self.plugins,
            };
            return transform(bytes, false, &mut output);
        }
        ResponseWriter {
            writer: &mut self.writer,
            plugins: self.plugins,
        }
        .write_all(bytes)
        .map_err(|e| e.to_string())
    }
}

fn execute(
    upstream: Upstream<'_>,
    socket: &mut Stream,
    options: curl::Options<'_>,
    forward: ForwardOptions<'_>,
    transform: Option<&mut protocol::StreamTransform<'_>>,
) -> Outcome {
    let Upstream {
        client,
        plugins,
        provider,
        model,
    } = upstream;
    let mut community = if provider.plugin.is_some() {
        match provider_envelope(provider, model, &options)
            .and_then(|request| plugins.provider_fetch(request.as_bytes()))
        {
            Ok(response) => Some(response),
            Err(error) => {
                let _ = http::error(socket, 502, &error);
                return Outcome {
                    status: 502,
                    bytes: 0,
                    rejected: None,
                };
            }
        }
    } else {
        None
    };
    let mut sink = Forward {
        writer: Chunked {
            socket,
            started: false,
            status: 502,
            content_type: "application/json".into(),
            extra: Vec::new(),
            bytes: 0,
            maximum: forward.maximum,
        },
        plugins,
        response_begun: false,
        transform,
        buffered: Vec::new(),
        convert: !forward.stream && forward.source != forward.target,
        header_bytes: 0,
        ready: false,
        defer_rejections: forward.defer_rejections,
        deferred_rejection: false,
    };
    let result = match &mut community {
        Some(response) => plugin_perform(
            response,
            &mut sink,
            Duration::from_millis(options.timeout_ms),
        ),
        None => client.perform(options, &mut sink),
    };
    // Only a complete, explicitly refused response is eligible. A timeout,
    // disconnect, 200 response or already forwarded body is never replayed.
    if result.is_ok() && sink.deferred_rejection {
        return Outcome {
            status: sink.writer.status,
            bytes: sink.buffered.len(),
            rejected: Some(RejectedResponse {
                status: sink.writer.status,
                content_type: std::mem::take(&mut sink.writer.content_type),
                extra: std::mem::take(&mut sink.writer.extra),
                body: std::mem::take(&mut sink.buffered),
            }),
        };
    }
    let result = result.and_then(|()| {
        if !sink.ready {
            return Err("upstream sent no final response".into());
        }
        if sink.convert && (200..300).contains(&sink.writer.status) && sink.writer.status != 204 {
            let body = protocol::convert_response(
                forward.source,
                forward.target,
                &sink.buffered,
                forward.model,
            )?;
            if body.len() > forward.maximum {
                return Err("converted response exceeds max_response_bytes".into());
            }
            sink.writer.content_type = "application/json".into();
            ResponseWriter {
                writer: &mut sink.writer,
                plugins: sink.plugins,
            }
            .write_all(&body)
            .map_err(|e| e.to_string())?;
        } else if (200..300).contains(&sink.writer.status)
            && sink.writer.status != 204
            && let Some(transform) = &mut sink.transform
        {
            let mut output = ResponseWriter {
                writer: &mut sink.writer,
                plugins: sink.plugins,
            };
            transform(&[], true, &mut output)?;
        }
        if let Some(bytes) = sink.plugins.response_write(
            &[],
            true,
            sink.writer.maximum.saturating_sub(sink.writer.bytes),
        )? {
            sink.writer.write_all(&bytes).map_err(|e| e.to_string())?;
        }
        sink.writer.finish().map_err(|e| e.to_string())
    });
    match result {
        Ok(()) => Outcome {
            status: sink.writer.status,
            bytes: sink.writer.bytes,
            rejected: None,
        },
        Err(error) => {
            if !sink.writer.started {
                let _ = http::error(sink.writer.socket, 502, &error);
            }
            // An interrupted chunked response has no final zero chunk: clients
            // observe truncation. An uncertain generation is never replayed.
            Outcome {
                status: 502,
                bytes: sink.writer.bytes,
                rejected: None,
            }
        }
    }
}

fn send_rejection(
    socket: &mut Stream,
    plugins: &mut Plugins,
    rejected: RejectedResponse,
    maximum: usize,
) {
    let mut writer = Chunked {
        socket,
        started: false,
        status: rejected.status,
        content_type: rejected.content_type,
        extra: rejected.extra,
        bytes: 0,
        maximum,
    };
    let result = (|| -> Result<(), String> {
        plugins.response_begin(writer.status as u32, &writer.content_type)?;
        ResponseWriter {
            writer: &mut writer,
            plugins,
        }
        .write_all(&rejected.body)
        .map_err(|e| e.to_string())?;
        if let Some(bytes) =
            plugins.response_write(&[], true, maximum.saturating_sub(writer.bytes))?
        {
            writer.write_all(&bytes).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())
    })();
    if let Err(error) = result
        && !writer.started
    {
        let _ = http::error(writer.socket, 502, &error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downstream() -> (Stream, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut reader = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (writer, _) = listener.accept().unwrap();
        let output = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        });
        (Stream::Tcp(writer), output)
    }

    fn rejection_forward<'a>(
        socket: &'a mut Stream,
        plugins: &'a mut Plugins,
    ) -> Forward<'a, 'a, 'a, 'a> {
        Forward {
            writer: Chunked {
                socket,
                started: false,
                status: 502,
                content_type: "application/json".into(),
                extra: Vec::new(),
                bytes: 0,
                maximum: RETRY_BODY_LIMIT * 2,
            },
            plugins,
            response_begun: false,
            transform: None,
            buffered: Vec::new(),
            convert: false,
            header_bytes: 0,
            ready: false,
            defer_rejections: true,
            deferred_rejection: false,
        }
    }

    #[test]
    fn only_explicit_refusals_hold_back_downstream_headers() {
        for status in [200, 401, 429, 500, 502, 503, 504] {
            let (mut socket, output) = downstream();
            let mut plugins = Plugins::load(&[]).unwrap();
            let deferred = matches!(status, 429 | 503);
            {
                let mut sink = rejection_forward(&mut socket, &mut plugins);
                sink.header(format!("HTTP/1.1 {status} Fixture\r\n").as_bytes())
                    .unwrap();
                sink.header(b"Content-Type: application/json\r\n").unwrap();
                sink.header(b"\r\n").unwrap();
                sink.body(b"{\"fixture\":true}").unwrap();
                assert_eq!(sink.deferred_rejection, deferred);
                assert_eq!(sink.writer.started, !deferred);
                assert_eq!(sink.response_begun, !deferred);
                if !deferred {
                    sink.writer.finish().unwrap();
                }
            }
            drop(socket);
            let bytes = output.join().unwrap();
            assert_eq!(bytes.is_empty(), deferred);
        }
    }

    #[test]
    fn oversized_refusal_streams_original_response_without_retry() {
        let (mut socket, output) = downstream();
        let mut plugins = Plugins::load(&[]).unwrap();
        {
            let mut sink = rejection_forward(&mut socket, &mut plugins);
            sink.header(b"HTTP/1.1 503 Service Unavailable\r\n")
                .unwrap();
            sink.header(b"Retry-After: 9\r\n").unwrap();
            sink.header(b"\r\n").unwrap();
            sink.body(b"prefix").unwrap();
            sink.body(&vec![b'x'; RETRY_BODY_LIMIT]).unwrap();
            assert!(!sink.deferred_rejection);
            assert!(sink.writer.started);
            assert_eq!(sink.writer.bytes, RETRY_BODY_LIMIT + 6);
            sink.writer.finish().unwrap();
        }
        drop(socket);
        let bytes = output.join().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 503"));
        assert!(text.contains("Retry-After: 9\r\n"));
        assert!(text.contains("6\r\nprefix\r\n"));
        assert!(text.ends_with("0\r\n\r\n"));
    }

    #[test]
    fn context_contains_only_whitelisted_client_metadata() {
        let request = Request {
            method: "POST".into(),
            path: "/v1/responses?private=query".into(),
            body: b"{}".to_vec(),
            headers: vec![
                ("authorization".into(), "Bearer local-secret".into()),
                ("cookie".into(), "private-cookie".into()),
                ("session-id".into(), "session-123".into()),
                ("x-cwd".into(), "/project".into()),
                ("user-agent".into(), "fixture".into()),
            ],
        };
        let inspection = protocol::Inspection {
            model: "public-model".into(),
            stream: true,
        };
        let text = request_context(
            &request,
            Protocol::OpenAiResponses,
            &inspection,
            "/v1/responses",
            true,
            None,
            "{}",
        )
        .unwrap();
        let value = crate::state_json::parse(text.as_bytes()).unwrap();
        assert_eq!(value.get("protocol").as_str(), Some("responses"));
        assert_eq!(value.get("session").as_str(), Some("session-123"));
        assert_eq!(value.get("cwd").as_str(), Some("/project"));
        assert_eq!(value.get("trusted_workspace").as_str(), Some(""));
        assert!(value.get("peer").as_object().unwrap().is_empty());
        assert!(
            !text.contains("local-secret")
                && !text.contains("private-cookie")
                && !text.contains("private=query")
        );
        assert!(matches!(
            value.get("request"),
            crate::state_json::Value::Null
        ));
    }
    #[test]
    fn upstream_headers_preserve_configured_account_without_local_authorization() {
        let text = "[provider upstream]\nbase_url=http://127.0.0.1:1/v1\nprotocol=responses\nheader.ChatGPT-Account-Id=account\nheader.OpenAI-Beta=configured-beta\n[route test]\nprovider=upstream\nmodel=upstream-model\n";
        let config = Config::parse(text, std::path::Path::new("/tmp")).unwrap();
        let mut provider = config.providers[0].clone();
        provider.key = Some("upstream-only".into());
        let request = Request {
            method: "POST".into(),
            path: "/v1/responses".into(),
            body: vec![],
            headers: vec![
                ("authorization".into(), "Bearer local-only".into()),
                ("openai-beta".into(), "client-beta".into()),
            ],
        };
        let headers =
            upstream_headers(&provider, &request, false, provider.protocol, provider.auth);
        assert!(headers.contains(&"Authorization: Bearer upstream-only".into()));
        assert!(headers.contains(&"chatgpt-account-id: account".into()));
        assert!(headers.contains(&"openai-beta: configured-beta".into()));
        assert!(
            !headers
                .iter()
                .any(|value| value.contains("local-only") || value.contains("client-beta"))
        );
        assert_eq!(
            headers
                .iter()
                .filter(|value| value.to_ascii_lowercase().starts_with("openai-beta:"))
                .count(),
            1
        );
    }

    #[test]
    fn gemini_path() {
        let (protocol, model, stream) =
            ingress("/v1beta/models/a%2Fb:streamGenerateContent").unwrap();
        assert_eq!(protocol, Protocol::GeminiGenerateContent);
        assert_eq!(model.as_deref(), Some("a/b"));
        assert!(stream);
        assert!(ingress("/v1beta/models/a%zz:generateContent").is_err());
    }
}
