//! Explicit native-plugin loading and bounded, ordered hook execution.

use std::{path::PathBuf, rc::Rc};
use zcalld_plugin_api::{GatewayPluginApi, GatewayPluginProxy, MAX_MODEL_BYTES, protocol, status};
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
            entries.push(Entry { path, proxy });
        }
        Ok(Self { entries })
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
        _ => error.to_string(),
    };
    format!("plugin {} {hook}: {reason}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chain_preserves_input_without_an_output_allocation() {
        let mut plugins = Plugins::load(&[]).unwrap();
        assert!(plugins.is_empty());
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
