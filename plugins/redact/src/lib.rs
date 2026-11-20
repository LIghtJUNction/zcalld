//! Native opt-in adapter for the isolated, attributed privacy engine.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::Path,
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use zcalld_compat_client::{
    Host, decode, encode,
    json::{self, Value, object},
};
use zcalld_plugin_api::{
    ContextPlugin, ContextPluginExport, GatewayPlugin, GatewayPluginExport, ResponsePlugin,
    ResponsePluginExport,
};
use zplugin::{Error, Result};
static INIT: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct RequestState {
    enabled: Cell<bool>,
    audit_failed: Cell<bool>,
    metadata: RefCell<Value>,
}
thread_local! {static ACTIVE:RefCell<HashMap<String,Rc<RequestState>>>=RefCell::new(HashMap::new());}
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Default)]
struct Redact {
    config: Value,
    active: Rc<RequestState>,
    host: Option<Host>,
    stream: u64,
    pending: Option<(Vec<u8>, bool, Vec<u8>)>,
}
fn unavailable<T>(_: T) -> Error {
    Error::PluginStatus(5)
}
impl Redact {
    fn configured(&mut self, config: &str) -> Result<()> {
        let mut value = json::parse(config.as_bytes()).map_err(|_| Error::InvalidArgument)?;
        let worker = value.get("worker").as_str().ok_or(Error::InvalidArgument)?;
        let worker = zcalld_compat_client::helpers::resolve_legacy(Path::new(worker));
        if !zcalld_compat_client::helpers::executable(&worker) {
            return Err(Error::InvalidArgument);
        }
        value.set("worker", worker.to_string_lossy().into_owned().into());
        for key in ["settings", "key"] {
            let path = value.get(key).as_str().ok_or(Error::InvalidArgument)?;
            if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
                return Err(Error::InvalidArgument);
            }
        }
        self.active = ACTIVE.with(|all| {
            all.borrow_mut()
                .entry(value.stringify())
                .or_default()
                .clone()
        });
        self.config = value;
        Ok(())
    }
    fn host(&mut self) -> Result<Host> {
        if let Some(h) = &self.host {
            return Ok(h.clone());
        }
        let _lock = INIT.lock().map_err(unavailable)?;
        let program = self
            .config
            .get("worker")
            .as_str()
            .ok_or(Error::InvalidArgument)?;
        let key = self.config.stringify();
        let (host, fresh) =
            Host::shared(&key, Path::new(program), &[], None).map_err(unavailable)?;
        if fresh {
            host.rpc(object([
                ("op", "configure".into()),
                ("config", self.config.clone()),
            ]))
            .map_err(unavailable)?;
        }
        self.host = Some(host.clone());
        Ok(host)
    }
    fn output(&mut self, input: &[u8], final_chunk: bool, output: &mut [u8]) -> Result<u64> {
        let pending = self.pending.as_ref().ok_or(Error::InvalidArgument)?;
        if pending.0 != input || pending.1 != final_chunk {
            return Err(Error::InvalidArgument);
        }
        if output.len() < pending.2.len() {
            return Err(Error::BufferTooSmall);
        }
        let count = pending.2.len();
        output[..count].copy_from_slice(&pending.2);
        self.pending = None;
        Ok(count as u64)
    }
    fn audit_failure(&self, failed: bool) {
        if failed {
            if !self.active.audit_failed.replace(true) {
                eprintln!("zcalld-redact: privacy audit history is unavailable");
            }
        } else {
            self.active.audit_failed.set(false);
        }
    }
    fn flush_audit(&self) {
        if let Some(host) = &self.host {
            let result = host.rpc(object([("op", "flush".into())]));
            self.audit_failure(!matches!(result, Ok(Value::Bool(true))));
        }
    }
    fn close(&mut self) {
        if self.stream != 0 {
            if let Some(host) = &self.host {
                let _ = host.rpc(object([
                    ("op", "close".into()),
                    ("stream", self.stream.into()),
                ]));
            }
            self.stream = 0;
        }
    }
}
impl Drop for Redact {
    fn drop(&mut self) {
        self.close();
        // stderr from the worker is intentionally isolated. Obtain an explicit
        // flush acknowledgement so shutdown-only audit errors reach the journal.
        self.flush_audit();
    }
}
impl GatewayPlugin for Redact {
    fn configure(&mut self, c: &str) -> Result<()> {
        self.configured(c)
    }
    fn route(&mut self, _: &str, _: &mut [u8]) -> Result<u64> {
        Ok(0)
    }
    fn request(&mut self, _: u32, input: &[u8], output: &mut [u8]) -> Result<u64> {
        if !self.pending.as_ref().is_some_and(|p| p.0 == input && !p.1) {
            self.pending = None;
            let metadata = self.active.metadata.borrow().clone();
            let result = self
                .host()?
                .rpc(object([
                    ("op", "mask".into()),
                    ("body", encode(input).into()),
                    ("context", metadata),
                ]))
                .map_err(unavailable)?;
            self.audit_failure(
                result
                    .get("audit_error")
                    .as_str()
                    .is_some_and(|error| !error.is_empty()),
            );
            self.active
                .enabled
                .set(result.get("enabled").as_bool().unwrap_or(true));
            if result.get("blocked").as_bool() == Some(true) {
                return Err(Error::PluginStatus(4));
            }
            let body = decode(result.get("body").as_str().ok_or(Error::InvalidArgument)?)
                .map_err(unavailable)?;
            if body == input {
                return Ok(0);
            }
            self.pending = Some((input.into(), false, body));
        }
        self.output(input, false, output)
    }
    fn observe(&mut self, _: u32, _: u64, _: u64, _: u64) -> Result<()> {
        self.pending = None;
        Ok(())
    }
}
impl ContextPlugin for Redact {
    fn configure(&mut self, config: &str) -> Result<()> {
        self.configured(config)
    }
    fn context(&mut self, metadata: &str) -> Result<()> {
        if metadata.len() > 16 * 1024 {
            return Err(Error::InvalidArgument);
        }
        let value = json::parse(metadata.as_bytes()).map_err(|_| Error::InvalidArgument)?;
        if value.get("phase").as_str() == Some("route") {
            return Ok(());
        }
        if value.get("phase").as_str() != Some("request") {
            return Err(Error::InvalidArgument);
        }
        let mut kept = object([]);
        for (source, target, maximum) in [
            ("agent", "agent", 512),
            ("session", "session", 1024),
            ("path", "what", 2048),
        ] {
            match value.get(source) {
                Value::Null => {}
                Value::String(text)
                    if text.len() <= maximum && !text.chars().any(char::is_control) =>
                {
                    kept.set(target, text.as_str().into());
                }
                _ => return Err(Error::InvalidArgument),
            }
        }
        // Retain only bounded audit labels. Raw prompts, authentication, peer
        // details and workspace paths never enter the history metadata.
        *self.active.metadata.borrow_mut() = kept;
        Ok(())
    }
}

