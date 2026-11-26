//! Explicit native-plugin loading and bounded, ordered hook execution.

use std::{path::PathBuf, rc::Rc};
use zcalld_plugin_api::{
    ContextPluginApi, ContextPluginProxy, GatewayPluginApi, GatewayPluginProxy, MAX_MODEL_BYTES,
    ProviderPluginApi, ProviderPluginProxy, ResponsePluginApi, ResponsePluginProxy, RetryPluginApi,
    RetryPluginProxy, protocol, status,
};
use zcalld_plugin_api::{EventPluginApi, EventPluginProxy, HttpPluginApi, HttpPluginProxy, events};
use zcalld_plugin_api::{RecoveryPluginApi, RecoveryPluginProxy, recovery};
use zplugin::{Error, Plugin, UnloadPolicy};

const MAX_PLUGINS: usize = 32;
const MAX_CONFIG_BYTES: usize = 65_536;
const REQUEST_BUFFER_HEADROOM: usize = 2048;

#[derive(Clone)]
pub struct PluginSpec {
    pub path: PathBuf,
    pub config: String,
    pub provider_id: Option<String>,
}

impl std::fmt::Debug for PluginSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginSpec")
            .field("path", &self.path)
            .field("config_bytes", &self.config.len())
            .finish()
    }
}

struct Entry {
    path: PathBuf,
    provider_id: Option<String>,
    proxy: GatewayPluginProxy<'static>,
    context: Option<ContextPluginProxy<'static>>,
    response: Option<ResponsePluginProxy<'static>>,
    provider: Option<ProviderPluginProxy<'static>>,
    retry: Option<RetryPluginProxy<'static>>,
    http: Option<HttpPluginProxy<'static>>,
    events: Option<EventPluginProxy<'static>>,
    recovery: Option<RecoveryPluginProxy<'static>>,
    recovery_executor: bool,
}

#[derive(Clone, Copy)]
pub struct HttpHandler {
    index: usize,
    pub route_context: bool,
}

fn http_decision(flags: u32) -> Result<Option<bool>, String> {
    match flags {
        0 => Ok(None),
        1 => Ok(Some(false)),
        3 => Ok(Some(true)),
        _ => Err("HTTP plugin returned an invalid route decision".into()),
    }
}

pub struct RetryPlan {
    pub model: String,
    pub body: Option<Vec<u8>>,
}

/// Each worker constructs and owns this value on its own thread.
///
/// zplugin's owning proxies retain their libraries and are deliberately neither
/// Send nor Sync. Native library images stay pinned until process termination.
pub struct Plugins {
    entries: Vec<Entry>,
}

