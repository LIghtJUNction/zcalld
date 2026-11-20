//! Optional, bounded desktop event projection. No prompts or credentials persist.
use crate::{
    config::Provider,
    digest, http,
    http::Request,
    state_json::{Value, object},
};
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use zcalld_plugin_api::events as abi;

static WARNED: AtomicBool = AtomicBool::new(false);
static OBSERVATIONS: AtomicU64 = AtomicU64::new(1);
static SALT: OnceLock<Option<[u8; 32]>> = OnceLock::new();

pub fn diagnostic(operation: &'static str) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("zcalld desktop events unavailable ({operation}); generation continues");
    }
}

/// Resources own their request state independently of a mutable plugin proxy.
#[derive(Default)]
pub struct Leases {
    resources: Vec<zplugin::Resource<'static>>,
}
impl Leases {
    pub fn push(&mut self, resource: zplugin::Resource<'static>) {
        self.resources.push(resource);
    }
    fn call(&mut self, operation: u32, bytes: &[u8]) {
        self.resources.retain_mut(|resource| {
            if matches!(resource.call(operation, bytes, &mut []), Ok(0)) {
                true
            } else {
                diagnostic("request event");
                // Dropping a broken lease releases its activity even on error.
                false
            }
        });
    }
    pub fn head(&mut self, status: u16, protocol: u32, stream: bool, content_type: &str) {
        if self.resources.is_empty() {
            return;
        }
        let metadata = format!(
            "{{\"status\":{status},\"protocol\":{protocol},\"stream\":{stream},\"content_type\":{}}}",
            http::quote(content_type)
        );
        if metadata.len() <= abi::MAX_EVENT_BYTES {
            self.call(abi::HEAD, metadata.as_bytes());
        } else {
            diagnostic("oversized response metadata");
            self.resources.clear();
        }
    }
    pub fn body(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.call(abi::BODY, bytes);
        }
    }
    pub fn finish(&mut self, status: u16, delivered: bool) {
        if !self.resources.is_empty() {
            self.call(
                abi::FINISH,
                format!("{{\"status\":{status},\"delivered\":{delivered}}}").as_bytes(),
            );
        }
    }
}

fn random_salt() -> Option<[u8; 32]> {
    let mut bytes = [0; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .ok()?
            .read_exact(&mut bytes)
            .ok()?;
        Some(bytes)
    }
    #[cfg(windows)]
    {
        crate::state_json::privacy_windows::random(&mut bytes).ok()?;
        Some(bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}
fn opaque(label: &str, value: &[u8], maximum: usize) -> String {
    if value.is_empty() || value.len() > maximum {
        return String::new();
    }
    let Some(salt) = SALT.get_or_init(random_salt) else {
        return String::new();
    };
    let mut bytes = Vec::with_capacity(salt.len() + label.len() + 1 + value.len());
    bytes.extend_from_slice(salt);
    bytes.extend_from_slice(label.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(value);
    digest::sha256(&bytes)
}
fn one_header<'a>(request: &'a Request, name: &str, maximum: usize) -> Option<&'a str> {
    let mut values = request
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let (_, value) = values.next()?;
    if values.next().is_some()
        || value.is_empty()
        || value.len() > maximum
        || value.chars().any(char::is_control)
    {
        None
    } else {
        Some(value)
    }
}
fn agent(request: &Request) -> &'static str {
    let value = one_header(request, "x-agent-name", 512)
        .or_else(|| one_header(request, "user-agent", 512))
        .unwrap_or("");
    let product = value
        .trim()
        .split(['/', ' '])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if product.starts_with("codex") {
        "codex"
    } else if product.starts_with("claude-code") || product == "claude-cli" || product == "claude" {
        "claude"
    } else if product.starts_with("opencode") {
        "opencode"
    } else {
        match product.as_str() {
            "gemini" | "gemini-cli" => "gemini",
            "qwen" | "qwen-code" => "qwen",
            "pi" | "pi-coding-agent" => "pi",
            "aider" => "aider",
            "goose" => "goose",
            "amp" => "amp",
            "kilo" => "kilo",
            "cursor" => "cursor",
            "code" | "vscode" => "vscode",
            _ => "other",
        }
    }
}
fn is(value: Option<zconv::json::Value<'_, '_>>, expected: &str) -> bool {
    value
        .and_then(|v| v.as_string())
        .is_some_and(|s| s.eq_str(expected))
}
fn decoded(value: Option<zconv::json::Value<'_, '_>>, maximum: usize) -> Option<String> {
    let value = value?.as_string()?;
    let length = value.decoded_len();
    if length == 0 || length > maximum {
        return None;
    }
    let mut bytes = vec![0; length];
    value.decode_into(&mut bytes).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}
