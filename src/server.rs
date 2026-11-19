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
    let started = Instant::now();
    let request = match http::read_request(
        socket,
        config.max_body,
        Duration::from_millis(config.request_timeout_ms),
    ) {
        Ok(request) => request,
        Err(error) => {
            let _ = http::error(socket, error.status, error.message);
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
    let selected = match plugins.route(&inspect.model) {
        Ok(model) => model,
        Err(error) => {
            let _ = http::error(socket, 403, &error);
            return;
        }
    };
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
    let patched = match plugins.request(protocol_id(source), &request.body, config.max_body) {
        Ok(value) => value,
        Err(error) => {
            let _ = http::error(socket, request_plugin_status(&error), &error);
            return;
        }
    };
    let original = patched.as_deref().unwrap_or(&request.body);
    let stream = if patched.is_some() {
        match protocol::inspect(source, original, path_model.as_deref()) {
            Ok(value) => value.stream || path_stream,
            Err(error) => {
                let _ = http::error(socket, 400, &error);
                return;
            }
        }
    } else {
        inspect.stream || path_stream
    };
    let body = match protocol::prepare_request(
        source,
        target_protocol,
        original,
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
    let headers = upstream_headers(provider, &request, stream, target_protocol, target_auth);
    let options = curl::Options {
        url: &url,
        body: &body,
        headers: &headers,
        timeout_ms: config.upstream_timeout_ms,
        connect_timeout_ms: config.connect_timeout_ms,
    };
    let forward = ForwardOptions {
        source: target_protocol,
        target: source,
        model: &inspect.model,
        stream,
        maximum: config.max_response,
    };
    let upstream = Upstream {
        client,
        plugins,
        provider,
        model: &route.model,
    };
    let outcome = if stream && source != target_protocol {
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
                }
            }
        }
    } else {
        execute(upstream, socket, options, forward, None)
    };
    if let Err(error) = plugins.observe(
        outcome.status as u32,
        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        request.body.len() as u64,
        outcome.bytes as u64,
    ) {
        eprintln!("plugin observer: {error}");
    }
}

fn request_plugin_status(error: &str) -> u16 {
    if error.contains("privacy filter unavailable; request held back") {
        503
    } else if error.contains("request blocked by privacy policy") {
        400
    } else {
        403
    }
}

fn authenticated(request: &Request, config: &Config, socket: &Stream) -> bool {
    if config.trust_loopback && socket.is_loopback_peer() {
        return true;
    }
    let Some(token) = &config.token else {
        return true;
    };
    let supplied = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| request.header("x-api-key"))
        .or_else(|| request.header("x-goog-api-key"));
    supplied.is_some_and(|value| http::constant_eq(value.as_bytes(), token.as_bytes()))
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
        "/v1/responses" | "/responses" => Some(Protocol::OpenAiResponses),
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
        } else if line.is_empty() {
            self.ready = self.writer.status >= 200;
            if self.ready {
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
            if matches!(
                key.to_ascii_lowercase().as_str(),
                "retry-after" | "request-id" | "x-request-id"
            ) {
                self.writer.extra.push((key.to_string(), value.to_string()));
            }
        }
        Ok(())
    }
    fn body(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.ready {
            return Err("upstream body arrived before final headers".into());
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
    };
    let result = match &mut community {
        Some(response) => plugin_perform(
            response,
            &mut sink,
            Duration::from_millis(options.timeout_ms),
        ),
        None => client.perform(options, &mut sink),
    }
    .and_then(|()| {
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
        },
        Err(error) => {
            if !sink.writer.started {
                let _ = http::error(sink.writer.socket, 502, &error);
            }
            // An interrupted chunked response has no final zero chunk: clients
            // observe truncation. Generation POSTs are never replayed.
            Outcome {
                status: 502,
                bytes: sink.writer.bytes,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
