//! Optional plugin-tools MCP endpoint. No listener and no new host process.
use super::{Community, Stream, fail, read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zcalld_compat_client::json::{self, Value, object};
use zcalld_plugin_api::HttpPlugin;
use zplugin::{Error, OwnedResource, ResourceHandler, Result};

const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER: &str = "magpie-tools"; // Preserve the imported caller-key scope name.
const MAX_BODY: usize = 4 << 20;
const MAX_RESPONSE: usize = 16 << 20;
static SESSION: AtomicU64 = AtomicU64::new(1);

fn endpoint(path: &str) -> Option<&str> {
    for base in ["/mcp/magpie/tools", "/mcp/zcalld/tools"] {
        if path == base {
            return Some("");
        }
        if let Some(name) = path.strip_prefix(base).and_then(|p| p.strip_prefix('/'))
            && !name.is_empty()
            && name.len() <= 24
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Some(name);
        }
    }
    None
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn bool_field(v: &Value, field: &str) -> Result<()> {
    if !matches!(v.get(field), Value::Null | Value::Bool(_)) {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

pub(super) fn validate_registry(registry: &Value) -> Result<()> {
    bool_field(registry, "mcp_tools_enabled")?;
    let rules = match registry.get("mcp_permissions") {
        Value::Null => return Ok(()),
        Value::Object(rules) if rules.len() <= 1024 => rules,
        _ => return Err(Error::InvalidArgument),
    };
    for (id, rule) in rules {
        if id.is_empty() || id.len() > 128 || rule.as_object().is_none() {
            return Err(Error::InvalidArgument);
        }
        for field in ["off", "read_only", "browser"] {
            bool_field(rule, field)?;
        }
        for field in ["expires_unix_ms", "max_concurrency"] {
            if !matches!(rule.get(field), Value::Null) && rule.get(field).as_u64().is_none() {
                return Err(Error::InvalidArgument);
            }
        }
        if rule.get("max_concurrency").as_u64().unwrap_or(0) > 4096 {
            return Err(Error::InvalidArgument);
        }
        // Token/currency budgets require accounting this adapter does not implement.
        // Refuse them instead of letting an imported restricted key become unlimited.
        if !matches!(rule.get("limit"), Value::Null)
            || !(matches!(rule.get("more_limits"), Value::Null)
                || matches!(rule.get("more_limits"), Value::Array(v) if v.is_empty()))
        {
            return Err(Error::InvalidArgument);
        }
        let grants = match rule.get("mcp") {
            Value::Null => &[][..],
            Value::Array(v) if v.len() <= 500 => v.as_slice(),
            _ => return Err(Error::InvalidArgument),
        };
        for grant in grants {
            let g = grant.as_str().ok_or(Error::InvalidArgument)?;
            if g == "-" {
                if grants.len() != 1 {
                    return Err(Error::InvalidArgument);
                }
                continue;
            }
            let (server, tool) = g.split_once('/').map_or((g, None), |(s, t)| (s, Some(t)));
            if server.is_empty()
                || server.len() > 64
                || !server.as_bytes()[0].is_ascii_alphanumeric()
                || !server
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || tool.is_some_and(|s| s.is_empty() || s.len() > 128 || s.trim() != s)
            {
                return Err(Error::InvalidArgument);
            }
        }
        let mut key = id.as_str();
        let mut seen = Vec::new();
        loop {
            if seen.contains(&key) || seen.len() >= 32 {
                return Err(Error::InvalidArgument);
            }
            seen.push(key);
            let current = rules.get(key).ok_or(Error::InvalidArgument)?;
            match current.get("issuer") {
                Value::Null => break,
                Value::String(next) if next.is_empty() => break,
                Value::String(next) => key = next,
                _ => return Err(Error::InvalidArgument),
            }
        }
    }
    Ok(())
}

fn permission(registry: &Value, metadata: &Value) -> Option<Value> {
    let mut key = match metadata.get("caller_key_id") {
        Value::Null if metadata.get("loopback").as_bool() == Some(true) => {
            return Some(object([
                ("grants", Value::Array(vec![])),
                ("concurrency", Value::Array(vec![])),
            ]));
        }
        Value::String(key) if !key.is_empty() => key.as_str(),
        _ => return None,
    };
    let mut grants = Vec::new();
    let mut concurrency = Vec::new();
    let mut seen = Vec::new();
    loop {
        if seen.len() >= 32 || seen.contains(&key) {
            return None;
        }
        seen.push(key);
        let rule = registry.get("mcp_permissions").get(key);
        if rule.as_object().is_none()
            || ["off", "read_only", "browser"]
                .iter()
                .any(|f| rule.get(f).as_bool() == Some(true))
            || rule
                .get("expires_unix_ms")
                .as_u64()
                .is_some_and(|n| n > 0 && n <= now_ms())
        {
            return None;
        }
        let list = rule.get("mcp").as_array();
        if !list.is_empty()
            && !list.iter().any(|g| {
                g.as_str().is_some_and(|s| {
                    s == SERVER
                        || s.strip_prefix(SERVER)
                            .is_some_and(|tail| tail.starts_with('/'))
                })
            })
        {
            return None;
        }
        grants.push(Value::Array(list.to_vec()));
        let n = rule.get("max_concurrency").as_u64().unwrap_or(0);
        if n > 0 {
            concurrency.push(object([("id", key.into()), ("max", n.into())]));
        }
        match rule.get("issuer").as_str().filter(|s| !s.is_empty()) {
            Some(next) => key = next,
            None => break,
        }
    }
    Some(object([
        ("grants", Value::Array(grants)),
        ("concurrency", Value::Array(concurrency)),
    ]))
}

fn allowed(acl: &Value, name: &str) -> bool {
    let specific = format!("{SERVER}/{name}");
    acl.get("grants").as_array().iter().all(|v| {
        v.as_array().is_empty()
            || v.as_array()
                .iter()
                .any(|g| g.as_str().is_some_and(|s| s == SERVER || s == specific))
    })
}

fn error(code: i32, message: &str) -> Value {
    object([
        ("code", Value::Number(code.to_string())),
        ("message", message.into()),
    ])
}

fn rpc(id: Value, name: &str, result: Value) -> String {
    object([("jsonrpc", "2.0".into()), ("id", id), (name, result)]).stringify()
}

fn tool_error(text: &str) -> Value {
    object([
        (
            "content",
            Value::Array(vec![object([
                ("type", "text".into()),
                ("text", text.into()),
            ])]),
        ),
        ("isError", true.into()),
    ])
}

pub(super) fn immediate(status: u64, headers: Value, bytes: Vec<u8>) -> Result<OwnedResource> {
    OwnedResource::new(MemoryStream {
        head: object([("status", status.into()), ("headers", headers)])
            .stringify()
            .into_bytes(),
        body: bytes,
        offset: 0,
    })
}

pub(super) fn failure(status: u64, message: &str) -> Result<OwnedResource> {
    immediate(
        status,
        object([("content-type", "application/json".into())]),
        object([("error", message.into())]).stringify().into_bytes(),
    )
}

impl HttpPlugin for Community {
    fn configure(&mut self, config: &str) -> Result<()> {
        self.configure_host(config)?;
        if super::claude::native_only(&self.config) {
            return Ok(());
        }
        let registry = read(
            self.config
                .get("registry")
                .as_str()
                .ok_or(Error::InvalidArgument)?,
        )?;
        validate_registry(&registry)?;
        super::codex::validate_registry(&registry)
    }

    fn accepts(&mut self, path: &str) -> Result<u32> {
        if super::claude::native_only(&self.config) {
            return Ok(0);
        }
        if super::codex::endpoint(path).is_some() {
            return super::codex::accepts(self, path);
        }
        Ok(u32::from(endpoint(path).is_some()))
    }

    fn handle(&mut self, metadata: &str, body: &[u8]) -> Result<OwnedResource> {
        if super::claude::native_only(&self.config) {
            return Err(Error::Unsupported);
        }
        let meta = json::parse(metadata.as_bytes()).map_err(fail)?;
        if super::codex::endpoint(meta.get("path").as_str().unwrap_or("")).is_some() {
            return super::codex::handle(self, &meta, body);
        }
        let one = endpoint(meta.get("path").as_str().ok_or(Error::InvalidArgument)?)
            .ok_or(Error::InvalidArgument)?;
        let registry = read(
            self.config
                .get("registry")
                .as_str()
                .ok_or(Error::InvalidArgument)?,
        )?;
        validate_registry(&registry)?;
        let Some(acl) = permission(&registry, &meta) else {
            return failure(403, "caller is not permitted to use this MCP server");
        };
        if meta.get("method").as_str() != Some("POST") {
            return immediate(
                405,
                object([
                    ("allow", "POST".into()),
                    ("content-type", "text/plain; charset=utf-8".into()),
                ]),
                b"POST JSON-RPC messages here (MCP Streamable HTTP)\n".to_vec(),
            );
        }
        if body.len() > MAX_BODY {
            return failure(413, "MCP request exceeds 4 MiB");
        }
        if let Some(version) = meta.get("headers").get("mcp-protocol-version").as_str()
            && !VERSIONS.contains(&version)
        {
            return failure(400, "unsupported MCP protocol version");
        }
        let input = match json::parse(body) {
            Ok(value) if matches!(value, Value::Object(_) | Value::Array(_)) => value,
            _ => {
                return immediate(
                    400,
                    object([("content-type", "application/json".into())]),
                    rpc(
                        Value::Null,
                        "error",
                        error(-32700, "parse error: expected a JSON-RPC message"),
                    )
                    .into_bytes(),
                );
            }
        };
        let batch = matches!(input, Value::Array(_));
        let messages = if batch {
            input.as_array()
        } else {
            std::slice::from_ref(&input)
        };
        if messages.is_empty()
            || messages.len() > 128
            || messages.iter().any(|v| v.as_object().is_none())
        {
            return failure(400, "MCP batch must contain 1 to 128 JSON-RPC objects");
        }
        let mut headers = object([("content-type", "application/json".into())]);
        if meta
            .get("headers")
            .get("mcp-session-id")
            .as_str()
            .is_none_or(str::is_empty)
            && messages
                .iter()
                .any(|m| m.get("method").as_str() == Some("initialize"))
        {
            // This is an opaque client label, never a credential or server-side session.
            headers.set(
                "mcp-session-id",
                format!(
                    "zcalld-{:x}-{:x}-{:x}",
                    now_ms(),
                    std::process::id(),
                    SESSION.fetch_add(1, Ordering::Relaxed)
                )
                .into(),
            );
        }
        let enabled = registry.get("mcp_tools_enabled").as_bool().unwrap_or(true);
        let plugins_on = registry.get("plugins").as_array().iter().any(|p| {
            p.get("enabled").as_bool() == Some(true) && p.get("kind").as_str() == Some("community")
        });
        let mut rows = Vec::new();
        let mut needs_host = false;
        for msg in messages {
            let id = msg.get("id").clone();
            let Some(method) = msg.get("method").as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if matches!(id, Value::Null) {
                continue;
            }
            let response = if !matches!(id, Value::String(_) | Value::Number(_))
                || msg.get("jsonrpc").as_str() != Some("2.0")
            {
                rpc(
                    Value::Null,
                    "error",
                    error(-32600, "invalid JSON-RPC request"),
                )
            } else {
                match method {
                    "initialize" => {
                        let asked = msg
                            .get("params")
                            .get("protocolVersion")
                            .as_str()
                            .unwrap_or("");
                        let version = if VERSIONS.contains(&asked) {
                            asked
                        } else {
                            VERSIONS[0]
                        };
                        rpc(id, "result", object([
                            ("protocolVersion", version.into()),
                            ("capabilities", object([("tools", object([]))])),
                            ("serverInfo", object([("name", "zcalld-tools".into()), ("title", "zcalld Community Tools".into()), ("version", env!("CARGO_PKG_VERSION").into())])),
                            ("instructions", "Tools supplied by explicitly enabled community plugins. Manage them with zcalld plugins.".into()),
                        ]))
                    }
                    "ping" => rpc(id, "result", object([])),
                    "tools/call"
                        if msg.get("params").as_object().is_none()
                            || msg
                                .get("params")
                                .get("name")
                                .as_str()
                                .is_none_or(str::is_empty)
                            || !matches!(
                                msg.get("params").get("arguments"),
                                Value::Null | Value::Object(_)
                            ) =>
                    {
                        rpc(id, "error", error(-32602, "invalid tool parameters"))
                    }
                    "tools/call"
                        if !allowed(&acl, msg.get("params").get("name").as_str().unwrap_or("")) =>
                    {
                        rpc(
                            id,
                            "error",
                            error(-32602, "caller is not permitted to use this MCP tool"),
                        )
                    }
                    "tools/list" if !enabled || !plugins_on => {
                        rpc(id, "result", object([("tools", Value::Array(vec![]))]))
                    }
                    "tools/call" if !enabled => rpc(
                        id,
                        "result",
                        tool_error("Community plugin tools are disabled."),
                    ),
                    "tools/call" if !plugins_on => rpc(
                        id,
                        "error",
                        error(-32602, "unknown tool: no enabled plugin supplies it"),
                    ),
                    "tools/list" | "tools/call" => {
                        needs_host = true;
                        rows.push(object([
                            ("id_json", id.stringify().into()),
                            ("method", method.into()),
                            ("params", msg.get("params").clone()),
                        ]));
                        continue;
                    }
                    _ => rpc(id, "error", error(-32601, "method not found")),
                }
            };
            rows.push(object([("reply_json", response.into())]));
        }
        if rows.is_empty() {
            return immediate(202, headers, Vec::new());
        }
        let max_response = meta
            .get("max_response_bytes")
            .as_u64()
            .unwrap_or(MAX_RESPONSE as u64)
            .clamp(1, MAX_RESPONSE as u64) as usize;
        if !needs_host {
            let joined = rows
                .iter()
                .map(|r| r.get("reply_json").as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(",");
            let body = if batch { format!("[{joined}]") } else { joined };
            if body.len() > max_response {
                return failure(502, "MCP response exceeds configured limit");
            }
            return immediate(200, headers, body.into_bytes());
        }
        let timeout = meta
            .get("timeout_ms")
            .as_u64()
            .unwrap_or(60_000)
            .clamp(1, 3_600_000);
        let params = object([
            ("requests", Value::Array(rows)),
            ("batch", batch.into()),
            ("one", one.into()),
            ("acl", acl),
            ("headers", headers),
            ("timeout_ms", timeout.into()),
            ("max_response_bytes", max_response.into()),
            ("window", 524_288usize.into()),
            (
                "agent",
                meta.get("agent")
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(256)
                    .collect::<String>()
                    .into(),
            ),
        ]);
        let call = self
            .host()?
            .start(object([("method", "mcpHttp".into()), ("params", params)]))
            .map_err(fail)?;
        OwnedResource::new(Stream {
            call,
            initialization: None,
            head: None,
            bytes: Vec::new(),
            offset: 0,
            ended: false,
            deadline: Instant::now() + Duration::from_millis(timeout),
        })
    }
}

struct MemoryStream {
    head: Vec<u8>,
    body: Vec<u8>,
    offset: usize,
}
impl ResourceHandler for MemoryStream {
    const ID: &'static str = "dev.zcalld.provider-stream";
    const VERSION: u32 = 1;
    fn call(&mut self, op: u32, _input: &[u8], out: &mut [u8]) -> Result<usize> {
        match op {
            1 => {
                if out.len() < self.head.len() {
                    return Err(Error::BufferTooSmall);
                }
                out[..self.head.len()].copy_from_slice(&self.head);
                Ok(self.head.len())
            }
            2 => {
                let n = out.len().min(self.body.len() - self.offset);
                out[..n].copy_from_slice(&self.body[self.offset..self.offset + n]);
                self.offset += n;
                Ok(n)
            }
            3 => {
                self.body.clear();
                self.offset = 0;
                Ok(0)
            }
            _ => Err(Error::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(v: &str) -> Value {
        json::parse(v.as_bytes()).unwrap()
    }
    #[test]
    fn paths_do_not_capture_unrelated_routes() {
        for p in [
            "/mcp/magpie/tools",
            "/mcp/magpie/tools/weather",
            "/mcp/zcalld/tools",
        ] {
            assert!(endpoint(p).is_some());
        }
        for p in [
            "/mcp/magpie/toolsmith",
            "/mcp/magpie/tools/",
            "/mcp/magpie/tools/a/b",
            "/v1/messages",
        ] {
            assert!(endpoint(p).is_none());
        }
    }
    #[test]
    fn caller_and_issuer_scopes_intersect() {
        let registry = parse(
            r#"{"mcp_permissions":{"parent":{"mcp":["magpie-tools/a"]},"child":{"mcp":["magpie-tools"],"issuer":"parent"}}}"#,
        );
        validate_registry(&registry).unwrap();
        let acl = permission(
            &registry,
            &parse(r#"{"caller_key_id":"child","loopback":false}"#),
        )
        .unwrap();
        assert!(allowed(&acl, "a"));
        assert!(!allowed(&acl, "b"));
        assert!(
            permission(
                &registry,
                &parse(r#"{"caller_key_id":"unknown","loopback":true}"#)
            )
            .is_none()
        );
        assert!(
            permission(
                &registry,
                &parse(r#"{"caller_key_id":null,"loopback":false}"#)
            )
            .is_none()
        );
        assert!(
            permission(
                &registry,
                &parse(r#"{"caller_key_id":null,"loopback":true}"#)
            )
            .is_some()
        );
    }
    #[test]
    fn unknown_limits_and_cycles_never_relax_permissions() {
        for raw in [
            r#"{"mcp_permissions":{"a":{"limit":{"requests":1}}}}"#,
            r#"{"mcp_permissions":{"a":{"issuer":"a"}}}"#,
            r#"{"mcp_permissions":{"a":{"mcp":["-","magpie-tools"]}}}"#,
        ] {
            assert!(validate_registry(&parse(raw)).is_err());
        }
    }
    #[test]
    fn disabled_and_expired_keys_are_refused() {
        for field in ["off", "read_only", "browser"] {
            let registry = parse(&format!(
                r#"{{"mcp_permissions":{{"a":{{"{field}":true}}}}}}"#
            ));
            assert!(
                permission(
                    &registry,
                    &parse(r#"{"caller_key_id":"a","loopback":true}"#)
                )
                .is_none()
            );
        }
        assert!(
            permission(
                &parse(r#"{"mcp_permissions":{"a":{"expires_unix_ms":1}}}"#),
                &parse(r#"{"caller_key_id":"a","loopback":true}"#)
            )
            .is_none()
        );
    }
}