#[derive(Default)]
struct BodyContext {
    valid: bool,
    compaction: bool,
    title: bool,
    session: String,
    conversation: String,
}
fn body_context(body: &[u8], label: &str) -> BodyContext {
    use zconv::json::{self, Kind, Token};
    const MAX_TOKENS: usize = 32_768;
    let limits = json::Limits {
        max_depth: 64,
        max_input: 8 << 20,
        max_tokens: MAX_TOKENS,
        max_object_fields: 4096,
    };
    let mut tokens = vec![Token::EMPTY; 256];
    loop {
        match json::parse(body, &mut tokens, limits) {
            Ok(document) => {
                let root = document.root();
                if root.kind() != Kind::Object {
                    return BodyContext::default();
                }
                let input = root.get("input");
                let metadata = root.get("metadata");
                let purpose = metadata.and_then(|m| m.get("purpose"));
                let mut result = BodyContext {
                    valid: true,
                    compaction: is(purpose, "compaction")
                        || input.is_some_and(|v| {
                            v.array()
                                .any(|item| is(item.get("type"), "compaction_trigger"))
                        }),
                    title: is(purpose, "title"),
                    ..BodyContext::default()
                };
                if let Some(session) = decoded(metadata.and_then(|v| v.get("session_id")), 1024) {
                    result.session = opaque(label, session.as_bytes(), 1024);
                } else if let Some(user) = decoded(metadata.and_then(|v| v.get("user_id")), 1024)
                    && let Some((_, session)) = user.rsplit_once("_session_")
                    && !session.is_empty()
                    && session.len() <= 128
                    && session
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    // Claude's structured session suffix; the user/account prefix
                    // never enters event metadata.
                    result.session = opaque(label, session.as_bytes(), 1024);
                }
                let first = root
                    .get("messages")
                    .and_then(|v| v.array().find(|item| is(item.get("role"), "user")))
                    .or_else(|| {
                        input.and_then(|v| {
                            if v.kind() == Kind::String {
                                Some(v)
                            } else {
                                v.array().find(|item| is(item.get("role"), "user"))
                            }
                        })
                    })
                    .or_else(|| {
                        root.get("contents")
                            .and_then(|v| v.array().find(|item| is(item.get("role"), "user")))
                    })
                    .or_else(|| root.get("prompt").filter(|v| v.kind() == Kind::String));
                if let Some(first) = first {
                    // A process-private salted digest. Never retain prompt bytes;
                    // oversize or unidentifiable conversations simply do not notify.
                    result.conversation = opaque(label, first.raw(), 64 << 10);
                }
                return result;
            }
            Err(error) => {
                if let zconv::Error::WorkspaceTooSmall { needed } = error
                    && tokens.len() < MAX_TOKENS
                {
                    let next = tokens.len().saturating_mul(2).max(needed);
                    if next <= MAX_TOKENS {
                        tokens.resize(next, Token::EMPTY);
                        continue;
                    }
                }
                return BodyContext::default();
            }
        }
    }
}
/// Called only when an EventPlugin is configured; ordinary core requests pay no
/// extra JSON scan. Failure to classify suppresses completion, never generation.
pub fn request_metadata(request: &Request, protocol: u32) -> String {
    let agent = agent(request);
    let body = body_context(&request.body, agent);
    let mut raw_session = None;
    let mut ambiguous = false;
    for name in [
        "session-id",
        "session_id",
        "x-session-id",
        "x-codex-session-id",
    ] {
        if let Some(value) = one_header(request, name, 1024) {
            if raw_session.is_some_and(|old| old != value) {
                ambiguous = true;
            }
            raw_session = Some(value);
        }
    }
    let session = if ambiguous {
        String::new()
    } else if let Some(raw) = raw_session {
        opaque(agent, raw.as_bytes(), 1024)
    } else {
        body.session
    };
    let purpose = one_header(request, "x-openai-subagent", 128).unwrap_or("");
    let subagent =
        one_header(request, "x-claude-code-agent-id", 512).is_some() || purpose == "collab_spawn";
    let compact_header = one_header(request, "x-deepseek-harness-compact", 32).is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    });
    let kind = if !body.valid || ambiguous {
        "unknown"
    } else if purpose == "compact" || compact_header || body.compaction {
        "compaction"
    } else if purpose == "title" || body.title {
        "title"
    } else {
        "generation"
    };
    object([
        ("version", 1u64.into()),
        ("protocol", u64::from(protocol).into()),
        ("agent", agent.into()),
        ("session", session.into()),
        ("conversation", body.conversation.into()),
        ("kind", kind.into()),
        ("subagent", subagent.into()),
    ])
    .stringify()
}