impl Plugins {
    pub fn load(specs: &[PluginSpec]) -> Result<Self, String> {
        if specs.len() > MAX_PLUGINS {
            return Err(format!("at most {MAX_PLUGINS} plugins may be configured"));
        }
        let mut entries = Vec::with_capacity(specs.len());
        for spec in specs {
            if !spec.path.is_absolute() {
                return Err("plugin path must be absolute".into());
            }
            if spec.config.len() > MAX_CONFIG_BYTES {
                return Err(format!(
                    "plugin configuration exceeds {MAX_CONFIG_BYTES} bytes"
                ));
            }
            let path = spec
                .path
                .canonicalize()
                .map_err(|error| format!("plugin {}: {error}", spec.path.display()))?;
            if !path.is_file() {
                return Err(format!("plugin {} is not a regular file", path.display()));
            }
            // SAFETY: only operator-configured trusted native code is loaded.
            // zplugin validates the ABI; the owning proxy retains the image.
            let library = Rc::new(
                unsafe { Plugin::load_path(&path, UnloadPolicy::Pinned) }
                    .map_err(|error| hook_error(&path, "load", error))?,
            );
            let instance = Plugin::create_owned(&library, GatewayPluginApi::SPEC)
                .map_err(|error| hook_error(&path, "create", error))?;
            let mut proxy = GatewayPluginApi::wrap(instance)
                .map_err(|error| hook_error(&path, "bind", error))?;
            proxy
                .configure(&spec.config)
                .map_err(|error| hook_error(&path, "configure", error))?;
            let context = match Plugin::create_owned(&library, ContextPluginApi::SPEC) {
                Ok(instance) => {
                    let mut p = ContextPluginApi::wrap(instance)
                        .map_err(|e| hook_error(&path, "context bind", e))?;
                    p.configure(&spec.config)
                        .map_err(|e| hook_error(&path, "context configure", e))?;
                    Some(p)
                }
                Err(Error::InterfaceNotFound) => None,
                Err(e) => return Err(hook_error(&path, "context create", e)),
            };
            let response = match Plugin::create_owned(&library, ResponsePluginApi::SPEC) {
                Ok(instance) => {
                    let mut p = ResponsePluginApi::wrap(instance)
                        .map_err(|e| hook_error(&path, "response bind", e))?;
                    p.configure(&spec.config)
                        .map_err(|e| hook_error(&path, "response configure", e))?;
                    Some(p)
                }
                Err(Error::InterfaceNotFound) => None,
                Err(e) => return Err(hook_error(&path, "response create", e)),
            };
            let provider = match Plugin::create_owned(&library, ProviderPluginApi::SPEC) {
                Ok(instance) => {
                    let mut p = ProviderPluginApi::wrap(instance)
                        .map_err(|e| hook_error(&path, "provider bind", e))?;
                    p.configure(&spec.config)
                        .map_err(|e| hook_error(&path, "provider configure", e))?;
                    Some(p)
                }
                Err(Error::InterfaceNotFound) => None,
                Err(e) => return Err(hook_error(&path, "provider create", e)),
            };
            let retry = match Plugin::create_owned(&library, RetryPluginApi::SPEC) {
                Ok(instance) => {
                    let mut p = RetryPluginApi::wrap(instance)
                        .map_err(|e| hook_error(&path, "retry bind", e))?;
                    p.configure(&spec.config)
                        .map_err(|e| hook_error(&path, "retry configure", e))?;
                    Some(p)
                }
                Err(Error::InterfaceNotFound) => None,
                Err(e) => return Err(hook_error(&path, "retry create", e)),
            };
            let http = match Plugin::create_owned(&library, HttpPluginApi::SPEC) {
                Ok(instance) => {
                    let mut p = HttpPluginApi::wrap(instance)
                        .map_err(|e| hook_error(&path, "HTTP bind", e))?;
                    p.configure(&spec.config)
                        .map_err(|e| hook_error(&path, "HTTP configure", e))?;
                    Some(p)
                }
                Err(Error::InterfaceNotFound) => None,
                Err(e) => return Err(hook_error(&path, "HTTP create", e)),
            };
            let events = match Plugin::create_owned(&library, EventPluginApi::SPEC) {
                Ok(instance) => {
                    let mut p = EventPluginApi::wrap(instance)
                        .map_err(|e| hook_error(&path, "events bind", e))?;
                    p.configure(&spec.config)
                        .map_err(|e| hook_error(&path, "events configure", e))?;
                    Some(p)
                }
                Err(Error::InterfaceNotFound) => None,
                Err(e) => return Err(hook_error(&path, "events create", e)),
            };
            let (recovery, recovery_executor) =
                match Plugin::create_owned(&library, RecoveryPluginApi::SPEC) {
                    Ok(instance) => {
                        let mut p = RecoveryPluginApi::wrap(instance)
                            .map_err(|e| hook_error(&path, "recovery bind", e))?;
                        p.configure(&spec.config)
                            .map_err(|e| hook_error(&path, "recovery configure", e))?;
                        let executor = match p
                            .accepts()
                            .map_err(|e| hook_error(&path, "recovery capability", e))?
                        {
                            0 => false,
                            1 => true,
                            _ => return Err("invalid recovery capability".into()),
                        };
                        (Some(p), executor)
                    }
                    Err(Error::InterfaceNotFound) => (None, false),
                    Err(e) => return Err(hook_error(&path, "recovery create", e)),
                };
            if recovery_executor && entries.iter().any(|e: &Entry| e.recovery_executor) {
                return Err("only one provider recovery executor may be configured".into());
            }
            if spec.provider_id.is_some() && provider.is_none() {
                return Err(format!(
                    "{} has no provider transport interface",
                    path.display()
                ));
            }
            if provider.is_some()
                && entries
                    .iter()
                    .any(|e: &Entry| e.provider.is_some() && e.provider_id == spec.provider_id)
            {
                return Err("duplicate provider plugin binding".into());
            }
            entries.push(Entry {
                path,
                provider_id: spec.provider_id.clone(),
                proxy,
                context,
                response,
                provider,
                retry,
                http,
                events,
                recovery,
                recovery_executor,
            });
        }
        Ok(Self { entries })
    }

