//! Use the existing local gateway ingress; never call providers or model tools directly.
use super::*;
use crate::curl::{Client, Options, Sink};
use crate::transport::client_address;

const MAX_RESPONSE: usize = 256 * 1024;
const MAX_TEXT: usize = 16 * 1024;

pub(super) struct Gateway {
    url: String,
    socket: Option<PathBuf>,
    model: String,
    headers: Vec<String>,
    timeout_ms: u64,
    connect_timeout_ms: u64,
    max_body: usize,
    max_response: usize,
}

impl Gateway {
    pub(super) fn new(
        config: &Config,
        model: &str,
        token_env: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        if !config
            .routes
            .iter()
            .any(|route| route.name == model && route.name != "*")
        {
            return Err(
                "listener requires an exact configured public route; use zcalld models".into(),
            );
        }
        let socket = match config.unix_socket.as_ref() {
            Some(path) => match fs::symlink_metadata(path) {
                Ok(_) => {
                    private_socket(path)?;
                    Some(path.clone())
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return Err("configured gateway socket is unavailable".into()),
            },
            None => None,
        };
        let url = if socket.is_some() {
            "http://localhost/v1/chat/completions".into()
        } else {
            let address = config
                .listen
                .map(client_address)
                .ok_or("local gateway has no available socket or TCP listener")?;
            if !address.ip().is_loopback() {
                return Err("native SSMCP listener only connects to a local gateway".into());
            }
            format!("http://{address}/v1/chat/completions")
        };
        let token = match token_env {
            Some(name) => Some(
                std::env::var(name)
                    .map_err(|_| "caller token environment variable is unavailable")?,
            ),
            None => config.token.clone(),
        };
        if token.as_ref().is_some_and(|value| {
            value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control)
        }) {
            return Err("local gateway caller token is invalid".into());
        }
        let mut headers = vec![
            "Content-Type: application/json".into(),
            "Accept: application/json".into(),
            "Expect:".into(),
            "X-Agent-Name: zcalld-ssmcp".into(),
        ];
        if let Some(token) = token {
            headers.push(format!("Authorization: Bearer {token}"));
        }
        Ok(Self {
            url,
            socket,
            model: model.into(),
            headers,
            timeout_ms: timeout_ms
                .unwrap_or(config.upstream_timeout_ms)
                .clamp(1000, 900_000),
            connect_timeout_ms: config.connect_timeout_ms.min(5000),
            max_body: config.max_body,
            max_response: config.max_response.min(MAX_RESPONSE),
        })
    }

    pub(super) fn generate(
        &self,
        text: &str,
        conversation: &str,
        delivery: &str,
    ) -> Result<String, String> {
        let body = request_body(&self.model, text);
        if body.len() > self.max_body {
            return Err("prepared input exceeds the configured gateway request limit".into());
        }
        let mut headers = self.headers.clone();
        // Existing routing metadata, not a claim that the gateway persists this conversation.
        headers.push(format!("X-Session-Id: {conversation}"));
        headers.push(format!("X-Request-Id: {delivery}"));
        let mut response = Response {
            status: 0,
            header_bytes: 0,
            body: Vec::new(),
            maximum: self.max_response,
        };
        Client::new()?.perform_local(
            Options {
                url: &self.url,
                body: body.as_bytes(),
                headers: &headers,
                timeout_ms: self.timeout_ms,
                connect_timeout_ms: self.connect_timeout_ms,
            },
            self.socket.as_deref(),
            &mut response,
        )?;
        if !(200..300).contains(&response.status) {
            // Provider error bodies and credentials must never be copied into the UI.
            return Err(format!("local gateway returned HTTP {}", response.status));
        }
        response_text(&response.body)
    }
}

fn request_body(model: &str, text: &str) -> String {
    object([
        ("model", model.into()),
        ("stream", false.into()),
        (
            "messages",
            Value::Array(vec![object([
                ("role", "user".into()),
                ("content", text.into()),
            ])]),
        ),
    ])
    .stringify()
}

