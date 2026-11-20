//! Optional OpenCode/pi provider bridge. A shared Bun host is started on demand.
use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::Path,
    rc::{Rc, Weak},
    sync::Mutex,
    time::{Duration, Instant},
};
use zcalld_compat_client::{
    Call, Host, decode, encode,
    json::{self, Value, object},
};
use zcalld_plugin_api::{
    ContextPlugin, ContextPluginExport, GatewayPlugin, GatewayPluginExport, ProviderPlugin,
    ProviderPluginExport, ResponsePlugin, ResponsePluginExport, status,
};
use zplugin::{Error, OwnedResource, ResourceHandler, Result};
mod codex;
mod maintenance;
mod mcp;
use zcalld_plugin_api::{HttpPluginExport, RecoveryPluginExport};
static INIT: Mutex<()> = Mutex::new(());
thread_local! {static CONTEXTS:RefCell<HashMap<String,Weak<RefCell<Context>>>>=RefCell::new(HashMap::new());}
#[derive(Default)]
struct Context {
    metadata: Value,
    gateway: Option<(Host, Value)>,
    request: Value,
    reply: Vec<u8>,
    content_type: String,
    protocol: String,
    success: bool,
}

#[derive(Default)]
struct Community {
    config: Value,
    host: Option<(u64, Host)>,
    pending: Option<(Vec<u8>, Vec<u8>)>,
    ctx: Rc<RefCell<Context>>,
}
fn fail<T>(_: T) -> Error {
    Error::PluginStatus(6)
}
fn read(path: &str) -> Result<Value> {
    let metadata = std::fs::metadata(path).map_err(fail)?;
    if metadata.len() > 8 * 1024 * 1024 {
        return Err(Error::InvalidArgument);
    }
    json::parse(&std::fs::read(path).map_err(fail)?).map_err(fail)
}
impl Community {
    fn configure_host(&mut self, c: &str) -> Result<()> {
        let config = json::parse(c.as_bytes()).map_err(|_| Error::InvalidArgument)?;
        for k in ["bun", "host", "pi", "registry"] {
            let path = config.get(k).as_str().ok_or(Error::InvalidArgument)?;
            if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
                return Err(Error::InvalidArgument);
            }
        }
        let key = config.stringify();
        self.ctx = CONTEXTS.with(|all| {
            let mut all = all.borrow_mut();
            if let Some(value) = all.get(&key).and_then(Weak::upgrade) {
                return value;
            }
            let value = Rc::new(RefCell::new(Context::default()));
            all.insert(key, Rc::downgrade(&value));
            value
        });
        self.config = config;
        Ok(())
    }
    // Start shared initialization without waiting for plugin code. Every operation
    // still waits on the same host-side ready promise, including failures.
    fn host_start(&mut self) -> Result<(Host, Option<Call>)> {
        let registry_path = self
            .config
            .get("registry")
            .as_str()
            .ok_or(Error::InvalidArgument)?;
        let registry = read(registry_path)?;
        let generation = host_generation(&registry);
        if let Some((g, h)) = &self.host
            && *g == generation
        {
            return Ok((h.clone(), None));
        }
        let _lock = INIT.lock().map_err(fail)?;
        let key = format!("community:{registry_path}:{generation}");
        let bun = self
            .config
            .get("bun")
            .as_str()
            .ok_or(Error::InvalidArgument)?;
        let script = self
            .config
            .get("host")
            .as_str()
            .ok_or(Error::InvalidArgument)?;
        let directory = Path::new(registry_path)
            .parent()
            .ok_or(Error::InvalidArgument)?;
        let (host, fresh) =
            Host::shared(&key, Path::new(bun), &[script.into()], Some(directory)).map_err(fail)?;
        let initialization = if fresh {
            let mut plugins = Vec::new();
            for p in registry.get("plugins").as_array() {
                if p.get("enabled").as_bool() != Some(true)
                    || p.get("kind").as_str() != Some("community")
                {
                    continue;
                }
                let mut item = object([
                    ("spec", p.get("source").clone()),
                    ("target", p.get("path").clone()),
                    ("options", p.get("options").clone()),
                ]);
                if let Some(gateway) = p.get("gateway").as_str() {
                    item.set("gateway", gateway.into());
                }
                plugins.push(item);
            }
            let auth = self
                .config
                .get("auth_path")
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| directory.join("plugin-auth.json").to_string_lossy().into());
            let params = object([
                ("authPath", auth.into()),
                ("registryPath", registry_path.into()),
                ("piPath", self.config.get("pi").clone()),
                ("directory", directory.to_string_lossy().to_string().into()),
                ("plugins", Value::Array(plugins)),
                ("config", registry.get("config").clone()),
                ("prefer", registry.get("prefer").clone()),
                ("options", registry.get("provider_options").clone()),
                ("modelsDevPath", registry.get("models_dev_path").clone()),
                ("nativeStrict", true.into()),
            ]);
            Some(
                host.start(object([("method", "init".into()), ("params", params)]))
                    .map_err(fail)?,
            )
        } else {
            None
        };
        self.host = Some((generation, host.clone()));
        Ok((host, initialization))
    }
    fn host(&mut self) -> Result<Host> {
        let (host, initialization) = self.host_start()?;
        if let Some(mut call) = initialization {
            loop {
                let event = call.recv().map_err(fail)?;
                if !matches!(event.get("error"), Value::Null) {
                    return Err(Error::PluginStatus(6));
                }
                if event.as_object().is_some_and(|v| v.contains_key("result")) {
                    call.finish();
                    break;
                }
            }
        }
        Ok(host)
    }
    fn close_gateway(&mut self) {
        if let Some((host, mut params)) = self.ctx.borrow_mut().gateway.take() {
            params.set("operation", "close".into());
            let _ = host.rpc(object([
                ("method", "gatewayRequest".into()),
                ("params", params),
            ]));
        }
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        if let Some((host, mut params)) = self.gateway.take() {
            params.set("operation", "close".into());
            let _ = host.rpc(object([
                ("method", "gatewayRequest".into()),
                ("params", params),
            ]));
        }
    }
}
impl GatewayPlugin for Community {
    fn configure(&mut self, c: &str) -> Result<()> {
        self.configure_host(c)
    }
    fn route(&mut self, _: &str, _: &mut [u8]) -> Result<u64> {
        Ok(0)
    }
    fn request(&mut self, protocol: u32, input: &[u8], output: &mut [u8]) -> Result<u64> {
        if !self.pending.as_ref().is_some_and(|p| p.0 == input) {
            self.pending = None;
            self.close_gateway();
            let registry = read(
                self.config
                    .get("registry")
                    .as_str()
                    .ok_or(Error::InvalidArgument)?,
            )?;
            if !registry.get("plugins").as_array().iter().any(|p| {
                p.get("enabled").as_bool() == Some(true)
                    && p.get("gateway").as_str().is_some_and(|v| !v.is_empty())
            }) {
                return Ok(0);
            }
            let host = self.host()?;
            let body = json::parse(input).map_err(|_| Error::InvalidArgument)?;
            let mut context = self.ctx.borrow().metadata.clone();
            context.remove("request");
            let protocol_name = match protocol {
                1 => "chat",
                2 => "responses",
                3 => "anthropic",
                4 => "gemini",
                5 => "completions",
                6 => "interactions",
                _ => return Err(Error::InvalidArgument),
            };
            context.set("protocol", protocol_name.into());
            if context.get("model").as_str().unwrap_or("").is_empty() {
                context.set("model", body.get("model").as_str().unwrap_or("").into());
            }
            if context.get("session").as_str().unwrap_or("").is_empty() {
                context.set("session", session(&body).into());
            }
            let result = host
                .rpc(object([
                    ("method", "gatewayRequest".into()),
                    (
                        "params",
                        object([
                            ("operation", "begin".into()),
                            ("body", body),
                            ("context", context),
                        ]),
                    ),
                ]))
                .map_err(fail)?;
            if matches!(result, Value::Null) {
                return Ok(0);
            }
            let next = result.get("body").clone();
            {
                let mut ctx = self.ctx.borrow_mut();
                ctx.gateway = Some((
                    host,
                    object([
                        ("id", result.get("id").clone()),
                        ("token", result.get("token").clone()),
                    ]),
                ));
                ctx.request = next.clone();
                ctx.protocol = protocol_name.into();
                ctx.reply.clear();
                ctx.success = false;
            }
            let next = next.stringify().into_bytes();
            if next == input {
                return Ok(0);
            }
            self.pending = Some((input.into(), next));
        }
        let pending = self.pending.as_ref().ok_or(Error::InvalidArgument)?;
        if output.len() < pending.1.len() {
            return Err(Error::BufferTooSmall);
        }
        let n = pending.1.len();
        output[..n].copy_from_slice(&pending.1);
        self.pending = None;
        Ok(n as u64)
    }
    fn observe(&mut self, status: u32, _: u64, _: u64, _: u64) -> Result<()> {
        self.pending = None;
        let run = {
            let mut ctx = self.ctx.borrow_mut();
            let reply = if ctx.content_type.contains("text/event-stream") {
                aggregate_sse(&ctx.reply, &ctx.protocol)
            } else {
                json::parse(&ctx.reply).unwrap_or(Value::Null)
            };
            let result = ctx.gateway.take().map(|(host, mut params)| {
                params.set("operation", "finish".into());
                params.set(
                    "success",
                    ((200..300).contains(&status) && ctx.success).into(),
                );
                params.set("reply", reply);
                params.set("request", ctx.request.clone());
                (host, params)
            });
            ctx.reply.clear();
            ctx.request = Value::Null;
            result
        };
        if let Some((host, params)) = run {
            host.rpc(object([
                ("method", "gatewayRequest".into()),
                ("params", params),
            ]))
            .map_err(fail)?;
        }
        Ok(())
    }
}
// Account metadata changes do not restart a host that may be rotating a token.
fn host_generation(registry: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    for key in [
        "plugins",
        "config",
        "prefer",
        "provider_options",
        "models_dev_path",
    ] {
        key.hash(&mut hasher);
        registry.get(key).stringify().hash(&mut hasher);
    }
    hasher.finish()
}
fn select_account(registry: &Value, params: &mut Value) -> Result<()> {
    let provider = params
        .get("provider")
        .as_str()
        .ok_or(Error::InvalidArgument)?
        .to_owned();
    let explicit = params.get("account").as_str().filter(|s| !s.is_empty());
    let default = registry
        .get("account_defaults")
        .get(&provider)
        .as_str()
        .filter(|s| !s.is_empty());
    let mut selected = explicit.or(default).map(str::to_owned);
    let belongs = |id: &str| {
        id == provider
            || id
                .strip_prefix(provider.as_str())
                .is_some_and(|rest| rest.starts_with('#'))
    };
    if selected.is_none()
        && let Some(accounts) = registry.get("accounts").as_object()
    {
        let mut any = false;
        for (id, meta) in accounts {
            if belongs(id) {
                any = true;
                if meta.get("enabled").as_bool() == Some(true) {
                    selected = Some(id.clone());
                    break;
                }
            }
        }
        if any && selected.is_none() {
            return Err(Error::PluginStatus(6));
        }
    }
    if let Some(id) = selected {
        if !belongs(&id) {
            return Err(Error::InvalidArgument);
        }
        if registry.get("accounts").get(&id).get("enabled").as_bool() == Some(false) {
            return Err(Error::PluginStatus(6));
        }
        params.set("account", id.into());
    }
    Ok(())
}
impl ProviderPlugin for Community {
    fn configure(&mut self, c: &str) -> Result<()> {
        self.configure_host(c)
    }
    fn fetch(&mut self, request: &[u8]) -> Result<OwnedResource> {
        let started = Instant::now();
        let mut params = json::parse(request).map_err(|_| Error::InvalidArgument)?;
        // Maintenance may use only an explicit configured provider account,
        // never the legacy default/first-account generation fallback.
        let explicit_account = params
            .get("account")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let timeout = params
            .get("timeout_ms")
            .as_u64()
            .unwrap_or(300_000)
            .clamp(1, 3_600_000);
        let deadline = started + Duration::from_millis(timeout);
        let registry = read(
            self.config
                .get("registry")
                .as_str()
                .ok_or(Error::InvalidArgument)?,
        )?;
        select_account(&registry, &mut params)?;
        let body = params.get("body").as_str().ok_or(Error::InvalidArgument)?;
        let encoded = encode(body.as_bytes());
        params.set("body", encoded.into());
        params.set("window", 524_288usize.into());
        // Initialization and an optional one-shot preflight are polled through
        // the resource. A generation RPC is not sent early or replayed here.
        OwnedResource::new(maintenance::Fetch::new(
            self,
            params,
            explicit_account.as_deref(),
            deadline,
        )?)
    }
}
struct Stream {
    call: Call,
    initialization: Option<Call>,
    head: Option<Vec<u8>>,
    bytes: Vec<u8>,
    offset: usize,
    ended: bool,
    deadline: Instant,
}
impl Stream {
    fn event(&mut self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(Error::PluginStatus(status::PROVIDER_TIMED_OUT));
        }
        if let Some(initialization) = &mut self.initialization {
            let Some(event) = initialization
                .poll()
                .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?
            else {
                return Err(Error::PluginStatus(3));
            };
            if !matches!(event.get("error"), Value::Null) {
                return Err(Error::PluginStatus(status::PROVIDER_REQUEST_FAILED));
            }
            if !event.as_object().is_some_and(|v| v.contains_key("result")) {
                return Err(Error::PluginStatus(3));
            }
            initialization.finish();
            self.initialization = None;
            if Instant::now() >= self.deadline {
                return Err(Error::PluginStatus(status::PROVIDER_TIMED_OUT));
            }
        }
        let Some(event) = self
            .call
            .poll()
            .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?
        else {
            return Err(Error::PluginStatus(3));
        };
        if !matches!(event.get("error"), Value::Null) {
            return Err(Error::PluginStatus(status::PROVIDER_REQUEST_FAILED));
        }
        match event.get("event").as_str() {
            Some("head") => {
                if self.head.is_some() {
                    return Err(Error::PluginStatus(status::PROVIDER_HEAD_INVALID));
                }
                let bytes = object([
                    ("status", event.get("status").clone()),
                    ("headers", event.get("headers").clone()),
                ])
                .stringify()
                .into_bytes();
                if bytes.len() > 16_384 {
                    return Err(Error::PluginStatus(status::PROVIDER_HEAD_INVALID));
                }
                self.head = Some(bytes);
            }
            Some("chunk") => {
                if self.head.is_none() {
                    return Err(Error::PluginStatus(status::PROVIDER_HEAD_INVALID));
                }
                let invalid = || Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED);
                let bytes = decode(event.get("data").as_str().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?;
                if bytes.is_empty() || bytes.len() > 65536 {
                    return Err(Error::InvalidArgument);
                }
                self.bytes = bytes;
                self.offset = 0;
            }
            _ => {
                if event.as_object().is_some_and(|v| v.contains_key("result")) {
                    if self.head.is_none() {
                        return Err(Error::PluginStatus(status::PROVIDER_HEAD_INVALID));
                    }
                    self.ended = true;
                    self.call.finish();
                }
            }
        }
        Ok(())
    }
}
impl ResourceHandler for Stream {
    const ID: &'static str = "dev.zcalld.provider-stream";
    const VERSION: u32 = 1;
    fn call(&mut self, operation: u32, _input: &[u8], output: &mut [u8]) -> Result<usize> {
        match operation {
            1 => {
                if self.head.is_none() {
                    self.event()?;
                }
                let head = self.head.as_ref().ok_or(Error::PluginStatus(3))?;
                if output.len() < head.len() {
                    return Err(Error::BufferTooSmall);
                }
                output[..head.len()].copy_from_slice(head);
                Ok(head.len())
            }
            2 => {
                if output.is_empty() {
                    return Ok(0);
                }
                if self.offset == self.bytes.len() {
                    self.bytes.clear();
                    self.offset = 0;
                    if self.ended {
                        return Ok(0);
                    }
                    self.event()?;
                    if self.bytes.is_empty() {
                        return if self.ended {
                            Ok(0)
                        } else {
                            Err(Error::PluginStatus(3))
                        };
                    }
                }
                let count = output.len().min(self.bytes.len() - self.offset);
                output[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
                self.offset += count;
                let credit = count
                    + if self.offset == self.bytes.len() {
                        2048
                    } else {
                        0
                    };
                self.call.credit(credit).map_err(fail)?;
                Ok(count)
            }
            3 => {
                self.ended = true;
                Ok(0)
            }
            _ => Err(Error::Unsupported),
        }
    }
}
fn create() -> Result<Community> {
    Ok(Community::default())
}
zplugin::export_interfaces!(
 version=(zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MAJOR")),zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MINOR")),zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_PATCH"))),unload=pinned;
 GatewayPluginExport<Community> => create,
 ProviderPluginExport<Community> => create,
 ContextPluginExport<Community> => create,
 ResponsePluginExport<Community> => create,
 HttpPluginExport<Community> => create,
 RecoveryPluginExport<Community> => create,
);

