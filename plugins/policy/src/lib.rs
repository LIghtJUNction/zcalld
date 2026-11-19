//! Optional model allowlist and request-size policy.

use std::collections::BTreeSet;
use zcalld_plugin_api::{GatewayPlugin, GatewayPluginExport, MAX_MODEL_BYTES, protocol, status};
use zplugin::{Error, Result};

#[derive(Default)]
struct Policy {
    allow: Option<BTreeSet<String>>,
    max_bytes: Option<u64>,
    completed_requests: u64,
    failed_requests: u64,
    transferred_bytes: u64,
}

impl GatewayPlugin for Policy {
    fn configure(&mut self, config: &str) -> Result<()> {
        let mut allow = None;
        let mut max_bytes = None;
        for line in config.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, value) = line.split_once('=').ok_or(Error::InvalidArgument)?;
            match name.trim() {
                "allow" if allow.is_none() => {
                    let mut models = BTreeSet::new();
                    for model in value.split(',').map(str::trim) {
                        if model.is_empty()
                            || model.len() > MAX_MODEL_BYTES
                            || model.chars().any(char::is_control)
                            || !models.insert(model.to_owned())
                        {
                            return Err(Error::InvalidArgument);
                        }
                    }
                    allow = Some(models);
                }
                "max_bytes" if max_bytes.is_none() => {
                    let limit = value
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| Error::InvalidArgument)?;
                    if limit == 0 {
                        return Err(Error::InvalidArgument);
                    }
                    max_bytes = Some(limit);
                }
                _ => return Err(Error::InvalidArgument),
            }
        }
        self.allow = allow;
        self.max_bytes = max_bytes;
        Ok(())
    }

    fn route(&mut self, model: &str, _output: &mut [u8]) -> Result<u64> {
        if self
            .allow
            .as_ref()
            .is_some_and(|allow| !allow.contains(model))
        {
            return Err(Error::PluginStatus(status::MODEL_DENIED));
        }
        Ok(0)
    }

    fn request(&mut self, protocol: u32, input: &[u8], _output: &mut [u8]) -> Result<u64> {
        if !protocol::is_supported(protocol) {
            return Err(Error::InvalidArgument);
        }
        if self
            .max_bytes
            .is_some_and(|limit| input.len() as u64 > limit)
        {
            return Err(Error::PluginStatus(status::REQUEST_TOO_LARGE));
        }
        Ok(0)
    }

    fn observe(
        &mut self,
        status: u32,
        _elapsed_ms: u64,
        input_bytes: u64,
        output_bytes: u64,
    ) -> Result<()> {
        self.completed_requests = self.completed_requests.saturating_add(1);
        if status >= 400 {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
        self.transferred_bytes = self
            .transferred_bytes
            .saturating_add(input_bytes)
            .saturating_add(output_bytes);
        Ok(())
    }
}

fn create() -> Result<Policy> {
    Ok(Policy::default())
}

zplugin::export!(
    GatewayPluginExport<Policy>,
    constructor = create,
    version = (
        zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MAJOR")),
        zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_MINOR")),
        zcalld_plugin_api::version_component(env!("CARGO_PKG_VERSION_PATCH"))
    ),
    unload = pinned
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_allowlist_and_request_size_without_rewriting() {
        let mut plugin = Policy::default();
        plugin
            .configure("allow=local,reasoning\nmax_bytes=3")
            .unwrap();
        assert_eq!(plugin.route("local", &mut []), Ok(0));
        assert_eq!(
            plugin.route("unknown", &mut []),
            Err(Error::PluginStatus(status::MODEL_DENIED)),
        );
        assert_eq!(
            plugin.request(protocol::OPENAI_CHAT, b"123", &mut []),
            Ok(0)
        );
        assert_eq!(
            plugin.request(protocol::OPENAI_CHAT, b"1234", &mut []),
            Err(Error::PluginStatus(status::REQUEST_TOO_LARGE)),
        );
    }

    #[test]
    fn rejects_unknown_duplicate_and_empty_settings() {
        for config in [
            "allowed=a",
            "allow=",
            "allow=a,a",
            "allow=a\nallow=b",
            "max_bytes=0",
            "max_bytes=-1",
            "max_bytes=1\nmax_bytes=2",
        ] {
            assert!(Policy::default().configure(config).is_err(), "{config}");
        }
    }

    #[test]
    fn configuration_is_atomic_and_observation_never_overflows() {
        let mut plugin = Policy::default();
        plugin.configure("allow=local").unwrap();
        assert!(plugin.configure("allow=other\nunknown=key").is_err());
        assert_eq!(plugin.route("local", &mut []), Ok(0));
        plugin.observe(200, 1, u64::MAX, 1).unwrap();
        plugin.observe(503, 1, 1, 1).unwrap();
        assert_eq!(plugin.completed_requests, 2);
        assert_eq!(plugin.failed_requests, 1);
        assert_eq!(plugin.transferred_bytes, u64::MAX);
    }
}
