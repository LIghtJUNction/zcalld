//! Bounded transport for optional HTTP plugins; application protocols stay in plugins.
use crate::{
    config::Config,
    http::{self, Request},
    plugins::{Plugins, ProviderHead, ProviderResponse},
    state_json::{Value, object},
    system,
    transport::Stream,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    time::{Duration, Instant},
};

fn metadata(
    request: &Request,
    path: &str,
    config: &Config,
    loopback: bool,
    caller: Option<&str>,
    route_context: bool,
) -> String {
    let mut headers = BTreeMap::new();
    for name in [
        "accept",
        "content-type",
        "mcp-protocol-version",
        "mcp-session-id",
    ] {
        if let Some(value) = request.header(name) {
            headers.insert(name.to_string(), Value::from(value));
        }
    }
    let mut result = object([
        ("method", request.method.clone().into()),
        ("path", path.into()),
        ("headers", Value::Object(headers)),
        ("loopback", loopback.into()),
        (
            "caller_key_id",
            caller.map(Value::from).unwrap_or(Value::Null),
        ),
        ("timeout_ms", config.upstream_timeout_ms.into()),
        ("max_response_bytes", config.max_response.into()),
    ]);
    if route_context {
        // The peer and route relationships come from the authenticated connection
        // and the loaded configuration. Never copy a client-supplied identity.
        let account_hints = request
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
            .map(|(_, value)| Value::from(value.clone()))
            .collect();
        // This is a consistency hint only, never authenticated identity. Preserve
        // multiplicity so the owning plugin can reject ambiguous/empty hints.
        result.set(
            "untrusted_headers",
            object([("chatgpt-account-id", Value::Array(account_hints))]),
        );
        result.set(
            "query",
            request
                .path
                .split_once('?')
                .map(|(_, q)| q)
                .unwrap_or("")
                .into(),
        );
        let providers = config
            .providers
            .iter()
            .map(|provider| {
                object([
                    ("id", provider.name.clone().into()),
                    (
                        "plugin",
                        provider
                            .plugin
                            .clone()
                            .map(Value::from)
                            .unwrap_or(Value::Null),
                    ),
                    (
                        "account",
                        provider
                            .plugin_account
                            .clone()
                            .map(Value::from)
                            .unwrap_or(Value::Null),
                    ),
                ])
            })
            .collect();
        let routes = config
            .routes
            .iter()
            .map(|route| {
                let dynamic = config.dynamic_routes.contains(&route.name);
                object([
                    ("id", route.name.clone().into()),
                    ("dynamic", dynamic.into()),
                    (
                        "provider",
                        if dynamic {
                            Value::Null
                        } else {
                            config.providers[route.provider].name.clone().into()
                        },
                    ),
                    (
                        "model",
                        if dynamic {
                            Value::Null
                        } else {
                            route.model.clone().into()
                        },
                    ),
                ])
            })
            .collect();
        result.set(
            "routing",
            object([
                ("providers", Value::Array(providers)),
                ("routes", Value::Array(routes)),
            ]),
        );
    }
    result.stringify()
}

fn response_headers(head: ProviderHead) -> (u16, String, Vec<(String, String)>) {
    let mut content_type = "application/json".to_string();
    let mut extra = Vec::new();
    for (name, value) in head.headers {
        match name.to_ascii_lowercase().as_str() {
            "content-type" => content_type = value,
            "mcp-session-id"
            | "mcp-protocol-version"
            | "retry-after"
            | "allow"
            | "www-authenticate"
            | "etag" => {
                extra.push((name, value));
            }
            // Framing, connection state, cookies and caching remain host-owned.
            _ => {}
        }
    }
    (head.status, content_type, extra)
}

fn error(socket: &mut Stream, status: u16, message: &str, head_only: bool) {
    if head_only {
        let _ = http::respond(socket, status, "application/json", &[]);
    } else {
        let _ = http::error(socket, status, message);
    }
}