impl ContextPlugin for Community {
    fn configure(&mut self, c: &str) -> Result<()> {
        self.configure_host(c)
    }
    fn context(&mut self, metadata: &str) -> Result<()> {
        let value = json::parse(metadata.as_bytes()).map_err(|_| Error::InvalidArgument)?;
        if value.get("phase").as_str() == Some("route") {
            return Ok(());
        }
        let mut kept = object([]);
        for key in [
            "protocol", "model", "session", "cwd", "agent", "origin", "path", "kind",
        ] {
            if let Some(v) = value.get(key).as_str() {
                if v.len() > 4096 || v.chars().any(|c| c.is_control()) {
                    return Err(Error::InvalidArgument);
                }
                kept.set(key, v.into());
            }
        }
        if let Some(v) = value.get("stream").as_bool() {
            kept.set("stream", v.into());
        }
        self.ctx.borrow_mut().metadata = kept;
        Ok(())
    }
}
impl ResponsePlugin for Community {
    fn configure(&mut self, c: &str) -> Result<()> {
        self.configure_host(c)
    }
    fn begin(&mut self, status: u32, content_type: &str) -> Result<()> {
        let mut ctx = self.ctx.borrow_mut();
        ctx.reply.clear();
        ctx.content_type = content_type.into();
        ctx.success = (200..300).contains(&status);
        Ok(())
    }
    fn write(&mut self, input: &[u8], final_chunk: u32, output: &mut [u8]) -> Result<u64> {
        if final_chunk > 1 {
            return Err(Error::InvalidArgument);
        }
        if output.len() < input.len() {
            return Err(Error::BufferTooSmall);
        }
        let mut ctx = self.ctx.borrow_mut();
        if ctx.gateway.is_some() {
            if ctx.reply.len().saturating_add(input.len()) > 16 * 1024 * 1024 {
                return Err(Error::BufferTooSmall);
            }
            ctx.reply.extend_from_slice(input);
        }
        output[..input.len()].copy_from_slice(input);
        Ok(input.len() as u64)
    }
}
fn session(body: &Value) -> String {
    for value in [
        body.get("prompt_cache_key"),
        body.get("session_id"),
        body.get("conversation_id"),
        body.get("metadata").get("session_id"),
    ] {
        if let Some(v) = value.as_str()
            && !v.is_empty()
            && v.len() <= 4096
        {
            return v.into();
        }
    }
    if let Some(user) = body.get("metadata").get("user_id").as_str() {
        if let Ok(v) = json::parse(user.as_bytes())
            && let Some(s) = v.get("session_id").as_str()
            && s.len() <= 4096
        {
            return s.into();
        }
        if let Some((_, s)) = user.rsplit_once("_session_")
            && !s.is_empty()
            && s.len() <= 128
        {
            return s.into();
        }
    }
    String::new()
}
fn append_string(value: &mut Value, key: &str, part: &str) {
    let mut text = value.get(key).as_str().unwrap_or("").to_owned();
    text.push_str(part);
    value.set(key, text.into());
}
fn merge(value: &mut Value, other: &Value) {
    if let Some(fields) = other.as_object() {
        for (k, v) in fields {
            value.set(k, v.clone());
        }
    }
}

