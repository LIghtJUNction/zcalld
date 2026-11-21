//! Optional management actions. No updater, account polling or database in the daemon.
use crate::{
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{fs, time::Duration};

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
            "-q".into(),
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
fn update(_context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("check");
    if args.len() > 1 || !matches!(action, "check" | "apply") {
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
    let managed = crate::installer::managed(&exe);
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
                    "verified-native-archive"
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
    crate::installer::update(latest.trim_start_matches('v'), &exe)
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

fn routing_state(context: &Context) -> Result<Value, String> {
    let path = context.directory.join("routing.json");
    if !path.try_exists().map_err(|error| error.to_string())? {
        return Ok(object([("groups", Value::Array(Vec::new()))]));
    }
    let state = json::read(&path)?;
    let fields = state.as_object().ok_or("routing.json must be an object")?;
    let groups: &[Value] = match fields.get("groups") {
        None => &[],
        Some(Value::Array(groups)) => groups,
        _ => return Err("routing.json groups must be an array".into()),
    };
    for group in groups {
        group
            .get("name")
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("routing.json group name must be a nonempty string")?;
        let members = match group.get("members") {
            Value::Array(members) if !members.is_empty() => members,
            _ => return Err("routing.json group members must be a nonempty array".into()),
        };
        for member in members {
            member
                .get("model")
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or("routing.json group member model must be a nonempty string")?;
        }
    }
    Ok(state)
}

/// Keep ordinary route/provider edits from invalidating managed model groups.
pub(crate) fn check_group_route_mutation<'a>(
    context: &Context,
    routes: impl IntoIterator<Item = &'a str>,
    removing: bool,
) -> Result<(), String> {
    let state = routing_state(context)?;
    let routes: std::collections::BTreeSet<_> = routes.into_iter().collect();
    for group in state.get("groups").as_array() {
        let name = group.get("name").as_str().unwrap();
        if routes.contains(name) {
            return Err(format!(
                "route {name} is a managed group; use groups set or groups remove"
            ));
        }
        if removing {
            for member in group.get("members").as_array() {
                let model = member.get("model").as_str().unwrap();
                if routes.contains(model) {
                    return Err(format!(
                        "route {model} is used by group {name}; update its members with groups set or remove it with groups remove first"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn groups(context: &Context, args: &[String]) -> Result<(), String> {
    let path = context.directory.join("routing.json");
    let mut state = routing_state(context)?;
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "list" {
        let deciders = state
            .get("deciders")
            .as_array()
            .iter()
            .map(|d| {
                object([
                    ("id", d.get("id").clone()),
                    ("url", d.get("url").clone()),
                    ("protocol", d.get("protocol").clone()),
                    (
                        "credential_configured",
                        d.get("key").as_str().is_some_and(|v| !v.is_empty()).into(),
                    ),
                ])
            })
            .collect();
        state.set("deciders", Value::Array(deciders));
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
            let definition = control::option(args, "--file")?
                .map(|file| json::read(std::path::Path::new(file)))
                .transpose()?;
            if let Some(value) = &definition {
                let fields = value
                    .as_object()
                    .ok_or("group definition must be an object")?;
                if fields.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "name"
                            | "mode"
                            | "routing"
                            | "members"
                            | "original"
                            | "rules"
                            | "affinity"
                            | "effort"
                            | "classifier"
                            | "levels"
                            | "intentSure"
                            | "effortMaps"
                    )
                }) {
                    return Err("group definition contains an unknown field".into());
                }
                if fields.contains_key("name") && value.get("name").as_str() != Some(name) {
                    return Err("group definition name differs from the selected group".into());
                }
                for field in ["mode", "routing"] {
                    if fields.contains_key(field) && value.get(field).as_str().is_none() {
                        return Err("group routing mode must be a string".into());
                    }
                }
                if fields.contains_key("members")
                    && !matches!(value.get("members"), Value::Array(members) if !members.is_empty())
                {
                    return Err("group members must be a nonempty array".into());
                }
                for original in [value, value.get("original")] {
                    if !matches!(original.get("rules"), Value::Null | Value::Array(_)) {
                        return Err("group rules must be an array".into());
                    }
                }
            }
            let file = definition.as_ref().unwrap_or(&Value::Null);
            let mode = control::option(args, "--mode")?
                .or_else(|| file.get("mode").as_str())
                .or_else(|| file.get("routing").as_str())
                .or_else(|| {
                    definition
                        .as_ref()
                        .and(previous.as_ref())
                        .and_then(|value| value.get("mode").as_str())
                })
                .unwrap_or("order")
                .to_string();
            if ![
                "smart", "order", "rotate", "usage", "pace", "weight", "drain",
            ]
            .contains(&mode.as_str())
            {
                return Err("unsupported routing mode".into());
            }
            let definitions = if let Some(names) = control::option(args, "--members")? {
                names.split(',').map(str::trim).map(Value::from).collect()
            } else if !file.get("members").as_array().is_empty() {
                file.get("members").as_array().to_vec()
            } else if definition.is_some() {
                previous
                    .as_ref()
                    .map(|value| value.get("members").as_array())
                    .unwrap_or(&[])
                    .iter()
                    .map(|member| member.get("model").clone())
                    .collect()
            } else {
                Vec::new()
            };
            if definitions.is_empty() {
                return Err("--members or a definition file must name configured routes".into());
            }
            let configured = crate::config::Config::load(&context.path)?;
            if previous.is_none() && configured.routes.iter().any(|r| r.name == *name) {
                return Err("a non-group route already uses this name".into());
            }
            let catalog = json::parse(configured.models_json.as_bytes())?;
            let mut members = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut first = None;
            for definition in &definitions {
                // JSON members already carry exact route names and optional settings.
                let model = definition
                    .as_str()
                    .or_else(|| definition.get("model").as_str())
                    .ok_or("group members must be route names or member objects")?;
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
                if let Some(fields) = definition.as_object() {
                    if let Some(weight) = fields.get("weight") {
                        if !weight
                            .as_u64()
                            .is_some_and(|value| (1..=10000).contains(&value))
                        {
                            return Err(
                                "group member weight must be an integer from 1 to 10000".into()
                            );
                        }
                        member.set("weight", weight.clone());
                    }
                    if let Some(off) = fields.get("off") {
                        if off.as_bool().is_none() {
                            return Err("group member off must be a boolean".into());
                        }
                        member.set("off", off.clone());
                    }
                }
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
            if let Some(definition) = &definition {
                let mut original = match definition.get("original") {
                    Value::Null => next.get("original").clone(),
                    value @ Value::Object(_) => value.clone(),
                    _ => return Err("original group settings must be an object".into()),
                };
                if matches!(original, Value::Null) {
                    original = object([]);
                }
                for key in [
                    "rules",
                    "affinity",
                    "effort",
                    "classifier",
                    "levels",
                    "intentSure",
                    "effortMaps",
                ] {
                    if definition.as_object().unwrap().contains_key(key) {
                        original.set(key, definition.get(key).clone());
                    }
                }
                for rule in original.get("rules").as_array() {
                    for target in
                        std::iter::once(rule.get("use")).chain(rule.get("more").as_array().iter())
                    {
                        let target = target.as_str().ok_or("rule target must be a model name")?;
                        if !seen.contains(target) {
                            return Err(
                                "rule target must be one of the group's configured members".into(),
                            );
                        }
                    }
                }
                next.set("original", original);
            }
            entries.retain(|v| v.get("name").as_str() != Some(name));
            entries.push(next);
            text = control::ini_set(
                &text,
                &section,
                "provider",
                &format!("\"{}\"", configured.providers[first.provider].name),
            )?;
            // This model is already parsed; preserve literal quotes and whitespace.
            text = control::ini_set(&text, &section, "model", &format!("\"{}\"", first.model))?;
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
    if let Some(configuration) =
        crate::migration::rule_configuration(state.get("groups").as_array())
    {
        let worker = crate::migration::validate_rule_configuration(
            &configuration,
            state.get("rules_worker").as_str(),
        )?;
        state.set("rules_worker", worker.to_string_lossy().into_owned().into());
    }
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
            if !crate::config::valid_model(crate::config::parse_ini_value(model)?) {
                return Err("model must be nonempty and at most 512 bytes".into());
            }
            if !control::sections(&text)
                .iter()
                .any(|(s, _)| s == &format!("provider {provider}"))
            {
                return Err("unknown provider".into());
            }
            check_group_route_mutation(context, [name.as_str()], false)?;
            text = control::ini_set(&text, &section, "provider", &format!("\"{provider}\""))?;
            text = control::ini_set(&text, &section, "model", model)?;
        }
        "remove" => {
            check_group_route_mutation(context, [name.as_str()], true)?;
            text = control::ini_remove(&text, &section);
        }
        _ => return Err("routes expects list, set or remove".into()),
    }
    control::save_config(context, &text, "routes")?;
    control::emit(object([
        ("ok", true.into()),
        ("restart_required", true.into()),
    ]))
}

#[path = "library.rs"]
mod agent_library;
fn library(context: &Context, args: &[String]) -> Result<(), String> {
    agent_library::run(context, args)
}

/// The setting is persisted configuration, not a claim about a running daemon.
fn usage_file(state: &Value, name: &str) -> Result<Option<std::path::PathBuf>, String> {
    match state.get(name) {
        Value::Null => Ok(None),
        Value::String(value) if value.is_empty() => Ok(None),
        Value::String(value) => {
            let path = std::path::PathBuf::from(value);
            if !path.is_absolute() || value.chars().any(char::is_control) {
                return Err(format!("routing.json {name} must be an absolute file path"));
            }
            Ok(Some(path))
        }
        _ => Err(format!("routing.json {name} must be a file path string")),
    }
}

fn usage_history(context: &Context, state: &Value) -> Result<std::path::PathBuf, String> {
    Ok(usage_file(state, "usage_file")?
        .or(usage_file(state, "usage_history_file")?)
        .unwrap_or_else(|| context.directory.join("usage.jsonl")))
}

fn usage_recording(context: &Context, enabled: bool) -> Result<(), String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static PROBE: AtomicU64 = AtomicU64::new(1);

    let path = context.directory.join("routing.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("refusing to replace a routing configuration symlink or non-file".into());
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut state = routing_state(context)?;
    let original = state.stringify();
    let configured = usage_file(&state, "usage_file")?;
    let native = if enabled {
        // Discover the library and validate the ordinary configuration before
        // staging files. Native configure below validates the routing settings.
        let native = crate::extensions::library("plugin_routing")?;
        let directory = fs::canonicalize(&context.directory).map_err(|e| e.to_string())?;
        crate::config::Config::parse(&control::config_text(context)?, &directory)?;
        let target = usage_history(context, &state)?;
        let target = if target.is_absolute() {
            target
        } else {
            directory.join("usage.jsonl")
        };
        if !target.parent().is_some_and(std::path::Path::is_dir) {
            return Err("usage history directory does not exist".into());
        }
        if target.exists() && !target.is_file() {
            return Err("usage history path is not a regular file".into());
        }
        if !target.exists() {
            // A valid path is not necessarily writable. Probe the target directory
            // with an empty private file, without creating or truncating the log.
            let write_probe = target.parent().unwrap().join(format!(
                ".usage-write-probe-{}-{}",
                std::process::id(),
                PROBE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            drop(options.open(&write_probe).map_err(|e| e.to_string())?);
            fs::remove_file(&write_probe).map_err(|e| e.to_string())?;
        }
        if target.is_file() {
            // Check append access without changing the retained history bytes.
            fs::OpenOptions::new()
                .append(true)
                .open(&target)
                .map_err(|e| e.to_string())?;
        }
        state.set("usage_file", target.to_string_lossy().into_owned().into());
        state.remove("usage_history_file");
        Some(native)
    } else {
        if let Some(target) = configured {
            // Keep a custom log readable and reusable while recording is disabled.
            state.set(
                "usage_history_file",
                target.to_string_lossy().into_owned().into(),
            );
        }
        state.remove("usage_file");
        None
    };
    let next = state.stringify();
    let changed = next != original;
    if changed || enabled {
        let probe = fs::canonicalize(&context.directory)
            .map_err(|e| e.to_string())?
            .join(format!(
                ".usage-probe-{}-{}.json",
                std::process::id(),
                PROBE.fetch_add(1, Ordering::Relaxed)
            ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.access_mode(0x40000000 | 0x00040000);
        }
        let file = options.open(&probe).map_err(|e| e.to_string())?;
        let result = (|| {
            let mut file = file;
            #[cfg(windows)]
            crate::extensions::privacy_windows::protect(&file)?;
            file.write_all(next.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
            drop(file);
            if let Some(native) = native {
                // Preflight against the staged file before publishing the setting.
                let _plugins = crate::plugins::Plugins::load(&[crate::plugins::PluginSpec {
                    path: native,
                    config: probe.to_string_lossy().into_owned(),
                }])
                .map_err(|error| {
                    format!("usage recording preflight failed; configuration unchanged: {error}")
                })?;
            }
            if changed {
                if let Some(metadata) = metadata {
                    fs::set_permissions(&probe, metadata.permissions())
                        .map_err(|e| e.to_string())?;
                }
                control::backup(context, std::slice::from_ref(&path), "usage-recording")?;
                fs::rename(&probe, &path).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                fs::File::open(&context.directory)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| e.to_string())?;
            }
            Ok::<(), String>(())
        })();
        let _ = fs::remove_file(&probe);
        result?;
    }
    control::emit(object([
        ("ok", true.into()),
        ("recording_configured", enabled.into()),
        ("restart_required", changed.into()),
    ]))
}

fn usage(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("summary");
    if matches!(action, "enable" | "disable") {
        if args.len() != 1 {
            return Err("usage enable and disable do not accept options".into());
        }
        return usage_recording(context, action == "enable");
    }
    if !matches!(action, "summary" | "export") {
        return Err("usage expects enable, disable, summary or export".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    let options = args.get(1..).unwrap_or(&[]);
    for pair in options.chunks(2) {
        let name = pair[0].as_str();
        let allowed = if action == "export" {
            name == "--file"
        } else {
            matches!(name, "--model" | "--since" | "--until")
        };
        if !allowed || !seen.insert(name) {
            return Err("unknown or duplicate usage option".into());
        }
        if pair.len() != 2 || pair[1].is_empty() || pair[1].starts_with("--") {
            return Err("usage option requires a value".into());
        }
    }
    let state = routing_state(context)?;
    let recording_configured = usage_file(&state, "usage_file")?.is_some();
    let path = usage_history(context, &state)?;
    if action == "export" {
        let target = control::option(args, "--file")?.ok_or("usage export requires --file")?;
        // A missing source must not leave an empty destination behind.
        let mut input = fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut output = fs::OpenOptions::new();
        output.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            output.mode(0o600);
        }
        let mut output = output.open(target).map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        return control::emit(object([("ok", true.into())]));
    }
    let model = control::option(args, "--model")?;
    let bound = |name| {
        control::option(args, name)?
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{name} requires Unix seconds"))
            })
            .transpose()
    };
    let since = bound("--since")?;
    let until = bound("--until")?;
    if since.zip(until).is_some_and(|(start, end)| start >= end) {
        return Err("--until must be later than --since".into());
    }
    use std::io::{BufRead, BufReader, Read};
    const MAX_LINE_BYTES: usize = 1024 * 1024;
    let mut requests = 0u64;
    let mut failed = 0u64;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut reasoning = 0u64;
    if path.exists() {
        let mut reader = BufReader::new(fs::File::open(&path).map_err(|e| e.to_string())?);
        let mut line = Vec::new();
        loop {
            line.clear();
            // Allow the content limit plus CRLF, without allocating an entire
            // oversized record before discovering that it should be skipped.
            let count = (&mut reader)
                .take((MAX_LINE_BYTES + 2) as u64)
                .read_until(b'\n', &mut line)
                .map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            } else if count == MAX_LINE_BYTES + 2 {
                reader.skip_until(b'\n').map_err(|e| e.to_string())?;
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                continue;
            }
            // Retain the existing error for invalid UTF-8 in retained records.
            std::str::from_utf8(&line)
                .map_err(|_| "stream did not contain valid UTF-8".to_owned())?;
            if let Ok(v) = json::parse(&line) {
                if v.as_object().is_none() {
                    continue;
                }
                let requested = v.get("req").as_str().or_else(|| v.get("model").as_str());
                if model.is_some_and(|model| requested != Some(model)) {
                    continue;
                }
                if since.is_some() || until.is_some() {
                    let Some(time) = usage_timestamp(&v) else {
                        continue;
                    };
                    if since.is_some_and(|start| time < start)
                        || until.is_some_and(|end| time >= end)
                    {
                        continue;
                    }
                }
                requests += 1;
                if v.get("status").as_u64().is_some_and(|v| v >= 400) {
                    failed += 1;
                }
                input = input.saturating_add(
                    v.get("input_tokens")
                        .as_u64()
                        .or_else(|| v.get("in").as_u64())
                        .unwrap_or(0),
                );
                output = output.saturating_add(
                    v.get("output_tokens")
                        .as_u64()
                        .or_else(|| v.get("out").as_u64())
                        .unwrap_or(0),
                );
                cache_read = cache_read.saturating_add(
                    v.get("cache_read_tokens")
                        .as_u64()
                        .or_else(|| v.get("cache_read").as_u64())
                        .unwrap_or(0),
                );
                cache_write = cache_write.saturating_add(
                    v.get("cache_write_tokens")
                        .as_u64()
                        .or_else(|| v.get("cache_write").as_u64())
                        .unwrap_or(0),
                );
                reasoning = reasoning.saturating_add(
                    v.get("reasoning_tokens")
                        .as_u64()
                        .or_else(|| v.get("reasoning").as_u64())
                        .unwrap_or(0),
                );
            }
        }
    }
    control::emit(object([
        ("requests", requests.into()),
        ("failed", failed.into()),
        ("input_tokens", input.into()),
        ("output_tokens", output.into()),
        ("cache_read_tokens", cache_read.into()),
        ("cache_write_tokens", cache_write.into()),
        ("reasoning_tokens", reasoning.into()),
        ("recording_configured", recording_configured.into()),
        ("source", path.to_string_lossy().into_owned().into()),
    ]))
}

// Native records use Unix seconds; migrated Magpie rows use RFC 3339 in `t`.
fn usage_timestamp(row: &Value) -> Option<u64> {
    if let Some(time) = row.get("time").as_u64().or_else(|| row.get("t").as_u64()) {
        return Some(time);
    }
    let value = row.get("t").as_str().or_else(|| row.get("time").as_str())?;
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let part = |start, end| -> Option<i64> {
        let bytes = bytes.get(start..end)?;
        if !bytes.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(bytes).ok()?.parse().ok()
    };
    let (mut year, month, day) = (part(0, 4)?, part(5, 7)?, part(8, 10)?);
    let (hour, minute, second) = (part(11, 13)?, part(14, 16)?, part(17, 19)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut end = 19;
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        let begin = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if begin == end {
            return None;
        }
    }
    let zone = bytes.get(end..)?;
    let offset = if matches!(zone, b"Z" | b"z") {
        0
    } else if zone.len() == 6 && matches!(zone[0], b'+' | b'-') && zone[3] == b':' {
        let (hours, minutes) = (part(end + 1, end + 3)?, part(end + 4, end + 6)?);
        if hours > 23 || minutes > 59 {
            return None;
        }
        (hours * 3600 + minutes * 60) * if zone[0] == b'-' { -1 } else { 1 }
    } else {
        return None;
    };
    year -= i64::from(month <= 2);
    let era = year.div_euclid(400);
    let y = year - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * m + 2) / 5 + day - 1;
    let epoch_days = era * 146097 + y * 365 + y / 4 - y / 100 + day_of_year - 719468;
    u64::try_from(epoch_days * 86400 + hour * 3600 + minute * 60 + second - offset).ok()
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
