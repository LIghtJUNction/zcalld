//! Load real shared libraries and verify the published interface on this host.

use std::{env, path::Path};
use zcalld_plugin_api::{GatewayPluginApi, protocol, status};
use zplugin::{Error, Plugin, UnloadPolicy};

fn load(path: &str) -> Result<Plugin, Box<dyn std::error::Error>> {
    let path = Path::new(path).canonicalize()?;
    // SAFETY: this example loads operator-supplied, locally built test plugins.
    Ok(unsafe { Plugin::load_path(path, UnloadPolicy::Pinned)? })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<_> = env::args().skip(1).collect();
    if paths.len() != 2 {
        return Err("usage: check_plugins ALIASES_SO POLICY_SO".into());
    }
    let aliases = load(&paths[0])?;
    let policy = load(&paths[1])?;
    let mut aliases = GatewayPluginApi::create(&aliases)?;
    let mut policy = GatewayPluginApi::create(&policy)?;
    aliases.configure("fast=local")?;
    policy.configure("allow=local\nmax_bytes=1024")?;
    let mut route = [0; 32];
    let count = aliases.route("fast", &mut route)? as usize;
    assert_eq!(&route[..count], b"local");
    assert_eq!(
        policy.route(std::str::from_utf8(&route[..count])?, &mut [])?,
        0
    );
    assert_eq!(aliases.request(protocol::OPENAI_CHAT, b"{}", &mut [])?, 0);
    assert_eq!(policy.request(protocol::OPENAI_CHAT, b"{}", &mut [])?, 0);
    assert_eq!(
        policy.route("blocked", &mut []),
        Err(Error::PluginStatus(status::MODEL_DENIED))
    );
    assert_eq!(
        policy.request(protocol::OPENAI_CHAT, &[0; 1025], &mut []),
        Err(Error::PluginStatus(status::REQUEST_TOO_LARGE)),
    );
    policy.observe(200, 1, 2, 64)?;
    println!("native plugins: aliases, ordered policy, request limit and observation passed");
    Ok(())
}