    /// Static validation only; no activation or inbox consumption.
    pub fn event_status(&mut self) -> Result<Vec<String>, String> {
        let mut result = Vec::new();
        for entry in &mut self.entries {
            if let Some(plugin) = &mut entry.events {
                let mut bytes = vec![0u8; events::MAX_EVENT_BYTES];
                let count = plugin
                    .status(&mut bytes)
                    .map_err(|e| hook_error(&entry.path, "events status", e))?;
                let count = usize::try_from(count)
                    .ok()
                    .filter(|n| *n <= bytes.len())
                    .ok_or("event plugin returned an invalid status length")?;
                bytes.truncate(count);
                result.push(String::from_utf8(bytes).map_err(|_| "event status is not UTF-8")?);
            }
        }
        Ok(result)
    }

    pub fn has_events(&self) -> bool {
        self.entries.iter().any(|entry| entry.events.is_some())
    }

    /// Notification failures cannot turn a successful generation into an error.
    pub fn emit_event(&mut self, kind: u32, event: &str) {
        if event.len() > events::MAX_EVENT_BYTES {
            crate::events::diagnostic("oversized event");
            return;
        }
        for entry in &mut self.entries {
            if let Some(plugin) = &mut entry.events {
                match plugin.emit(kind, event) {
                    Ok(0..=2) => {}
                    _ => crate::events::diagnostic("event delivery"),
                }
            }
        }
    }

    pub fn begin_events(&mut self, metadata: &str) -> crate::events::Leases {
        let mut leases = crate::events::Leases::default();
        if metadata.len() > events::MAX_EVENT_BYTES {
            crate::events::diagnostic("oversized request metadata");
            return leases;
        }
        for entry in &mut self.entries {
            if let Some(plugin) = &mut entry.events {
                match plugin.begin(metadata) {
                    Ok(resource) => {
                        if resource
                            .check(events::REQUEST_TYPE, events::REQUEST_VERSION)
                            .is_ok()
                        {
                            leases.push(resource);
                        } else {
                            crate::events::diagnostic("invalid request lease");
                        }
                    }
                    Err(_) => crate::events::diagnostic("request lease"),
                }
            }
        }
        leases
    }

    /// Initialize optional post-conversion response hooks for one request.
    pub fn response_begin(&mut self, status: u32, content_type: &str) -> Result<(), String> {
        for entry in &mut self.entries {
            if let Some(response) = &mut entry.response {
                response
                    .begin(status, content_type)
                    .map_err(|e| hook_error(&entry.path, "response begin", e))?;
            }
        }
        Ok(())
    }

    /// A Some(empty) output holds a partial placeholder until more input arrives.
    pub fn response_write(
        &mut self,
        input: &[u8],
        final_chunk: bool,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut changed: Option<Vec<u8>> = None;
        for entry in self.entries.iter_mut().rev() {
            let Some(response) = &mut entry.response else {
                continue;
            };
            let body = changed.as_deref().unwrap_or(input);
            let mut output = vec![0; body.len().saturating_add(1024).min(max_bytes)];
            let count = loop {
                match response.write(body, u32::from(final_chunk), &mut output) {
                    Ok(n) => break n,
                    Err(Error::BufferTooSmall) if output.len() < max_bytes => {
                        output.resize(output.len().max(256).saturating_mul(2).min(max_bytes), 0);
                    }
                    Err(e) => return Err(hook_error(&entry.path, "response write", e)),
                }
            };
            let count = checked_count(count, output.len(), &entry.path)?;
            output.truncate(count);
            changed = Some(output);
        }
        Ok(changed)
    }

