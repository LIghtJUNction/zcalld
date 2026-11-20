//! Explicit native-plugin loading and bounded, ordered hook execution.

use std::{path::PathBuf, rc::Rc};
use zcalld_plugin_api::{
    ContextPluginApi, ContextPluginProxy, GatewayPluginApi, GatewayPluginProxy, MAX_MODEL_BYTES,
    ProviderPluginApi, ProviderPluginProxy, ResponsePluginApi, ResponsePluginProxy, RetryPluginApi,
    RetryPluginProxy, protocol, status,
};
use zcalld_plugin_api::{HttpPluginApi, HttpPluginProxy};
use zplugin::{Error, Plugin, UnloadPolicy};

const MAX_PLUGINS: usize = 32;
const MAX_CONFIG_BYTES: usize = 65_536;

#[derive(Clone)]
pub struct PluginSpec {
    pub path: PathBuf,
    pub config: String,
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
    proxy: GatewayPluginProxy<'static>,
    context: Option<ContextPluginProxy<'static>>,
    response: Option<ResponsePluginProxy<'static>>,
    provider: Option<ProviderPluginProxy<'static>>,
    retry: Option<RetryPluginProxy<'static>>,
    http: Option<HttpPluginProxy<'static>>,
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
            if provider.is_some() && entries.iter().any(|e: &Entry| e.provider.is_some()) {
                return Err("only one community provider host may be configured".into());
            }
            entries.push(Entry {
                path,
                proxy,
                context,
                response,
                provider,
                retry,
                http,
            });
        }
        Ok(Self { entries })
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
    pub fn http_handler(&mut self, path: &str) -> Result<Option<usize>, String> {
        let mut selected = None;
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let Some(http) = &mut entry.http else {
                continue;
            };
            match http
                .accepts(path)
                .map_err(|e| hook_error(&entry.path, "HTTP route", e))?
            {
                0 => {}
                1 if selected.is_none() => selected = Some(index),
                1 => return Err("multiple plugins claim the same HTTP endpoint".into()),
                _ => return Err("HTTP plugin returned an invalid route decision".into()),
            }
        }
        Ok(selected)
    }

    pub fn http_fetch(
        &mut self,
        handler: usize,
        metadata: &str,
        body: &[u8],
    ) -> Result<ProviderResponse, String> {
        let entry = self.entries.get_mut(handler).ok_or("unknown HTTP plugin")?;
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

    /// Select the first configured custom provider bridge explicitly.
    pub fn provider_fetch(&mut self, request_json: &[u8]) -> Result<ProviderResponse, String> {
        for entry in &mut self.entries {
            if let Some(provider) = &mut entry.provider {
                let resource = provider
                    .fetch(request_json)
                    .map_err(|e| hook_error(&entry.path, "provider fetch", e))?;
                resource
                    .check("dev.zcalld.provider-stream", 1)
                    .map_err(|e| hook_error(&entry.path, "provider stream", e))?;
                return Ok(ProviderResponse {
                    resource,
                    ended: false,
                });
            }
        }
        Err("no custom provider plugin is configured".into())
    }

    pub fn has_context(&self) -> bool {
        self.entries.iter().any(|entry| entry.context.is_some())
    }

    pub fn has_retry(&self) -> bool {
        self.entries.iter().any(|entry| entry.retry.is_some())
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
            let input = changed.as_deref().unwrap_or(body);
            let count = loop {
                match entry.proxy.request(protocol, input, &mut output) {
                    Ok(count) => break count,
                    Err(Error::BufferTooSmall) if output.len() < max_body => {
                        let capacity = if output.is_empty() {
                            input.len().saturating_add(1024).max(256).min(max_body)
                        } else {
                            output.len().saturating_mul(2).min(max_body)
                        };
                        output.resize(capacity, 0);
                    }
                    Err(error) => return Err(hook_error(&entry.path, "request", error)),
                }
            };
            if count > 0 {
                let count = checked_count(count, output.len(), &entry.path)?;
                changed = Some(output[..count].to_vec());
            }
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
            _ => return Err("invalid provider response metadata".into()),
        };
        let value = crate::state_json::parse(&output[..count])?;
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
            _ => return Err(std::io::Error::other("invalid community provider output")),
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

pub struct ProviderHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chain_preserves_input_without_an_output_allocation() {
        let mut plugins = Plugins::load(&[]).unwrap();
        assert!(plugins.is_empty());
        assert!(!plugins.has_retry());
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
    fn rejects_relative_paths_and_invalid_output_lengths() {
        let spec = PluginSpec {
            path: PathBuf::from("plugin.so"),
            config: String::new(),
        };
        assert!(Plugins::load(&[spec]).is_err());
        assert!(checked_count(8, 4, std::path::Path::new("/tmp/test.so")).is_err());
        assert!(validate_model("bad\nmodel").is_err());
    }
}
