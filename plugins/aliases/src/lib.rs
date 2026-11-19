//! Explicit one-step model aliases, loaded only when configured by the operator.

use std::collections::BTreeMap;
use zcalld_plugin_api::{GatewayPlugin, GatewayPluginExport, MAX_MODEL_BYTES};
use zplugin::{Error, Result};

#[derive(Default)]
struct Aliases {
    entries: BTreeMap<String, String>,
}

fn model(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_MODEL_BYTES || value.chars().any(char::is_control) {
        return Err(Error::InvalidArgument);
    }
    Ok(value)
}

impl GatewayPlugin for Aliases {
    fn configure(&mut self, config: &str) -> Result<()> {
        let mut entries = BTreeMap::new();
        for line in config.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (alias, target) = line.split_once('=').ok_or(Error::InvalidArgument)?;
            let alias = model(alias)?.to_owned();
            let target = model(target)?.to_owned();
            if entries.insert(alias, target).is_some() {
                return Err(Error::InvalidArgument);
            }
        }
        self.entries = entries;
        Ok(())
    }

    fn route(&mut self, model: &str, output: &mut [u8]) -> Result<u64> {
        let Some(target) = self.entries.get(model) else {
            return Ok(0);
        };
        if target == model {
            return Ok(0);
        }
        if output.len() < target.len() {
            return Err(Error::BufferTooSmall);
        }
        output[..target.len()].copy_from_slice(target.as_bytes());
        Ok(target.len() as u64)
    }

    fn request(&mut self, _protocol: u32, _input: &[u8], _output: &mut [u8]) -> Result<u64> {
        Ok(0)
    }

    fn observe(
        &mut self,
        _status: u32,
        _elapsed_ms: u64,
        _input_bytes: u64,
        _output_bytes: u64,
    ) -> Result<()> {
        Ok(())
    }
}

fn create() -> Result<Aliases> {
    Ok(Aliases::default())
}

zplugin::export!(
    GatewayPluginExport<Aliases>,
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
    fn selects_explicit_alias_and_leaves_unknown_model_alone() {
        let mut plugin = Aliases::default();
        plugin
            .configure("# routes\nfast = local\n思考 = reasoning\n")
            .unwrap();
        let mut output = [0; 64];
        let length = plugin.route("fast", &mut output).unwrap() as usize;
        assert_eq!(&output[..length], b"local");
        assert_eq!(plugin.route("other", &mut output).unwrap(), 0);
        let length = plugin.route("思考", &mut output).unwrap() as usize;
        assert_eq!(&output[..length], b"reasoning");
    }

    #[test]
    fn bad_configuration_is_atomic_and_short_buffer_is_replay_safe() {
        let mut plugin = Aliases::default();
        plugin.configure("fast=local").unwrap();
        assert!(plugin.configure("x=y\nx=z").is_err());
        assert_eq!(
            plugin.route("fast", &mut [0; 1]),
            Err(Error::BufferTooSmall)
        );
        let mut output = [0; 5];
        assert_eq!(plugin.route("fast", &mut output).unwrap(), 5);
        assert_eq!(&output, b"local");
    }

    #[test]
    fn rejects_empty_or_control_character_names() {
        for config in [
            "=target",
            "alias=",
            "alias=bad\u{0000}name",
            "missing equals",
        ] {
            assert!(Aliases::default().configure(config).is_err());
        }
    }
}