    /// Resolve an optional endpoint before allocating any HTTP metadata.
    pub fn http_handler(&mut self, path: &str) -> Result<Option<HttpHandler>, String> {
        let mut selected = None;
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let Some(http) = &mut entry.http else {
                continue;
            };
            let flags = http
                .accepts(path)
                .map_err(|e| hook_error(&entry.path, "HTTP route", e))?;
            if let Some(route_context) = http_decision(flags)? {
                if selected.is_some() {
                    return Err("multiple plugins claim the same HTTP endpoint".into());
                }
                selected = Some(HttpHandler {
                    index,
                    route_context,
                });
            }
        }
        Ok(selected)
    }

    pub fn http_fetch(
        &mut self,
        handler: HttpHandler,
        metadata: &str,
        body: &[u8],
    ) -> Result<ProviderResponse, String> {
        let entry = self
            .entries
            .get_mut(handler.index)
            .ok_or("unknown HTTP plugin")?;
        let http = entry.http.as_mut().ok_or("HTTP plugin is unavailable")?;
        let resource = http
            .handle(metadata, body)
            .map_err(|e| hook_error(&entry.path, "HTTP request", e))?;
        resource
            .check("dev.zcalld.provider-stream", 1)
            .map_err(|e| hook_error(&entry.path, "HTTP stream", e))?;
        Ok(ProviderResponse {
            resource,
            ended: false,
        })
    }

    /// Exact native bindings take precedence over the existing community bridge.
    pub fn provider_fetch(
        &mut self,
        id: &str,
        request_json: &[u8],
    ) -> Result<ProviderResponse, String> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.provider_id.as_deref() == Some(id))
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|entry| entry.provider_id.is_none() && entry.provider.is_some())
            })
            .ok_or("no provider transport is configured for this plugin")?;
        let entry = &mut self.entries[index];
        let provider = entry
            .provider
            .as_mut()
            .ok_or("provider transport unavailable")?;
        let resource = provider
            .fetch(request_json)
            .map_err(|error| hook_error(&entry.path, "provider fetch", error))?;
        resource
            .check("dev.zcalld.provider-stream", 1)
            .map_err(|error| hook_error(&entry.path, "provider stream", error))?;
        Ok(ProviderResponse {
            resource,
            ended: false,
        })
    }

    pub fn has_context(&self) -> bool {
        self.entries.iter().any(|entry| entry.context.is_some())
    }

    pub fn has_retry(&self) -> bool {
        self.entries.iter().any(|entry| entry.retry.is_some())
    }

    pub fn has_recovery(&self) -> bool {
        self.entries.iter().any(|entry| entry.recovery_executor)
    }

    pub fn begin_recovery(&mut self, metadata: &[u8]) -> Result<Option<RecoveryResponse>, String> {
        if metadata.len() > recovery::MAX_METADATA_BYTES {
            return Err("recovery metadata exceeds limit".into());
        }
        for entry in &mut self.entries {
            if !entry.recovery_executor {
                continue;
            }
            let executor = entry.recovery.as_mut().expect("validated executor");
            let resource = executor
                .begin(metadata)
                .map_err(|e| hook_error(&entry.path, "recovery begin", e))?;
            resource
                .check(recovery::RESOURCE_ID, recovery::RESOURCE_VERSION)
                .map_err(|e| hook_error(&entry.path, "recovery resource", e))?;
            return Ok(Some(RecoveryResponse { resource }));
        }
        Ok(None)
    }

    /// Only route observers adjust an already filtered request after recovery.
    pub fn resume_recovery(
        &mut self,
        protocol: u32,
        model: &str,
        body: &[u8],
        max_body: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        validate_model(model)?;
        if !protocol::is_supported(protocol) || max_body == 0 || body.len() > max_body {
            return Err("invalid recovered request".into());
        }
        let mut current: Option<Vec<u8>> = None;
        for entry in &mut self.entries {
            let Some(observer) = &mut entry.recovery else {
                continue;
            };
            let input = current.as_deref().unwrap_or(body);
            let mut output = Vec::new();
            let count = loop {
                match observer.resume(protocol, model, input, &mut output) {
                    Ok(n) => break n,
                    Err(Error::BufferTooSmall) if output.len() < max_body => {
                        let next = if output.is_empty() {
                            input.len().saturating_add(2048).max(256).min(max_body)
                        } else {
                            output.len().saturating_mul(2).min(max_body)
                        };
                        output.resize(next, 0);
                    }
                    Err(e) => return Err(hook_error(&entry.path, "recovery resume", e)),
                }
            };
            let count = checked_count(count, output.len(), &entry.path)?;
            if count != 0 {
                output.truncate(count);
                current = Some(output);
            }
        }
        Ok(current)
    }

    /// The first willing retry controller selects an alternative and adjusts
    /// only the already filtered request. General middleware is never replayed.
    pub fn retry(
        &mut self,
        protocol: u32,
        status: u32,
        previous_model: &str,
        body: &[u8],
        max_body: usize,
    ) -> Result<Option<RetryPlan>, String> {
        if !matches!(status, 429 | 503) {
            return Ok(None);
        }
        validate_model(previous_model)?;
        if !protocol::is_supported(protocol) || max_body == 0 || body.len() > max_body {
            return Err("invalid protocol or request size for group retry".into());
        }
        for entry in &mut self.entries {
            let Some(retry) = &mut entry.retry else {
                continue;
            };
            let mut selected = [0_u8; MAX_MODEL_BYTES];
            let count = retry
                .retry(status, previous_model, &mut selected)
                .map_err(|e| hook_error(&entry.path, "retry route", e))?;
            let count = checked_count(count, selected.len(), &entry.path)?;
            if count == 0 {
                continue;
            }
            let model = std::str::from_utf8(&selected[..count])
                .map_err(|_| "retry plugin returned an invalid UTF-8 model")?;
            validate_model(model)?;
            let mut output = Vec::new();
            let count = loop {
                match retry.request(protocol, body, &mut output) {
                    Ok(n) => break n,
                    Err(Error::BufferTooSmall) if output.len() < max_body => {
                        let next = if output.is_empty() {
                            body.len().saturating_add(1024).max(256).min(max_body)
                        } else {
                            output.len().saturating_mul(2).min(max_body)
                        };
                        output.resize(next, 0);
                    }
                    Err(e) => return Err(hook_error(&entry.path, "retry request", e)),
                }
            };
            let count = checked_count(count, output.len(), &entry.path)?;
            output.truncate(count);
            return Ok(Some(RetryPlan {
                model: model.to_owned(),
                body: (count != 0).then_some(output),
            }));
        }
        Ok(None)
    }

    pub fn context(&mut self, metadata: &str) -> Result<(), String> {
        if metadata.len() > 16 * 1024 * 1024 + 16384 {
            return Err("plugin context exceeds limit".into());
        }
        for entry in &mut self.entries {
            if let Some(context) = &mut entry.context {
                context
                    .context(metadata)
                    .map_err(|e| hook_error(&entry.path, "request context", e))?;
            }
        }
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Apply each configured routing hook once, in configuration order.
    pub fn route(&mut self, model: &str) -> Result<String, String> {
        validate_model(model)?;
        let mut selected = model.to_owned();
        let mut output = [0_u8; MAX_MODEL_BYTES];
        for entry in &mut self.entries {
            let count = entry
                .proxy
                .route(&selected, &mut output)
                .map_err(|error| hook_error(&entry.path, "route", error))?;
            if count == 0 {
                continue;
            }
            let count = checked_count(count, output.len(), &entry.path)?;
            let replacement = std::str::from_utf8(&output[..count]).map_err(|_| {
                format!(
                    "plugin {} returned an invalid UTF-8 model",
                    entry.path.display()
                )
            })?;
            validate_model(replacement)?;
            selected.clear();
            selected.push_str(replacement);
        }
        Ok(selected)
    }

    /// Keep the original body borrowed unless a plugin returns a replacement.
    ///
    /// Unchanged hooks receive an empty output buffer and allocate nothing.
    /// Transforming hooks may request a larger buffer with BufferTooSmall.
    pub fn request(
        &mut self,
        protocol: u32,
        body: &[u8],
        max_body: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        if !protocol::is_supported(protocol) {
            return Err("unsupported plugin protocol number".into());
        }
        if max_body == 0 || body.len() > max_body {
            return Err("request exceeds gateway body limit".into());
        }
        let mut changed: Option<Vec<u8>> = None;
        let mut output = Vec::new();
        for entry in &mut self.entries {
            request_hook(
                body,
                &mut changed,
                &mut output,
                max_body,
                &entry.path,
                |input, output| entry.proxy.request(protocol, input, output),
            )?;
        }
        if let Some(body) = &mut changed {
            // A shrinking transform must not retain a previous, much larger
            // request allocation for the lifetime of an upstream generation.
            body.shrink_to(body.len().saturating_add(REQUEST_BUFFER_HEADROOM));
        }
        Ok(changed)
    }

    /// Notify every observer; return the first error for the host's diagnostics.
    pub fn observe(
        &mut self,
        status: u32,
        elapsed_ms: u64,
        input_bytes: u64,
        output_bytes: u64,
    ) -> Result<(), String> {
        let mut first_error = None;
        for entry in &mut self.entries {
            if let Err(error) = entry
                .proxy
                .observe(status, elapsed_ms, input_bytes, output_bytes)
            {
                first_error.get_or_insert_with(|| hook_error(&entry.path, "observe", error));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn validate_model(model: &str) -> Result<(), String> {
    if model.is_empty() || model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control) {
        return Err(format!(
            "model must contain 1..={MAX_MODEL_BYTES} UTF-8 bytes without control characters"
        ));
    }
    Ok(())
}

fn request_hook(
    body: &[u8],
    changed: &mut Option<Vec<u8>>,
    output: &mut Vec<u8>,
    max_body: usize,
    path: &std::path::Path,
    mut request: impl FnMut(&[u8], &mut [u8]) -> Result<u64, Error>,
) -> Result<(), String> {
    let input = changed.as_deref().unwrap_or(body);
    let count = loop {
        match request(input, output) {
            Ok(count) => break count,
            Err(Error::BufferTooSmall) if output.len() < max_body => {
                let capacity = if output.is_empty() {
                    input
                        .len()
                        .saturating_add(REQUEST_BUFFER_HEADROOM)
                        .min(max_body)
                } else {
                    output.len().saturating_mul(2).min(max_body)
                };
                output.resize(capacity, 0);
            }
            Err(error) => return Err(hook_error(path, "request", error)),
        }
    };
    if count > 0 {
        let count = checked_count(count, output.len(), path)?;
        output.truncate(count);
        // Transfer ownership of the initialized bytes instead of cloning the
        // entire request. The previous result becomes scratch for a later hook;
        // input and output remain distinct throughout every plugin call.
        *output = changed.replace(std::mem::take(output)).unwrap_or_default();
    }
    Ok(())
}

fn checked_count(count: u64, capacity: usize, path: &std::path::Path) -> Result<usize, String> {
    let count = usize::try_from(count).map_err(|_| {
        format!(
            "plugin {} returned an invalid output length",
            path.display()
        )
    })?;
    if count > capacity {
        return Err(format!(
            "plugin {} exceeded its output buffer",
            path.display()
        ));
    }
    Ok(count)
}

fn hook_error(path: &std::path::Path, hook: &str, error: Error) -> String {
    let reason = match error {
        Error::PluginStatus(status::MODEL_DENIED) => "model denied by plugin policy".to_owned(),
        Error::PluginStatus(status::REQUEST_TOO_LARGE) => {
            "request exceeds plugin body limit".to_owned()
        }
        Error::PluginStatus(4) => "request blocked by privacy policy".into(),
        Error::PluginStatus(5) => "privacy filter unavailable; request held back".into(),
        Error::PluginStatus(6) => "community plugin unavailable; request held back".into(),
        Error::PluginStatus(7) => "all group members unavailable; request held back".into(),
        Error::PluginStatus(8) => "routing control or observer failed; request held back".into(),
        _ => error.to_string(),
    };
    format!("plugin {} {hook}: {reason}", path.display())
}

// Provider errors cross an untrusted transport boundary. Only these fixed
// categories reach clients; arbitrary worker messages can contain credentials.
fn provider_stream_error(error: Error) -> &'static str {
    match error {
        Error::PluginStatus(status::PROVIDER_REQUEST_FAILED) => "provider request failed",
        Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED) => {
            "provider worker or transport ended"
        }
        Error::PluginStatus(status::PROVIDER_TIMED_OUT) => "provider request timed out",
        Error::PluginStatus(status::PROVIDER_HEAD_INVALID) => "invalid provider response metadata",
        _ => "provider stream resource failed",
    }
}

/// Worker-local native stream. Drop cancels upstream work even on client failure.
pub struct ProviderResponse {
    resource: zplugin::Resource<'static>,
    ended: bool,
}
impl ProviderResponse {
    pub fn poll_head(&mut self) -> Result<Option<ProviderHead>, String> {
        let mut output = [0_u8; 16_384];
        let count = match self.resource.call(1, &[], &mut output) {
            Ok(n) if n <= output.len() => n,
            Err(Error::PluginStatus(3)) => return Ok(None),
            Err(Error::BufferTooSmall) => return Err("invalid provider response metadata".into()),
            Err(error) => return Err(provider_stream_error(error).into()),
            Ok(_) => return Err("invalid provider response metadata".into()),
        };
        let value = crate::state_json::parse(&output[..count])
            .map_err(|_| "invalid provider response metadata")?;
        let status = value
            .get("status")
            .as_u64()
            .filter(|n| (200..=599).contains(n))
            .ok_or("invalid provider response status")? as u16;
        let values = value
            .get("headers")
            .as_object()
            .ok_or("invalid provider response headers")?;
        let mut headers = Vec::with_capacity(values.len());
        for (name, value) in values {
            let value = value.as_str().ok_or("invalid provider header value")?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                || value.bytes().any(|b| b < 32 || b == 127)
            {
                return Err("invalid provider response header".into());
            }
            headers.push((name.clone(), value.into()));
        }
        Ok(Some(ProviderHead { status, headers }))
    }
}
impl std::io::Read for ProviderResponse {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.ended {
            return Ok(0);
        }
        let count = match self.resource.call(2, &[], output) {
            Ok(n) if n <= output.len() => n,
            Err(Error::PluginStatus(3)) => return Err(std::io::ErrorKind::WouldBlock.into()),
            Err(error) => return Err(std::io::Error::other(provider_stream_error(error))),
            Ok(_) => return Err(std::io::Error::other("invalid community provider output")),
        };
        if count == 0 {
            self.ended = true;
        }
        Ok(count)
    }
}
impl Drop for ProviderResponse {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.resource.call(3, &[], &mut []);
        }
    }
}

