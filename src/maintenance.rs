//! Optional management actions. No updater, account polling or database in the daemon.
use crate::{
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("update") => update(context, &args[1..]),
        Some("accounts") => accounts(context, &args[1..]),
        Some("groups") => groups(context, &args[1..]),
        Some("library") => library(context, &args[1..]),
        Some("usage") => usage(context, &args[1..]),
        Some("routes") => routes(context, &args[1..]),
        _ => Err("unknown management command".into()),
    }
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let (code, body, _) = control::command(
        "curl",
        &[
            "--fail".into(),
            "--silent".into(),
            "--show-error".into(),
            "--location".into(),
            "--proto".into(),
            "=https".into(),
            "--proto-redir".into(),
            "=https".into(),
            "--max-time".into(),
            "30".into(),
            "--max-filesize".into(),
            "2097152".into(),
            "--header".into(),
            "Accept: application/vnd.github+json".into(),
            url.into(),
        ],
        None,
        Duration::from_secs(35),
    )?;
    if code != 0 {
        return Err("could not read the published GitHub release".into());
    }
    Ok(body.into_bytes())
}
fn target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err("no published binary for this platform".into()),
    }
}
fn semver(value: &str) -> Result<[u64; 3], String> {
    let parts: Vec<_> = value
        .trim_start_matches('v')
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<_, _>>()
        .map_err(|_| "invalid release version")?;
    parts
        .try_into()
        .map_err(|_| "release must use major.minor.patch".into())
}
fn update(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("check");
    if !matches!(action, "check" | "apply") {
        return Err("update expects check or apply".into());
    }
    let release = json::parse(&fetch(
        "https://api.github.com/repos/LIghtJUNction/zcalld/releases/latest",
    )?)?;
    let latest = release
        .get("tag_name")
        .as_str()
        .ok_or("release has no version tag")?;
    let latest_version = semver(latest)?;
    let available = latest_version > semver(env!("CARGO_PKG_VERSION"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let managed = exe.starts_with("/usr/bin")
        || exe.starts_with("/usr/sbin")
        || exe.starts_with("/opt/homebrew")
        || exe.to_string_lossy().contains("Program Files");
    if action == "check" {
        return control::emit(object([
            ("current", env!("CARGO_PKG_VERSION").into()),
            ("latest", latest.trim_start_matches('v').into()),
            ("available", available.into()),
            ("managed", managed.into()),
            (
                "method",
                if managed {
                    "package-manager"
                } else {
                    "verified-binary"
                }
                .into(),
            ),
            (
                "message",
                if managed {
                    "Update the installed zcalld package with your package manager."
                } else if available {
                    "A published version is available."
                } else {
                    "Already on the latest published version."
                }
                .into(),
            ),
        ]));
    }
    if !available {
        return control::emit(object([
            ("ok", true.into()),
            ("changed", false.into()),
            ("latest", latest.into()),
            ("message", "Already up to date.".into()),
        ]));
    }
    if managed {
        return Err("this installation is owned by the package manager; on Arch run paru -Syu zcalld-bin (or zcalld for source builds)".into());
    }
    let name = format!(
        "zcalld-{}-{}{}",
        latest.trim_start_matches('v'),
        target()?,
        if cfg!(windows) { ".exe" } else { "" }
    );
    let assets = release.get("assets").as_array();
    let asset = assets
        .iter()
        .find(|v| v.get("name").as_str() == Some(&name))
        .ok_or(
            "this release has no standalone binary for this platform; use its package instead",
        )?;
    let checksum = assets
        .iter()
        .find(|v| v.get("name").as_str() == Some("SHA256SUMS"))
        .ok_or("release has no integrity manifest")?;
    let prefix = format!("https://github.com/LIghtJUNction/zcalld/releases/download/{latest}/");
    let binary_url = asset
        .get("browser_download_url")
        .as_str()
        .filter(|v| v.starts_with(&prefix))
        .ok_or("unexpected release asset location")?;
    let checksum_url = checksum
        .get("browser_download_url")
        .as_str()
        .filter(|v| v.starts_with(&prefix))
        .ok_or("unexpected integrity manifest location")?;
    let checksums =
        String::from_utf8(fetch(checksum_url)?).map_err(|_| "checksum manifest is not UTF-8")?;
    let expected = checksums
        .lines()
        .find_map(|line| {
            let mut p = line.split_whitespace();
            let hash = p.next()?;
            let file = p.next()?.trim_start_matches('*');
            if file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                Some(hash.to_ascii_lowercase())
            } else {
                None
            }
        })
        .ok_or("binary missing from SHA256SUMS")?;
    let parent = exe.parent().ok_or("executable has no parent directory")?;
    let temporary = parent.join(format!(
        ".zcalld-update-{}{}",
        std::process::id(),
        if cfg!(windows) { ".exe" } else { "" }
    ));
    if temporary.exists() {
        return Err("an update staging file already exists; inspect it before retrying".into());
    }
    let result = (|| {
        let (code, _, _) = control::command(
            "curl",
            &[
                "--fail".into(),
                "--silent".into(),
                "--show-error".into(),
                "--location".into(),
                "--proto".into(),
                "=https".into(),
                "--proto-redir".into(),
                "=https".into(),
                "--max-time".into(),
                "120".into(),
                "--max-filesize".into(),
                "52428800".into(),
                "--output".into(),
                temporary.to_string_lossy().into_owned(),
                binary_url.into(),
            ],
            None,
            Duration::from_secs(125),
        )?;
        if code != 0 {
            return Err("binary download failed".into());
        }
        let data = fs::read(&temporary).map_err(|e| e.to_string())?;
        if crate::digest::sha256(&data) != expected {
            return Err("release checksum mismatch; installed binary was not changed".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        let (code, version, _) = control::command(
            &temporary.to_string_lossy(),
            &["--version".into()],
            None,
            Duration::from_secs(5),
        )?;
        if code != 0 || version.trim() != format!("zcalld {}", latest.trim_start_matches('v')) {
            return Err("downloaded binary failed its version check".into());
        }
        let saved = parent.join(format!("zcalld-{}.previous", env!("CARGO_PKG_VERSION")));
        if saved.exists() {
            return Err(
                "previous binary backup already exists; preserve or remove it before updating"
                    .into(),
            );
        }
        fs::copy(&exe, &saved).map_err(|e| e.to_string())?;
        #[cfg(not(windows))]
        fs::rename(&temporary, &exe).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        {
            return crate::windows_update::queue(context, &temporary, &exe, &expected, latest);
        }
        #[cfg(not(windows))]
        {
            let _ = context;
            control::emit(object([
                ("ok", true.into()),
                ("changed", true.into()),
                ("latest", latest.into()),
                ("restart_required", true.into()),
                ("backup", saved.to_string_lossy().into_owned().into()),
                (
                    "message",
                    "Verified update installed. Restart the gateway service to use it.".into(),
                ),
            ]))
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn accounts(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    if ![
        "list", "import", "use", "enable", "disable", "remove", "session",
    ]
    .contains(&action)
    {
        return Err(
            "accounts expects list, import, use, enable, disable, remove or session".into(),
        );
    }
    let helper = crate::extensions::helper()?;
    let root = context.directory.join("extensions");
    if action == "session" {
        let status = std::process::Command::new(&helper)
            .args(["--root", &root.to_string_lossy(), "accounts", "session"])
            .status()
            .map_err(|e| e.to_string())?;
        return if status.success() {
            Ok(())
        } else {
            Err("account session failed".into())
        };
    }
    let local_provider = control::option(args, "--local-provider")?;
    let mut text = if action == "use" {
        Some(control::config_text(context)?)
    } else {
        None
    };
    if let Some(name) = local_provider
        && !control::sections(
            text.as_deref()
                .ok_or("--local-provider is only valid with accounts use")?,
        )
        .iter()
        .any(|(section, _)| section == &format!("provider {name}"))
    {
        return Err("unknown local provider".into());
    }
    let mut command = vec![
        "--root".into(),
        root.to_string_lossy().into_owned(),
        "accounts".into(),
    ];
    if args.is_empty() {
        command.push("list".into());
    }
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--local-provider" {
            skip = true;
            continue;
        }
        command.push(arg.clone());
    }
    let (code, out, _) = control::command(
        &helper.to_string_lossy(),
        &command,
        None,
        Duration::from_secs(30),
    )?;
    if code != 0 {
        return Err("account operation was rejected by the compatibility helper; no credentials are included in this error".into());
    }
    let mut result = json::parse(out.as_bytes())?;
    if action == "use" {
        let selected = result.get("selection");
        let provider = selected
            .get("provider")
            .as_str()
            .ok_or("account selection has no provider")?;
        let account = selected
            .get("account")
            .as_str()
            .ok_or("account selection has no account")?;
        let mut content = text.take().unwrap();
        let mut changed = 0u64;
        for (section, values) in control::sections(&content) {
            let Some(name) = section.strip_prefix("provider ") else {
                continue;
            };
            if local_provider.is_some_and(|wanted| wanted != name) {
                continue;
            }
            if values.get("plugin").is_some_and(|id| id == provider) {
                content = control::ini_set(&content, &section, "plugin_account", account)?;
                changed += 1;
            }
        }
        if changed > 0 {
            control::save_config(context, &content, "account-selection")?;
        }
        result.set("updated_providers", changed.into());
        result.set("restart_required", (changed > 0).into());
    }
    control::emit(result)
}

pub fn plugin_helper() -> Result<String, String> {
    Ok(crate::extensions::helper()?.to_string_lossy().into_owned())
}

fn groups(context: &Context, args: &[String]) -> Result<(), String> {
    let path = context.directory.join("routing.json");
    let mut state = if path.exists() {
        json::read(&path)?
    } else {
        object([("groups", Value::Array(Vec::new()))])
    };
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "list" {
        return control::emit(state);
    }
    let name = args
        .get(1)
        .filter(|v| control::valid_name(v))
        .ok_or("group name required")?;
    let mut entries = state.get("groups").as_array().to_vec();
    let previous = entries
        .iter()
        .find(|v| v.get("name").as_str() == Some(name))
        .cloned();
    let mut text = control::config_text(context)?;
    let section = format!("route {name}");
    match action {
        "set" => {
            let mode = control::option(args, "--mode")?.unwrap_or("order");
            if ![
                "smart", "order", "rotate", "usage", "pace", "weight", "drain",
            ]
            .contains(&mode)
            {
                return Err("unsupported routing mode".into());
            }
            let names = control::option(args, "--members")?
                .ok_or("--members requires comma-separated configured routes")?;
            let configured = crate::config::Config::load(&context.path)?;
            if previous.is_none() && configured.routes.iter().any(|r| r.name == *name) {
                return Err("a non-group route already uses this name".into());
            }
            let catalog = json::parse(configured.models_json.as_bytes())?;
            let mut members = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut first = None;
            for model in names.split(',').map(str::trim) {
                if model == name
                    || entries
                        .iter()
                        .any(|g| g.get("name").as_str() == Some(model))
                {
                    return Err(
                        "group members must be direct routes; nested groups are not supported"
                            .into(),
                    );
                }
                let route = configured
                    .routes
                    .iter()
                    .find(|r| r.name == model)
                    .ok_or_else(|| format!("unknown route {model}"))?;
                if !seen.insert(model) {
                    return Err("group members must be unique".into());
                }
                if first.is_none() {
                    first = Some(route);
                }
                let old = previous
                    .as_ref()
                    .map(|v| v.get("members").as_array())
                    .unwrap_or(&[]);
                let mut member = old
                    .iter()
                    .find(|m| m.get("model").as_str() == Some(model))
                    .cloned()
                    .unwrap_or_else(|| object([("model", model.into()), ("weight", 1u64.into())]));
                if let Some(metadata) = catalog
                    .get("data")
                    .as_array()
                    .iter()
                    .find(|m| m.get("id").as_str() == Some(model))
                {
                    member.set("reasoning_levels", metadata.get("reasoning_levels").clone());
                    member.set("context_window", metadata.get("context_window").clone());
                }
                members.push(member);
            }
            let first = first.ok_or("group needs at least one route")?;
            let mut next = previous.unwrap_or_else(|| object([("name", name.clone().into())]));
            next.set("mode", mode.into());
            next.set("members", Value::Array(members));
            entries.retain(|v| v.get("name").as_str() != Some(name));
            entries.push(next);
            text = control::ini_set(
                &text,
                &section,
                "provider",
                &configured.providers[first.provider].name,
            )?;
            text = control::ini_set(&text, &section, "model", &first.model)?;
        }
        "remove" => {
            if previous.is_none() {
                return Err("unknown group".into());
            }
            entries.retain(|v| v.get("name").as_str() != Some(name));
            text = control::ini_remove(&text, &section);
        }
        _ => return Err("groups expects list, set or remove".into()),
    }
    state.set("groups", Value::Array(entries));
    control::replace_files(
        context,
        &[
            (path, Some(state.stringify().into_bytes())),
            (context.path.clone(), Some(text.into_bytes())),
        ],
        "routing",
    )?;
    control::emit(object([
        ("ok", true.into()),
        ("restart_required", true.into()),
    ]))
}

fn routes(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let mut text = control::config_text(context)?;
    if action == "list" {
        let list = control::sections(&text)
            .into_iter()
            .filter_map(|(name, values)| {
                name.strip_prefix("route ").map(|name| {
                    object([
                        ("name", name.into()),
                        (
                            "provider",
                            values.get("provider").cloned().unwrap_or_default().into(),
                        ),
                        (
                            "model",
                            values.get("model").cloned().unwrap_or_default().into(),
                        ),
                    ])
                })
            })
            .collect();
        return control::emit(object([("routes", Value::Array(list))]));
    }
    let name = args
        .get(1)
        .filter(|v| control::valid_name(v))
        .ok_or("route name required")?;
    let section = format!("route {name}");
    match action {
        "set" => {
            let provider = control::option(args, "--provider")?.ok_or("--provider required")?;
            let model = control::option(args, "--model")?.ok_or("--model required")?;
            if !control::sections(&text)
                .iter()
                .any(|(s, _)| s == &format!("provider {provider}"))
            {
                return Err("unknown provider".into());
            }
            text = control::ini_set(&text, &section, "provider", provider)?;
            text = control::ini_set(&text, &section, "model", model)?;
        }
        "remove" => text = control::ini_remove(&text, &section),
        _ => return Err("routes expects list, set or remove".into()),
    }
    control::save_config(context, &text, "routes")?;
    control::emit(object([
        ("ok", true.into()),
        ("restart_required", true.into()),
    ]))
}

fn library(context: &Context, args: &[String]) -> Result<(), String> {
    let path = context.directory.join("library.json");
    let mut state = json::read(&path).unwrap_or_else(|_| {
        object([
            ("mcp", Value::Array(Vec::new())),
            ("skills", Value::Array(Vec::new())),
            ("instructions", Value::Object(BTreeMap::new())),
        ])
    });
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            let mcp = state
                .get("mcp")
                .as_array()
                .iter()
                .map(|v| {
                    object([
                        ("name", v.get("name").clone()),
                        ("transport", v.get("transport").clone()),
                        (
                            "enabled",
                            (!v.get("disabled").as_bool().unwrap_or(false)).into(),
                        ),
                    ])
                })
                .collect();
            control::emit(object([
                ("mcp", Value::Array(mcp)),
                ("skills", state.get("skills").as_array().len().into()),
                (
                    "instructions",
                    state
                        .get("instructions")
                        .as_object()
                        .map_or(0, |v| v.len())
                        .into(),
                ),
            ]))
        }
        "import" => {
            let input = control::option(args, "--file")?.ok_or("library import requires --file")?;
            let imported = json::read(Path::new(input))?;
            if !matches!(imported, Value::Object(_)) {
                return Err("library JSON must be an object".into());
            }
            control::backup(context, std::slice::from_ref(&path), "library")?;
            json::atomic_write(&path, imported.stringify().as_bytes())?;
            control::emit(object([("ok", true.into())]))
        }
        "export" => {
            let target = control::option(args, "--file")?
                .ok_or("library export requires a private --file")?;
            json::atomic_write(Path::new(target), state.stringify().as_bytes())?;
            control::emit(object([("ok", true.into())]))
        }
        "add-mcp" => {
            let input = control::option(args, "--file")?.ok_or("add-mcp requires --file")?;
            let entry = json::read(Path::new(input))?;
            let name = entry
                .get("name")
                .as_str()
                .filter(|v| control::safe_id(v))
                .ok_or("MCP entry needs a safe name")?;
            let mut entries = state.get("mcp").as_array().to_vec();
            entries.retain(|v| v.get("name").as_str() != Some(name));
            entries.push(entry);
            state.set("mcp", Value::Array(entries));
            control::backup(context, std::slice::from_ref(&path), "library")?;
            json::atomic_write(&path, state.stringify().as_bytes())?;
            control::emit(object([("ok", true.into())]))
        }
        "apply" => apply_mcp(context, &state, args),
        _ => Err("library expects list, import, export, add-mcp or apply --agent NAME".into()),
    }
}
fn apply_mcp(context: &Context, state: &Value, args: &[String]) -> Result<(), String> {
    let agent = control::option(args, "--agent")?.ok_or("--agent required")?;
    let (path, key) = match agent {
        "claude" => (context.home.join(".claude.json"), "mcpServers"),
        "gemini" => (context.home.join(".gemini/settings.json"), "mcpServers"),
        "qwen" => (context.home.join(".qwen/settings.json"), "mcpServers"),
        "opencode" => (context.home.join(".config/opencode/opencode.json"), "mcp"),
        "vscode" => (
            context.home.join(if cfg!(target_os = "macos") {
                "Library/Application Support/Code/User/mcp.json"
            } else {
                ".config/Code/User/mcp.json"
            }),
            "servers",
        ),
        _ => return Err(
            "this agent needs a community MCP adapter; existing Magpie MCP files remain untouched"
                .into(),
        ),
    };
    let mut text = if path.exists() {
        control::read_text(&path)?
    } else {
        "{}\n".into()
    };
    let mut count = 0usize;
    for entry in state.get("mcp").as_array() {
        if entry.get("disabled").as_bool() == Some(true) {
            continue;
        }
        let name = entry
            .get("name")
            .as_str()
            .filter(|n| control::safe_id(n))
            .ok_or("invalid MCP name")?;
        let mut item = entry.clone();
        for key in ["name", "agents", "icon", "disabled"] {
            item.remove(key);
        }
        if agent == "opencode" {
            let remote = item.get("url").as_str().is_some();
            item.set("type", if remote { "remote" } else { "local" }.into());
            if !remote {
                let mut command = vec![item.get("command").clone()];
                command.extend_from_slice(item.get("args").as_array());
                item.set("command", Value::Array(command));
                item.remove("args");
            }
            if let Some(env) = item.remove("env") {
                item.set("environment", env);
            }
            item.remove("transport");
            item.set("enabled", true.into());
        } else {
            item.remove("transport");
        }
        text = json::patch_json(&text, &[key, name], &item)?;
        count += 1;
    }
    control::backup(context, std::slice::from_ref(&path), "mcp-apply")?;
    json::atomic_write(&path, text.as_bytes())?;
    control::emit(object([
        ("ok", true.into()),
        ("agent", agent.into()),
        ("servers", count.into()),
    ]))
}

fn usage(context: &Context, args: &[String]) -> Result<(), String> {
    let path = context.directory.join("usage.jsonl");
    if args.first().is_some_and(|v| v == "export") {
        let target = control::option(args, "--file")?.ok_or("usage export requires --file")?;
        let mut output = fs::OpenOptions::new();
        output.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            output.mode(0o600);
        }
        let mut output = output.open(target).map_err(|e| e.to_string())?;
        let mut input = fs::File::open(&path).map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        return control::emit(object([("ok", true.into())]));
    }
    use std::io::{BufRead, BufReader};
    let mut requests = 0u64;
    let mut failed = 0u64;
    let mut input = 0u64;
    let mut output = 0u64;
    if path.exists() {
        for line in BufReader::new(fs::File::open(&path).map_err(|e| e.to_string())?).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.len() > 1024 * 1024 {
                continue;
            }
            if let Ok(v) = json::parse(line.as_bytes()) {
                requests += 1;
                if v.get("status").as_u64().is_some_and(|v| v >= 400) {
                    failed += 1;
                }
                input += v.get("input_tokens").as_u64().unwrap_or(0);
                output += v.get("output_tokens").as_u64().unwrap_or(0);
            }
        }
    }
    control::emit(object([
        ("requests", requests.into()),
        ("failed", failed.into()),
        ("input_tokens", input.into()),
        ("output_tokens", output.into()),
        ("source", path.to_string_lossy().into_owned().into()),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_order_is_numeric() {
        assert!(semver("v0.10.0").unwrap() > semver("0.9.99").unwrap());
        assert!(semver("0.2.0-rc1").is_err());
        assert!(semver("../../etc").is_err());
    }
}