fn child<'a>(value: &'a mut Value, key: &str) -> &'a mut Value {
    if !matches!(value.get(key), Value::Object(_)) {
        value.set(key, object([]));
    }
    value.get_mut(key).unwrap()
}
fn slot<'a>(value: &'a mut Value, key: &str, index: usize) -> Option<&'a mut Value> {
    if index > 4096 {
        return None;
    }
    if !matches!(value.get(key), Value::Array(_)) {
        value.set(key, Value::Array(Vec::new()));
    }
    let array = value.get_mut(key)?.as_array_mut()?;
    if index >= array.len() {
        array.resize_with(index + 1, || object([]));
    }
    array.get_mut(index)
}
fn aggregate_sse(bytes: &[u8], protocol: &str) -> Value {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Value::Null;
    };
    let mut reply = Value::Null;
    let mut data = String::new();
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(part) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n')
            }
            data.push_str(part.strip_prefix(' ').unwrap_or(part));
        } else if line.is_empty() && !data.is_empty() {
            if data.trim() != "[DONE]"
                && let Ok(event) = json::parse(data.as_bytes())
            {
                absorb(&mut reply, &event, protocol);
            }
            data.clear();
        }
    }
    reply
}
fn absorb(reply: &mut Value, event: &Value, protocol: &str) {
    match protocol {
        "responses" => {
            if !matches!(event.get("response"), Value::Null) {
                *reply = event.get("response").clone();
            }
            if let Some(item) = event.get("item").as_object() {
                let _ = item;
                if event.get("type").as_str() == Some("response.output_item.done") {
                    let index = event.get("output_index").as_u64().unwrap_or(0) as usize;
                    if let Some(slot) = slot(reply, "output", index) {
                        *slot = event.get("item").clone();
                    }
                }
            }
        }
        "anthropic" => match event.get("type").as_str() {
            Some("message_start") => *reply = event.get("message").clone(),
            Some("message_delta") => {
                merge(reply, event.get("delta"));
                merge(child(reply, "usage"), event.get("usage"));
            }
            Some("content_block_start") => {
                if let Some(slot) = slot(
                    reply,
                    "content",
                    event.get("index").as_u64().unwrap_or(0) as usize,
                ) {
                    *slot = event.get("content_block").clone();
                }
            }
            Some("content_block_delta") => {
                if let Some(block) = slot(
                    reply,
                    "content",
                    event.get("index").as_u64().unwrap_or(0) as usize,
                ) {
                    let delta = event.get("delta");
                    for key in ["text", "thinking", "signature"] {
                        if let Some(value) = delta.get(key).as_str() {
                            append_string(block, key, value);
                        }
                    }
                    if let Some(value) = delta.get("partial_json").as_str() {
                        append_string(block, "__partial_json", value);
                    }
                }
            }
            Some("content_block_stop") => {
                if let Some(block) = slot(
                    reply,
                    "content",
                    event.get("index").as_u64().unwrap_or(0) as usize,
                ) && let Some(Value::String(value)) = block.remove("__partial_json")
                    && let Ok(input) = json::parse(value.as_bytes())
                {
                    block.set("input", input);
                }
            }
            _ => {}
        },
        "gemini" => {
            for key in [
                "usageMetadata",
                "modelVersion",
                "responseId",
                "promptFeedback",
            ] {
                if !matches!(event.get(key), Value::Null) {
                    reply.set(key, event.get(key).clone());
                }
            }
            for candidate in event.get("candidates").as_array() {
                let index = candidate.get("index").as_u64().unwrap_or(0) as usize;
                if let Some(target) = slot(reply, "candidates", index) {
                    for key in [
                        "index",
                        "finishReason",
                        "safetyRatings",
                        "citationMetadata",
                        "groundingMetadata",
                    ] {
                        if !matches!(candidate.get(key), Value::Null) {
                            target.set(key, candidate.get(key).clone());
                        }
                    }
                    let content = child(target, "content");
                    content.set(
                        "role",
                        candidate
                            .get("content")
                            .get("role")
                            .as_str()
                            .unwrap_or("model")
                            .into(),
                    );
                    if !matches!(content.get("parts"), Value::Array(_)) {
                        content.set("parts", Value::Array(Vec::new()));
                    }
                    let parts = content.get_mut("parts").unwrap().as_array_mut().unwrap();
                    for part in candidate.get("content").get("parts").as_array() {
                        if let Some(text) = part.get("text").as_str()
                            && let Some(last) = parts.last_mut()
                            && last.get("text").as_str().is_some()
                            && last.get("thought") == part.get("thought")
                        {
                            append_string(last, "text", text);
                            continue;
                        }
                        parts.push(part.clone());
                    }
                }
            }
        }
        _ => {
            for key in ["id", "model", "created", "system_fingerprint", "usage"] {
                if !matches!(event.get(key), Value::Null) {
                    reply.set(key, event.get(key).clone());
                }
            }
            reply.set(
                "object",
                if protocol == "completions" {
                    "text_completion"
                } else {
                    "chat.completion"
                }
                .into(),
            );
            for choice in event.get("choices").as_array() {
                if let Some(target) = slot(
                    reply,
                    "choices",
                    choice.get("index").as_u64().unwrap_or(0) as usize,
                ) {
                    target.set("index", choice.get("index").clone());
                    if !matches!(choice.get("finish_reason"), Value::Null) {
                        target.set("finish_reason", choice.get("finish_reason").clone());
                    }
                    if let Some(text) = choice.get("text").as_str() {
                        append_string(target, "text", text);
                    }
                    let delta = choice.get("delta");
                    let message = child(target, "message");
                    if let Some(role) = delta.get("role").as_str() {
                        message.set("role", role.into());
                    } else if matches!(message.get("role"), Value::Null) {
                        message.set("role", "assistant".into());
                    }
                    for key in ["content", "refusal"] {
                        if let Some(text) = delta.get(key).as_str() {
                            append_string(message, key, text);
                        }
                    }
                    for call in delta.get("tool_calls").as_array() {
                        if let Some(target) = slot(
                            message,
                            "tool_calls",
                            call.get("index").as_u64().unwrap_or(0) as usize,
                        ) {
                            for key in ["id", "type"] {
                                if !matches!(call.get(key), Value::Null) {
                                    target.set(key, call.get(key).clone());
                                }
                            }
                            let function = child(target, "function");
                            for key in ["name", "arguments"] {
                                if let Some(text) = call.get("function").get(key).as_str() {
                                    append_string(function, key, text);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_selection_checks_runtime_metadata_without_restarting_auth() {
        let registry = json::parse(br#"{"plugins":[],"accounts":{"mock#one":{"enabled":false},"mock#two":{"enabled":true}},"account_defaults":{"mock":"mock#two"}}"#).unwrap();
        let mut request = object([("provider", "mock".into())]);
        select_account(&registry, &mut request).unwrap();
        assert_eq!(request.get("account").as_str(), Some("mock#two"));
        request.set("account", "mock#one".into());
        assert!(matches!(
            select_account(&registry, &mut request),
            Err(Error::PluginStatus(6))
        ));
        let mut other = registry.clone();
        other.set("accounts", Value::Null);
        other.set("account_defaults", Value::Null);
        assert_eq!(host_generation(&registry), host_generation(&other));
    }
    #[test]
    fn legacy_providers_keep_their_own_account_selection() {
        let registry = object([]);
        let mut request = object([("provider", "legacy".into())]);
        select_account(&registry, &mut request).unwrap();
        assert!(matches!(request.get("account"), Value::Null));
    }
    #[test]
    fn chat_reply_keeps_text_tools_and_usage() {
        let data=b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        let out = aggregate_sse(data, "chat");
        assert_eq!(
            out.get("choices").as_array()[0]
                .get("message")
                .get("content")
                .as_str(),
            Some("hello")
        );
        assert_eq!(out.get("usage").get("prompt_tokens").as_u64(), Some(3));
    }
    #[test]
    fn anthropic_usage_is_merged() {
        let data=b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0},\"content\":[]}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n";
        let out = aggregate_sse(data, "anthropic");
        assert_eq!(out.get("usage").get("input_tokens").as_u64(), Some(3));
        assert_eq!(out.get("usage").get("output_tokens").as_u64(), Some(2));
    }
    #[test]
    fn body_session_lookup_does_not_invent_identity() {
        let body =
            json::parse(br#"{"metadata":{"user_id":"user_x_session_test-session"}}"#).unwrap();
        assert_eq!(session(&body), "test-session");
        assert!(session(&object([])).is_empty());
    }
}
