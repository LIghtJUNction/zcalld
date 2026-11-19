//! Optional extension management is a short-lived compatibility helper.
//! It is never launched for ordinary gateway requests without configured plugins.
use crate::{
    plugins::PluginSpec,
    state_json::{self, Value, object},
};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
pub fn root(config_path: &Path) -> Result<PathBuf, String> {
    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(parent)
    };
    Ok(parent.join("extensions"))
}
fn helper() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ZCALLD_PLUGINCTL") {
        let p = PathBuf::from(path);
        if p.is_absolute() && p.is_file() {
            return Ok(p);
        }
        return Err("ZCALLD_PLUGINCTL must name an absolute compatibility helper".into());
    }
    let file = format!("zcalld-pluginctl{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = vec![
        Path::new("/usr/libexec/zcalld").join(&file),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("extensions/compat/bin")
            .join(&file),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.insert(0, dir.join("libexec/zcalld").join(&file));
        if let Some(prefix) = dir.parent() {
            candidates.insert(0, prefix.join("libexec/zcalld").join(&file));
        }
    }
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("community management requires the optional zcalld-pluginctl compatibility helper".into())
}
pub fn run(args: &[String], config_path: &Path) -> Result<(), String> {
    let root = root(config_path)?;
    let args = if args.is_empty() {
        vec!["list".into()]
    } else {
        args.to_vec()
    };
    let binary = match helper() {
        Ok(p) => p,
        Err(e) if args[0] == "list" => {
            let registry = root.join("registry.json");
            let value = if registry.exists() {
                state_json::read(&registry)?
            } else {
                object([("plugins", Value::Array(Vec::new()))])
            };
            let plugins = value
                .get("plugins")
                .as_array()
                .iter()
                .map(|p| {
                    let mut v = object([]);
                    for key in [
                        "name", "path", "kind", "enabled", "version", "source", "revision",
                        "sha256",
                    ] {
                        v.set(key, p.get(key).clone());
                    }
                    v
                })
                .collect();
            println!(
                "{}",
                object([("plugins", Value::Array(plugins))]).stringify()
            );
            let _ = e;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let status = Command::new(binary)
        .arg("--root")
        .arg(root)
        .args(args)
        .status()
        .map_err(|_| "could not start compatibility helper")?;
    if status.success() {
        Ok(())
    } else {
        Err("plugin operation failed; see the helper diagnostic".into())
    }
}
fn library(kind: &str) -> Result<PathBuf, String> {
    let variable = format!("ZCALLD_{}_PLUGIN", kind.to_ascii_uppercase());
    if let Some(path) = std::env::var_os(variable) {
        let path = PathBuf::from(path);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err("configured extension library must be an absolute regular file".into());
    }
    let name = format!(
        "{}zcalld_{kind}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let mut candidates = vec![
        Path::new("/usr/lib/zcalld/plugins").join(&name),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/release")
            .join(&name),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/debug")
            .join(&name),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.insert(0, dir.join("plugins").join(&name));
        if let Some(prefix) = dir.parent() {
            candidates.insert(0, prefix.join("lib/zcalld/plugins").join(&name));
        }
    }
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!("optional {kind} native plugin is not installed"))
}
/// Explicit managed records augment INI plugin sections, in registry order.
/// Community packages share one native bridge; their JS code stays out of core.
pub fn managed_plugins(config_path: &Path) -> Result<Vec<PluginSpec>, String> {
    let root = root(config_path)?;
    let registry = root.join("registry.json");
    if !registry.exists() {
        return Ok(Vec::new());
    }
    let value = state_json::read(&registry)?;
    let mut specs = Vec::new();
    let mut community = false;
    for p in value.get("plugins").as_array() {
        if p.get("enabled").as_bool() != Some(true) {
            continue;
        }
        match p.get("kind").as_str() {
            Some("community") => community = true,
            Some("native") => {
                let path = PathBuf::from(
                    p.get("path")
                        .as_str()
                        .ok_or("native extension path missing")?,
                );
                if !path.is_absolute() || !path.is_file() {
                    return Err("native extension path unavailable".into());
                }
                let name = p
                    .get("name")
                    .as_str()
                    .ok_or("native extension name missing")?;
                let status = Command::new(helper()?)
                    .arg("--root")
                    .arg(&root)
                    .arg("verify")
                    .arg(name)
                    .arg("--json")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|_| "native extension checksum verifier unavailable")?;
                if !status.success() {
                    return Err("managed native extension checksum mismatch".into());
                }
                let config = if let Some(file) = p.get("config_file").as_str() {
                    let bytes = std::fs::read(file)
                        .map_err(|_| "native extension configuration unavailable")?;
                    if bytes.len() > 65536 {
                        return Err("native extension configuration exceeds limit".into());
                    }
                    String::from_utf8(bytes).map_err(|_| "native extension config is not UTF-8")?
                } else {
                    String::new()
                };
                specs.push(PluginSpec { path, config });
            }
            _ => return Err("unknown managed extension kind".into()),
        }
    }
    if community {
        let file = root.join("host.json");
        let config = state_json::read(&file)?.stringify();
        specs.push(PluginSpec {
            path: library("community")?,
            config,
        });
    }
    Ok(specs)
}
/// Privacy flags stay in their private settings file and are read per request.
pub fn privacy_plugin(config_path: &Path) -> Result<Option<PluginSpec>, String> {
    let config = root(config_path)?
        .parent()
        .ok_or("configuration parent missing")?
        .join("privacy/plugin.json");
    if !config.exists() {
        return Ok(None);
    }
    Ok(Some(PluginSpec {
        path: library("redact")?,
        config: state_json::read(&config)?.stringify(),
    }))
}

/// Group routing runs after privacy, so an optional decision provider sees masked text.
pub fn routing_plugin(config_path: &Path) -> Result<Option<PluginSpec>, String> {
    let path = root(config_path)?
        .parent()
        .ok_or("configuration parent missing")?
        .join("routing.json");
    if !path.exists() {
        return Ok(None);
    }
    let value = state_json::read(&path)?;
    if value.get("groups").as_array().is_empty() {
        return Ok(None);
    }
    Ok(Some(PluginSpec {
        path: library("plugin_routing")?,
        config: path.to_string_lossy().into_owned(),
    }))
}
