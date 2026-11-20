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
    object([
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
    ])
    .stringify()
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
            | "www-authenticate" => {
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
        let text = metadata(&request, "/mcp/example", &config, true, Some("verified-id"));
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