fn interrupted(deadline: Instant) -> bool {
    system::STOPPING.load(std::sync::atomic::Ordering::Relaxed) || Instant::now() >= deadline
}

fn relay(socket: &mut Stream, config: &Config, mut response: ProviderResponse, head_only: bool) {
    let deadline = Instant::now() + Duration::from_millis(config.upstream_timeout_ms);
    let head = loop {
        if interrupted(deadline) {
            error(
                socket,
                504,
                "HTTP extension request interrupted or timed out",
                head_only,
            );
            return;
        }
        match response.poll_head() {
            Ok(Some(head)) => break head,
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => {
                error(
                    socket,
                    502,
                    "HTTP extension returned invalid response metadata",
                    head_only,
                );
                return;
            }
        }
    };
    let (status, content_type, extra) = response_headers(head);
    let mut writer = http::Chunked {
        socket,
        started: false,
        status,
        content_type,
        extra,
        bytes: 0,
        maximum: config.max_response,
    };
    if head_only {
        // HEAD has the corresponding response headers but no body or chunk trailer.
        // Releasing the resource also cancels work that would produce a body.
        let _ = writer.start();
        return;
    }
    let mut buffer = [0_u8; 16_384];
    loop {
        if interrupted(deadline) {
            if !writer.started {
                let _ = http::error(
                    writer.socket,
                    504,
                    "HTTP extension request interrupted or timed out",
                );
            }
            // Dropping the resource cancels its unfinished operation. A partial
            // response is closed rather than marked as a complete success.
            return;
        }
        match response.read(&mut buffer) {
            Ok(0) => {
                let _ = writer.finish();
                return;
            }
            Ok(count) => {
                if writer.write_all(&buffer[..count]).is_err() {
                    if !writer.started {
                        let _ = http::error(
                            writer.socket,
                            502,
                            "HTTP extension response exceeds the host limit",
                        );
                    }
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => {
                if !writer.started {
                    let _ = http::error(writer.socket, 502, "HTTP extension response failed");
                }
                return;
            }
        }
    }
}

/// Invoke only after the core authenticated the actual connection and rejected
/// forbidden Origin/Host values. Ordinary generation paths allocate no metadata.
pub fn try_handle(
    socket: &mut Stream,
    request: &Request,
    config: &Config,
    plugins: &mut Plugins,
    caller_key_id: Option<&str>,
) -> bool {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let head_only = request.method == "HEAD";
    let handler = match plugins.http_handler(path) {
        Ok(Some(handler)) => handler,
        Ok(None) => return false,
        Err(_) => {
            error(
                socket,
                503,
                "HTTP extension route is unavailable",
                head_only,
            );
            return true;
        }
    };
    let envelope = metadata(
        request,
        path,
        config,
        socket.is_loopback_peer(),
        caller_key_id,
        handler.route_context,
    );
    match plugins.http_fetch(handler, &envelope, &request.body) {
        Ok(response) => relay(socket, config, response, head_only),
        Err(_) => {
            error(socket, 503, "HTTP extension is unavailable", head_only);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_context_comes_from_config_and_never_claims_group_capabilities() {
        let mut config = Config::parse(
            "[server]\nlisten = 127.0.0.1:9415\n[provider fixture]\nbase_url = https://private-host.example/v1\nprotocol = responses\nauth = none\nplugin = builtin:codex\nplugin_account = builtin:codex#bound\n[route alias]\nprovider = fixture\nmodel = upstream-slug\n[route group]\nprovider = fixture\nmodel = guessed-member\n",
            std::path::Path::new("."),
        ).unwrap();
        config.providers[0].key = Some("upstream-secret-canary".into());
        config.providers[0]
            .headers
            .push(("x-private".into(), "header-secret-canary".into()));
        config.dynamic_routes.push("group".into());
        let request = Request {
            method: "GET".into(),
            path: "/backend-api/codex/models?client_version=0.170.0".into(),
            headers: vec![
                ("x-caller-key-id".into(), "forged-caller".into()),
                ("x-loopback".into(), "true".into()),
                ("chatgpt-account-id".into(), "forged-account".into()),
            ],
            body: Vec::new(),
        };
        let text = metadata(
            &request,
            "/backend-api/codex/models",
            &config,
            false,
            Some("verified"),
            true,
        );
        let value = crate::state_json::parse(text.as_bytes()).unwrap();
        assert_eq!(value.get("loopback").as_bool(), Some(false));
        assert_eq!(value.get("caller_key_id").as_str(), Some("verified"));
        assert_eq!(value.get("query").as_str(), Some("client_version=0.170.0"));
        assert_eq!(
            value
                .get("untrusted_headers")
                .get("chatgpt-account-id")
                .as_array()[0]
                .as_str(),
            Some("forged-account")
        );
        let providers = value.get("routing").get("providers").as_array();
        assert_eq!(
            providers[0].get("account").as_str(),
            Some("builtin:codex#bound")
        );
        let routes = value.get("routing").get("routes").as_array();
        assert_eq!(routes[0].get("id").as_str(), Some("alias"));
        assert_eq!(routes[0].get("model").as_str(), Some("upstream-slug"));
        assert_eq!(routes[1].get("dynamic").as_bool(), Some(true));
        assert_eq!(routes[1].get("provider"), &Value::Null);
        assert_eq!(routes[1].get("model"), &Value::Null);
        for forbidden in [
            "private-host",
            "upstream-secret",
            "header-secret",
            "forged-caller",
            "guessed-member",
        ] {
            assert!(!text.contains(forbidden));
        }
    }
    #[test]
    fn extension_metadata_has_only_validated_identity_and_protocol_headers() {
        let config = Config::parse(
            "[server]\nlisten = 127.0.0.1:9415\n[provider fixture]\nbase_url = http://127.0.0.1:1\nprotocol = chat\nauth = none\n[route fixture]\nprovider = fixture\nmodel = fixture\n",
            std::path::Path::new("."),
        )
        .unwrap();
        let request = Request {
            method: "POST".into(),
            path: "/mcp/example?secret=do-not-forward".into(),
            headers: vec![
                ("authorization".into(), "Bearer gateway-private".into()),
                ("cookie".into(), "session=private-cookie".into()),
                ("x-caller-id".into(), "forged".into()),
                ("mcp-protocol-version".into(), "2025-03-26".into()),
                ("content-type".into(), "application/json".into()),
            ],
            body: b"{}".to_vec(),
        };
        let text = metadata(
            &request,
            "/mcp/example",
            &config,
            true,
            Some("verified-id"),
            false,
        );
        let value = crate::state_json::parse(text.as_bytes()).unwrap();
        assert_eq!(value.get("caller_key_id").as_str(), Some("verified-id"));
        assert_eq!(value.get("path").as_str(), Some("/mcp/example"));
        assert_eq!(
            value.get("headers").get("mcp-protocol-version").as_str(),
            Some("2025-03-26")
        );
        for secret in [
            "gateway-private",
            "private-cookie",
            "do-not-forward",
            "forged",
        ] {
            assert!(!text.contains(secret));
        }
    }

    #[test]
    fn extension_cannot_change_host_framing_or_set_browser_cookies() {
        let (status, content_type, headers) = response_headers(ProviderHead {
            status: 202,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Content-Length".into(), "999999".into()),
                ("Transfer-Encoding".into(), "gzip".into()),
                ("Set-Cookie".into(), "secret=x".into()),
                ("Mcp-Session-Id".into(), "fixture".into()),
                ("Allow".into(), "POST".into()),
            ],
        });
        assert_eq!(status, 202);
        assert_eq!(content_type, "application/json");
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, "Mcp-Session-Id");
    }
}
