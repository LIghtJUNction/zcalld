//! Optional native desktop notifications and activity inhibition.
//! Loading, configuring and inspecting the plugin never starts an OS action.
mod config;
mod event;
mod hub;
mod logic;
mod os;
mod private;
mod scan;
use std::sync::{Arc, atomic::Ordering};
use zcalld_compat_client::json;
use zcalld_plugin_api::{
    EventPlugin, EventPluginExport, GatewayPlugin, GatewayPluginExport, events,
};
use zplugin::{Error, OwnedResource, ResourceHandler, Result};

#[derive(Default)]
struct Desktop {
    hub: Option<Arc<hub::Hub>>,
}
fn bad<T>(_: T) -> Error {
    Error::InvalidArgument
}
impl Desktop {
    fn configure_inner(&mut self, input: &str) -> Result<()> {
        let config = config::Config::parse(input).map_err(bad)?;
        let next = hub::Hub::shared(config).map_err(bad)?;
        self.hub = Some(next);
        Ok(())
    }
    fn hub(&self) -> Result<&Arc<hub::Hub>> {
        self.hub.as_ref().ok_or(Error::InvalidArgument)
    }
}
impl EventPlugin for Desktop {
    fn configure(&mut self, input: &str) -> Result<()> {
        self.configure_inner(input)
    }
    fn begin(&mut self, metadata: &str) -> Result<OwnedResource> {
        if metadata.len() > events::MAX_EVENT_BYTES {
            return Err(Error::InvalidArgument);
        }
        let v = json::parse(metadata.as_bytes()).map_err(bad)?;
        if v.get("version").as_u64() != Some(1) {
            return Err(Error::InvalidArgument);
        }
        let p = v
            .get("protocol")
            .as_u64()
            .filter(|n| *n <= u32::MAX as u64)
            .ok_or(Error::InvalidArgument)? as u32;
        let hub = self.hub()?.clone();
        let active = hub.config.enabled;
        let mut turn = None;
        if active {
            hub.activate();
            hub.state.active.fetch_add(1, Ordering::AcqRel);
            if hub.config.turn_done_seconds > 0
                && v.get("kind").as_str() == Some("generation")
                && v.get("subagent").as_bool() != Some(true)
            {
                let agent = config::text(v.get("agent"), 128).unwrap_or("");
                let identity = config::text(v.get("session"), 256)
                    .filter(|s| !s.is_empty())
                    .or_else(|| config::text(v.get("conversation"), 256).filter(|s| !s.is_empty()));
                if known_agent(agent)
                    && let Some(id) = identity
                    && let Ok(mut turns) = hub.state.turns.lock()
                {
                    // Length separators avoid concatenation ambiguity; neither identifier is rendered.
                    turn = turns.begin(format!("{}:{agent}:{id}", agent.len()), hub.state.now());
                }
            }
            hub.state.wake();
        }
        OwnedResource::new(Lease {
            hub,
            active,
            turn,
            scan: scan::Scan::new(p),
            finished: false,
            delivered: false,
        })
    }
    fn emit(&mut self, kind: u32, input: &str) -> Result<u32> {
        let hub = self.hub()?;
        if kind == events::CONTROL_ACTIVATE {
            if input != "{}" {
                return Err(Error::InvalidArgument);
            }
            return Ok(hub.activate());
        }
        match event::parse(kind, input.as_bytes()) {
            Ok(event) => Ok(hub.emit(event)),
            Err(_) => {
                hub.state.invalid.fetch_add(1, Ordering::Relaxed);
                Err(Error::InvalidArgument)
            }
        }
    }
    fn status(&mut self, output: &mut [u8]) -> Result<u64> {
        let text = self.hub()?.status().stringify();
        if output.len() < text.len() {
            return Err(Error::BufferTooSmall);
        }
        output[..text.len()].copy_from_slice(text.as_bytes());
        Ok(text.len() as u64)
    }
}
impl GatewayPlugin for Desktop {
    fn configure(&mut self, input: &str) -> Result<()> {
        self.configure_inner(input)
    }
    fn route(&mut self, _: &str, _: &mut [u8]) -> Result<u64> {
        Ok(0)
    }
    fn request(&mut self, _: u32, _: &[u8], _: &mut [u8]) -> Result<u64> {
        Ok(0)
    }
    fn observe(&mut self, _: u32, _: u64, _: u64, _: u64) -> Result<()> {
        Ok(())
    }
}
struct Lease {
    hub: Arc<hub::Hub>,
    active: bool,
    turn: Option<String>,
    scan: scan::Scan,
    finished: bool,
    delivered: bool,
}
impl ResourceHandler for Lease {
    const ID: &'static str = events::REQUEST_TYPE;
    const VERSION: u32 = events::REQUEST_VERSION;
    fn call(&mut self, operation: u32, input: &[u8], _: &mut [u8]) -> Result<usize> {
        if self.finished {
            return if operation == events::FINISH {
                Ok(0)
            } else {
                Err(Error::InvalidArgument)
            };
        }
        match operation {
            events::HEAD => {
                if input.len() > events::MAX_EVENT_BYTES {
                    return Err(Error::InvalidArgument);
                }
                let v = json::parse(input).map_err(bad)?;
                let p = v
                    .get("protocol")
                    .as_u64()
                    .filter(|n| *n <= u32::MAX as u64)
                    .ok_or(Error::InvalidArgument)? as u32;
                let status = v.get("status").as_u64().ok_or(Error::InvalidArgument)?;
                let stream = v.get("stream").as_bool().ok_or(Error::InvalidArgument)?;
                let mime =
                    config::text(v.get("content_type"), 256).ok_or(Error::InvalidArgument)?;
                self.scan.head(p, status, stream, mime);
                Ok(0)
            }
            events::BODY => {
                if self.turn.is_some() {
                    self.scan.push(input);
                }
                Ok(0)
            }
            events::FINISH => {
                if input.len() > events::MAX_EVENT_BYTES {
                    return Err(Error::InvalidArgument);
                }
                let v = json::parse(input).map_err(bad)?;
                self.delivered = v.get("delivered").as_bool() == Some(true)
                    && v.get("status")
                        .as_u64()
                        .is_some_and(|n| (200..300).contains(&n));
                self.finished = true;
                Ok(0)
            }
            _ => Err(Error::Unsupported),
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let s = &self.hub.state;
        let now = s.now();
        let final_reply = self.finished && self.delivered && self.scan.completed();
        if final_reply {
            s.completed.fetch_add(1, Ordering::Relaxed);
        }
        if self.turn.is_some() && !final_reply {
            s.suppressed.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(key) = &self.turn
            && let Ok(mut turns) = s.turns.lock()
        {
            turns.end(key, now, final_reply);
        }
        s.last_end.store(now, Ordering::Release);
        s.had_activity.store(true, Ordering::Release);
        s.active.fetch_sub(1, Ordering::AcqRel);
        s.wake();
    }
}
fn known_agent(agent: &str) -> bool {
    matches!(
        agent,
        "codex"
            | "claude"
            | "opencode"
            | "gemini"
            | "qwen"
            | "pi"
            | "aider"
            | "goose"
            | "amp"
            | "kilo"
            | "cursor"
            | "vscode"
    )
}
fn create() -> Result<Desktop> {
    Ok(Desktop::default())
}
zplugin::export_interfaces!(
    version=(zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MAJOR")),zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MINOR")),zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_PATCH"))),unload=pinned;
    GatewayPluginExport<Desktop> => create,
    EventPluginExport<Desktop> => create,
);
#[cfg(test)]
mod tests;