impl ResponsePlugin for Redact {
    fn configure(&mut self, c: &str) -> Result<()> {
        self.configured(c)
    }
    fn begin(&mut self, _: u32, content_type: &str) -> Result<()> {
        self.close();
        self.pending = None;
        self.stream = NEXT.fetch_add(1, Ordering::Relaxed);
        let stream = self.stream;
        let enabled = self.active.enabled.get();
        self.host()?
            .rpc(object([
                ("op", "open".into()),
                ("stream", stream.into()),
                ("content_type", content_type.into()),
                ("enabled", enabled.into()),
            ]))
            .map_err(unavailable)?;
        Ok(())
    }
    fn write(&mut self, input: &[u8], final_chunk: u32, output: &mut [u8]) -> Result<u64> {
        if final_chunk > 1 || self.stream == 0 {
            return Err(Error::InvalidArgument);
        }
        let final_chunk = final_chunk == 1;
        if !self
            .pending
            .as_ref()
            .is_some_and(|p| p.0 == input && p.1 == final_chunk)
        {
            self.pending = None;
            let stream = self.stream;
            let result = self
                .host()?
                .rpc(object([
                    ("op", "write".into()),
                    ("stream", stream.into()),
                    ("body", encode(input).into()),
                    ("final", final_chunk.into()),
                ]))
                .map_err(unavailable)?;
            let body = match result.get("body") {
                Value::Null => Vec::new(),
                v => decode(v.as_str().ok_or(Error::InvalidArgument)?).map_err(unavailable)?,
            };
            self.pending = Some((input.into(), final_chunk, body));
        }
        let count = self.output(input, final_chunk, output)?;
        if final_chunk {
            self.stream = 0;
        }
        Ok(count)
    }
}
fn create() -> Result<Redact> {
    Ok(Redact::default())
}
zplugin::export_interfaces!(
 version=(zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MAJOR")),zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MINOR")),zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_PATCH"))),unload=pinned;
 GatewayPluginExport<Redact> => create,
 ContextPluginExport<Redact> => create,
 ResponsePluginExport<Redact> => create,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_context_keeps_only_bounded_labels() {
        let mut plugin = Redact::default();
        ContextPlugin::context(&mut plugin, r#"{"phase":"request","agent":"fixture-agent","session":"fixture-session","path":"/v1/responses","model":"public/model","body":"private prompt","authorization":"private token","cwd":"/private/workspace","peer":{"uid":1000}}"#).unwrap();
        assert_eq!(
            *plugin.active.metadata.borrow(),
            object([
                ("agent", "fixture-agent".into()),
                ("session", "fixture-session".into()),
                ("what", "/v1/responses".into()),
            ])
        );
        // A routing-stage callback must not replace the request's audit labels.
        ContextPlugin::context(
            &mut plugin,
            r#"{"phase":"route","body":"filtered request","model":"different"}"#,
        )
        .unwrap();
        assert_eq!(
            plugin.active.metadata.borrow().get("agent").as_str(),
            Some("fixture-agent")
        );
    }

    #[test]
    fn audit_context_rejects_unbounded_or_malformed_labels() {
        let mut plugin = Redact::default();
        for value in [
            object([
                ("phase", "request".into()),
                ("agent", "x".repeat(513).into()),
            ]),
            object([
                ("phase", "request".into()),
                ("session", "line\nbreak".into()),
            ]),
            object([("phase", "request".into()), ("path", 123u64.into())]),
            object([("phase", "unknown".into())]),
        ] {
            assert!(ContextPlugin::context(&mut plugin, &value.stringify()).is_err());
        }
        assert!(ContextPlugin::context(&mut plugin, &" ".repeat(16 * 1024 + 1)).is_err());
    }
}