/// No implicit/default account can be assigned another account's observations.
pub fn codex_source(provider: &Provider) -> Option<String> {
    let account = provider.plugin_account.as_deref()?;
    if provider.plugin.as_deref() != Some("builtin:codex")
        || !(account == "builtin:codex"
            || account
                .strip_prefix("builtin:codex#")
                .is_some_and(|v| !v.is_empty()))
    {
        return None;
    }
    let mut input = b"zcalld:codex-account:v1\0".to_vec();
    input.extend_from_slice(account.as_bytes());
    Some(digest::sha256(&input))
}
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let mut values = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let (_, value) = values.next()?;
    values.next().is_none().then_some(value.as_str())
}
fn numeric(value: Option<&str>) -> Option<f64> {
    let text = value?;
    if text.len() > 64 {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}
/// Only already observed, exact account-wide Codex fields are eligible. Named
/// model families cannot be inferred to be account-wide or tied to a model.
pub fn quota(source: &str, headers: &[(String, String)], now: u64) -> Option<String> {
    let mut windows = Vec::new();
    for id in ["primary", "secondary"] {
        let prefix = format!("x-codex-{id}");
        let Some(used) =
            numeric(header(headers, &format!("{prefix}-used-percent"))).filter(|v| *v <= 100.0)
        else {
            continue;
        };
        let Some(minutes) = header(headers, &format!("{prefix}-window-minutes"))
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| (1..=525600).contains(v))
        else {
            continue;
        };
        let Some(reset) = header(headers, &format!("{prefix}-reset-at"))
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|v| v.checked_mul(1000))
            .filter(|v| *v > now)
        else {
            continue;
        };
        windows.push(object([
            ("id", id.into()),
            ("pool", "".into()),
            ("used_percent", Value::Number(used.to_string())),
            ("span_seconds", (minutes * 60).into()),
            ("reset_at_ms", reset.into()),
            ("aside", false.into()),
        ]));
    }
    let balance = if header(headers, "x-codex-credits-unlimited") == Some("false")
        && matches!(
            header(headers, "x-codex-credits-has-credits"),
            Some("true" | "false")
        ) {
        numeric(header(headers, "x-codex-credits-balance")).map(|amount| {
            object([
                ("amount", Value::Number(amount.to_string())),
                ("unit", "credits".into()),
            ])
        })
    } else {
        None
    };
    if windows.is_empty() && balance.is_none() {
        return None;
    }
    let observation = format!(
        "{}:{}:{}",
        std::process::id(),
        now,
        OBSERVATIONS.fetch_add(1, Ordering::Relaxed)
    );
    Some(
        object([
            ("version", 1u64.into()),
            ("source_id", source.into()),
            (
                "observation_id",
                digest::sha256(observation.as_bytes()).into(),
            ),
            ("observed_at_ms", now.into()),
            ("fresh", true.into()),
            ("success", true.into()),
            ("windows", Value::Array(windows)),
            ("balance", balance.unwrap_or_default()),
        ])
        .stringify(),
    )
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(body: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: "POST".into(),
            path: "/v1/responses".into(),
            headers: headers
                .iter()
                .map(|(a, b)| ((*a).into(), (*b).into()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }
    #[test]
    fn identities_are_opaque_and_request_content_is_never_in_metadata() {
        let r = request(
            r#"{"model":"fixture","input":[{"role":"user","content":"PRIVATE-prompt@example.invalid"}]}"#,
            &[
                ("User-Agent", "codex-cli/0.170"),
                ("session_id", "PRIVATE-session"),
                ("Authorization", "PRIVATE-token"),
            ],
        );
        let a = request_metadata(&r, 2);
        let b = request_metadata(&r, 2);
        assert_eq!(a, b);
        assert!(!a.contains("PRIVATE"));
        let v = crate::state_json::parse(a.as_bytes()).unwrap();
        assert_eq!(v.get("agent").as_str(), Some("codex"));
        assert_eq!(v.get("session").as_str().unwrap().len(), 64);
        assert_eq!(v.get("conversation").as_str().unwrap().len(), 64);
    }
    #[test]
    fn compaction_subagent_title_ambiguity_and_unknown_shapes_cannot_complete_a_turn() {
        for (body, headers, kind, subagent) in [
            (
                r#"{"model":"f","input":[{"type":"compaction_trigger"}]}"#,
                vec![],
                "compaction",
                false,
            ),
            (
                r#"{"model":"f","metadata":{"purpose":"title"}}"#,
                vec![],
                "title",
                false,
            ),
            (
                r#"{"model":"f"}"#,
                vec![("x-openai-subagent", "collab_spawn")],
                "generation",
                true,
            ),
            (
                r#"{"model":"f"}"#,
                vec![("session_id", "a"), ("x-session-id", "b")],
                "unknown",
                false,
            ),
            ("[]", vec![], "unknown", false),
        ] {
            let text = request_metadata(&request(body, &headers), 2);
            let v = crate::state_json::parse(text.as_bytes()).unwrap();
            assert_eq!(v.get("kind").as_str(), Some(kind));
            assert_eq!(v.get("subagent").as_bool(), Some(subagent));
        }
        let r = request(
            r#"{"model":"f","input":[{"type":"compaction","encrypted_content":"opaque"},{"role":"user","content":"hello"}]}"#,
            &[],
        );
        let v = crate::state_json::parse(request_metadata(&r, 2).as_bytes()).unwrap();
        assert_eq!(v.get("kind").as_str(), Some("generation"));
    }
    #[test]
    fn quota_requires_complete_fresh_unambiguous_account_wide_windows() {
        let headers = vec![
            ("x-codex-primary-used-percent".into(), "82.5".into()),
            ("x-codex-primary-window-minutes".into(), "300".into()),
            ("x-codex-primary-reset-at".into(), "1900000000".into()),
            ("x-codex-other-primary-used-percent".into(), "100".into()),
            ("x-codex-other-primary-window-minutes".into(), "60".into()),
            ("x-codex-other-primary-reset-at".into(), "1900000000".into()),
            ("Set-Cookie".into(), "PRIVATE-cookie".into()),
        ];
        let text = quota(&"a".repeat(64), &headers, 1800000000000).unwrap();
        assert!(!text.contains("PRIVATE"));
        assert!(!text.contains("other"));
        let v = crate::state_json::parse(text.as_bytes()).unwrap();
        assert_eq!(v.get("windows").as_array().len(), 1);
        assert_eq!(
            v.get("windows").as_array()[0].get("span_seconds").as_u64(),
            Some(18000)
        );
        let mut duplicate = headers.clone();
        duplicate.push(("X-CODEX-PRIMARY-USED-PERCENT".into(), "0".into()));
        assert!(quota(&"a".repeat(64), &duplicate, 1800000000000).is_none());
        assert!(quota(&"a".repeat(64), &headers, 1950000000000).is_none());
        assert!(quota(&"a".repeat(64), &headers[3..], 1800000000000).is_none());
    }
    #[test]
    fn credit_balance_requires_explicit_units_and_never_synthesizes_missing_amounts() {
        let headers = vec![
            ("x-codex-credits-unlimited".into(), "false".into()),
            ("x-codex-credits-has-credits".into(), "true".into()),
            ("x-codex-credits-balance".into(), "2.50".into()),
        ];
        let text = quota(&"a".repeat(64), &headers, 1800000000000).unwrap();
        let v = crate::state_json::parse(text.as_bytes()).unwrap();
        assert_eq!(v.get("balance").get("unit").as_str(), Some("credits"));
        assert!(quota(&"a".repeat(64), &headers[..2], 1800000000000).is_none());
        let mut unlimited = headers;
        unlimited[0].1 = "true".into();
        assert!(quota(&"a".repeat(64), &unlimited, 1800000000000).is_none());
    }
}