fn response_text(bytes: &[u8]) -> Result<String, String> {
    let value =
        crate::state_json::parse(bytes).map_err(|_| "local gateway returned invalid JSON")?;
    let choice = value
        .get("choices")
        .as_array()
        .first()
        .ok_or("local gateway returned no assistant choice")?;
    let message = choice.get("message");
    if !message.get("tool_calls").as_array().is_empty()
        || choice.get("finish_reason").as_str() == Some("tool_calls")
    {
        return Err("the native listener does not execute model tools".into());
    }
    let text = message
        .get("content")
        .as_str()
        .or_else(|| message.get("refusal").as_str())
        .ok_or("local gateway returned no textual result")?;
    if text.trim().is_empty()
        || text.len() > MAX_TEXT
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err("model result exceeds 16 KiB or contains unsupported controls".into());
    }
    Ok(text.into())
}

struct Response {
    status: u16,
    header_bytes: usize,
    body: Vec<u8>,
    maximum: usize,
}
impl Sink for Response {
    fn is_cancelled(&self) -> bool {
        // This process owns one lease; a failed lease heartbeat sets STOPPING.
        STOPPING.load(Ordering::Relaxed)
    }

    fn header(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.header_bytes = self
            .header_bytes
            .checked_add(bytes.len())
            .ok_or("gateway headers exceed limit")?;
        if self.header_bytes > 16 * 1024 {
            return Err("gateway headers exceed 16 KiB".into());
        }
        if bytes.starts_with(b"HTTP/") {
            let line =
                std::str::from_utf8(bytes).map_err(|_| "invalid local gateway status line")?;
            self.status = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .ok_or("invalid local gateway status")?;
        }
        Ok(())
    }
    fn body(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > self.maximum.saturating_sub(self.body.len()) {
            return Err("local gateway result exceeds the bounded response limit".into());
        }
        self.body.extend_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_listener_request_keeps_exact_user_text_and_does_not_restore_context() {
        let text = "  中文 @literal $skill\n$(literal)  ";
        let request =
            crate::state_json::parse(request_body("exact-route", text).as_bytes()).unwrap();
        assert_eq!(request.get("model").as_str(), Some("exact-route"));
        assert_eq!(request.get("stream").as_bool(), Some(false));
        let messages = request.get("messages").as_array();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("role").as_str(), Some("user"));
        assert_eq!(messages[0].get("content").as_str(), Some(text));
    }

    #[test]
    fn native_listener_model_result_preserves_text_and_rejects_tools_and_controls() {
        let value = object([(
            "choices",
            Value::Array(vec![object([
                ("message", object([("content", "模型结果\n  exact".into())])),
                ("finish_reason", "stop".into()),
            ])]),
        )]);
        assert_eq!(
            response_text(value.stringify().as_bytes()).unwrap(),
            "模型结果\n  exact"
        );
        let tools = br#"{"choices":[{"message":{"content":"no","tool_calls":[{"function":{"name":"delete"}}]},"finish_reason":"tool_calls"}]}"#;
        assert!(
            response_text(tools)
                .unwrap_err()
                .contains("does not execute")
        );
        for content in ["\u{1b}[2J".into(), "界".repeat(MAX_TEXT / 3 + 1), "".into()] {
            let value = object([(
                "choices",
                Value::Array(vec![object([(
                    "message",
                    object([("content", content.into())]),
                )])]),
            )]);
            assert!(response_text(value.stringify().as_bytes()).is_err());
        }
    }

    #[test]
    fn native_listener_response_is_bounded_before_json_parsing() {
        let mut response = Response {
            status: 0,
            header_bytes: 0,
            body: Vec::new(),
            maximum: 4,
        };
        response.header(b"HTTP/1.1 200 OK\r\n").unwrap();
        response.body(b"1234").unwrap();
        assert!(response.body(b"5").is_err());
        assert_eq!(response.body, b"1234");
    }
}