/// Drop owns cancellation while the core polls under the original request deadline.
pub struct RecoveryResponse {
    resource: zplugin::Resource<'static>,
}
impl RecoveryResponse {
    pub fn poll(&mut self) -> Result<Option<Option<usize>>, String> {
        let mut output = [0_u8; recovery::MAX_RESULT_BYTES];
        let count = match self.resource.call(recovery::POLL, &[], &mut output) {
            Ok(n) if n <= output.len() => n,
            Err(Error::PluginStatus(3)) => return Ok(None),
            _ => return Err("invalid provider recovery result".into()),
        };
        let value = crate::state_json::parse(&output[..count])
            .map_err(|_| "invalid provider recovery result")?;
        let object = value
            .as_object()
            .ok_or("invalid provider recovery result")?;
        match value.get("recovered").as_bool() {
            Some(false) if object.len() == 1 => Ok(Some(None)),
            Some(true) if object.len() == 2 => {
                let index = value
                    .get("candidate")
                    .as_u64()
                    .filter(|n| *n < recovery::MAX_ATTEMPTS as u64)
                    .ok_or("invalid recovery candidate")?;
                Ok(Some(Some(index as usize)))
            }
            _ => Err("invalid provider recovery result".into()),
        }
    }
}

pub struct ProviderHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_route_flags_are_explicit_and_reject_unknown_capabilities() {
        assert_eq!(http_decision(0).unwrap(), None);
        assert_eq!(http_decision(1).unwrap(), Some(false));
        assert_eq!(http_decision(3).unwrap(), Some(true));
        for flags in [2, 4, 5, 7, u32::MAX] {
            assert!(http_decision(flags).is_err());
        }
    }
    #[test]
    fn empty_chain_preserves_input_without_an_output_allocation() {
        let mut plugins = Plugins::load(&[]).unwrap();
        assert!(plugins.is_empty());
        assert!(!plugins.has_retry());
        assert!(!plugins.has_recovery());
        assert!(plugins.begin_recovery(b"{}").unwrap().is_none());
        assert!(plugins.http_handler("/mcp/example").unwrap().is_none());
        assert!(
            plugins
                .retry(protocol::OPENAI_CHAT, 429, "local", b"{}", 1024)
                .unwrap()
                .is_none()
        );
        assert_eq!(plugins.route("local").unwrap(), "local");
        assert_eq!(
            plugins.request(protocol::OPENAI_CHAT, b"{}", 1024).unwrap(),
            None
        );
        plugins.observe(200, 10, 2, 3).unwrap();
    }

    #[test]
    fn request_hooks_reuse_owned_buffers_and_preserve_chain_order() {
        let body = vec![b'a'; 1024 * 1024];
        let mut changed = None;
        let mut output = Vec::new();
        let path = std::path::Path::new("/fixture/request-plugin");
        let mut buffers = Vec::new();
        for (before, after) in [(b'a', b'b'), (b'b', b'c'), (b'c', b'd'), (b'd', b'e')] {
            let mut written = std::ptr::null();
            request_hook(
                &body,
                &mut changed,
                &mut output,
                body.len(),
                path,
                |input, output| {
                    assert_eq!(input.len(), body.len());
                    assert!(input.iter().all(|byte| *byte == before));
                    if output.len() < input.len() {
                        return Err(Error::BufferTooSmall);
                    }
                    assert_ne!(input.as_ptr(), output.as_ptr());
                    output[..input.len()].fill(after);
                    written = output.as_ptr();
                    Ok(input.len() as u64)
                },
            )
            .unwrap();
            let current = changed.as_deref().unwrap();
            assert_eq!(
                current.as_ptr(),
                written,
                "a completed hook must not copy its output"
            );
            assert!(current.iter().all(|byte| *byte == after));
            buffers.push(written);

            // A subsequent unchanged hook may inspect the current replacement
            // and write scratch, but must not replace or corrupt that input.
            request_hook(
                &body,
                &mut changed,
                &mut output,
                body.len(),
                path,
                |input, output| {
                    assert!(input.iter().all(|byte| *byte == after));
                    output.fill(b'x');
                    Ok(0)
                },
            )
            .unwrap();
            assert_eq!(changed.as_ref().unwrap().as_ptr(), written);
            assert!(changed.as_ref().unwrap().iter().all(|byte| *byte == after));
        }
        assert_eq!(buffers[0], buffers[2]);
        assert_eq!(buffers[1], buffers[3]);
        assert_ne!(buffers[0], buffers[1]);
        assert!(body.iter().all(|byte| *byte == b'a'));
    }

    #[test]
    fn unchanged_request_hook_keeps_original_borrowed() {
        let body = b"{\"model\":\"local\"}";
        let mut changed = None;
        let mut output = Vec::new();
        request_hook(
            body,
            &mut changed,
            &mut output,
            1024,
            std::path::Path::new("/fixture/request-plugin"),
            |input, output| {
                assert_eq!(input.as_ptr(), body.as_ptr());
                assert!(output.is_empty());
                Ok(0)
            },
        )
        .unwrap();
        assert!(changed.is_none());
        assert_eq!(output.capacity(), 0);
    }

    #[test]
    fn failed_request_hook_never_publishes_partial_output() {
        let body = b"original";
        let path = std::path::Path::new("/fixture/request-plugin");
        let mut changed = Some(b"previous".to_vec());
        let mut output = Vec::new();
        let mut sizes = Vec::new();
        let error = request_hook(body, &mut changed, &mut output, 512, path, |_, output| {
            sizes.push(output.len());
            output.fill(b'x');
            Err(Error::BufferTooSmall)
        });
        assert!(error.is_err());
        assert_eq!(sizes, [0, 512]);
        assert_eq!(changed.as_deref(), Some(b"previous".as_slice()));

        let error = request_hook(body, &mut changed, &mut output, 512, path, |_, output| {
            Ok(output.len() as u64 + 1)
        });
        assert!(error.unwrap_err().contains("exceeded its output buffer"));
        assert_eq!(changed.as_deref(), Some(b"previous".as_slice()));

        let error = request_hook(body, &mut changed, &mut output, 512, path, |_, output| {
            output.fill(b'x');
            Err(Error::PluginStatus(status::REQUEST_TOO_LARGE))
        });
        assert!(
            error
                .unwrap_err()
                .contains("request exceeds plugin body limit")
        );
        assert_eq!(changed.as_deref(), Some(b"previous".as_slice()));
    }

    #[test]
    fn rejects_relative_paths_and_invalid_output_lengths() {
        let spec = PluginSpec {
            provider_id: None,
            path: PathBuf::from("plugin.so"),
            config: String::new(),
        };
        assert!(Plugins::load(&[spec]).is_err());
        assert!(checked_count(8, 4, std::path::Path::new("/tmp/test.so")).is_err());
        assert!(validate_model("bad\nmodel").is_err());
    }
}
