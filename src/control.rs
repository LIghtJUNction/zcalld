//! On-demand configuration and service control. Never loaded as a separate daemon.
mod service_install;
use crate::{
    config, http,
    state_json::{self as json, Value, object},
    transport,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct Context {
    pub path: PathBuf,
    pub directory: PathBuf,
    pub home: PathBuf,
}

pub fn run(input: &[String]) -> Option<Result<(), String>> {
    let mut args = Vec::new();
    let mut explicit = None;
    let mut config_count = 0;
    let mut json_count = 0;
    let mut invalid_config_value = false;
    let mut index = 0;
    while index < input.len() {
        match input[index].as_str() {
            "--json" => json_count += 1,
            "--config" | "-c" => {
                index += 1;
                if index >= input.len() {
                    return Some(Err("--config requires a path".into()));
                }
                config_count += 1;
                invalid_config_value |= input[index].starts_with('-');
                explicit = Some(PathBuf::from(&input[index]));
            }
            _ => args.push(input[index].clone()),
        }
        index += 1;
    }
    let command = args.first().map(String::as_str).unwrap_or("");
    if command == "install" {
        if config_count != 0 || json_count > 1 {
            return Some(Err(
                "install does not accept --config or duplicate --json".into()
            ));
        }
        return Some(crate::installer::run(&args[1..]));
    }
    if command == "keys" && (config_count > 1 || json_count > 1 || invalid_config_value) {
        return Some(Err("duplicate or invalid global caller key option".into()));
    }
    if !matches!(
        command,
        "status"
            | "service"
            | "agents"
            | "privacy"
            | "desktop"
            | "profiles"
            | "migrate"
            | "update"
            | "accounts"
            | "groups"
            | "library"
            | "usage"
            | "routes"
            | "keys"
    ) && !(matches!(command, "providers" | "plugins") && args.len() > 1)
    {
        return None;
    }
    Some((|| {
        let path = explicit.map(Ok).unwrap_or_else(config::default_path)?;
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let home = home()?;
        let context = Context {
            path,
            directory,
            home,
        };
        match command {
            "status" => emit(status(&context)),
            "providers" => providers(&context, &args[1..]),
            "keys" => crate::keys::run(&context, &args[1..]),
            "service" => service(&context, &args[1..]),
            "agents" => crate::adapters::run(&context, &args[1..]),
            "privacy" => privacy(&context, &args[1..]),
            "desktop" => crate::desktop::run(&context, &args[1..]),
            "profiles" => profiles(&context, &args[1..]),
            "plugins" => crate::extensions::run(&args[1..], &context.path),
            "migrate" => crate::migration::run(&context, &args[1..]),
            _ => crate::maintenance::run(&context, &args),
        }
    })())
}

pub fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot determine user home directory".into())
}
pub fn emit(value: Value) -> Result<(), String> {
    println!("{}", value.stringify());
    Ok(())
}
pub fn option<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>, String> {
    let mut values = args.iter().enumerate().filter(|(_, v)| v.as_str() == name);
    let value = values
        .next()
        .map(|(i, _)| {
            args.get(i + 1)
                .filter(|v| !v.starts_with("--"))
                .map(String::as_str)
                .ok_or_else(|| format!("{name} requires a value"))
        })
        .transpose()?;
    if values.next().is_some() {
        return Err(format!("duplicate {name}"));
    }
    Ok(value)
}
pub fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|x| x == name)
}
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(|c| c.is_control() || "[]".contains(c))
}
pub fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && !value.starts_with('.')
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn read_text(path: &Path) -> Result<String, String> {
    let size = fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > 8 * 1024 * 1024 {
        return Err("configuration exceeds 8 MiB".into());
    }
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}
pub fn config_text(context: &Context) -> Result<String, String> {
    if context.path.exists() {
        read_text(&context.path)
    } else {
        Ok(format!(
            "[server]\nlisten = 127.0.0.1:9415\n{}workers = 4\nqueue = 16\n",
            if cfg!(unix) {
                "unix_socket = auto\n"
            } else {
                ""
            }
        ))
    }
}
fn section_header(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix('[')?
        .strip_suffix(']')
        .map(str::trim)
}

pub fn sections(text: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let mut out: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = section_header(line) {
            out.push((section.into(), BTreeMap::new()));
        } else if !line.starts_with(['#', ';'])
            && let Some((key, value)) = line.split_once('=')
            && let Some((_, fields)) = out.last_mut()
        {
            let value = value.trim();
            let value = if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                &value[1..value.len() - 1]
            } else {
                value
            };
            fields.insert(key.trim().into(), value.into());
        }
    }
    out
}
pub fn ini_get(text: &str, section: &str, key: &str) -> Option<String> {
    sections(text)
        .into_iter()
        .find(|(s, _)| s == section)
        .and_then(|(_, mut v)| v.remove(key))
}
pub fn ini_set(text: &str, section: &str, key: &str, value: &str) -> Result<String, String> {
    if section.chars().any(|c| c.is_control() || "[]".contains(c))
        || key.chars().any(|c| c.is_control() || "=[]".contains(c))
        || value.chars().any(char::is_control)
    {
        return Err("invalid configuration field".into());
    }
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let header = format!("[{section}]");
    let start = if let Some(i) = lines
        .iter()
        .position(|v| section_header(v) == Some(section))
    {
        i
    } else {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(header);
        lines.len() - 1
    };
    let end = (start + 1..lines.len())
        .find(|i| section_header(&lines[*i]).is_some())
        .unwrap_or(lines.len());
    if let Some(i) = (start + 1..end).find(|i| {
        lines[*i]
            .split_once('=')
            .is_some_and(|(k, _)| k.trim() == key)
    }) {
        lines[i] = format!("{key} = {value}");
    } else {
        lines.insert(end, format!("{key} = {value}"));
    }
    Ok(lines.join("\n") + "\n")
}
pub fn ini_remove(text: &str, section: &str) -> String {
    let mut removing = false;
    let mut out = String::new();
    for line in text.lines() {
        if let Some(header) = section_header(line) {
            removing = header == section;
        }
        if !removing {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Keeps exact original bytes once per operation, outside the source repository.
pub fn backup(context: &Context, paths: &[PathBuf], label: &str) -> Result<PathBuf, String> {
    if !safe_id(label) {
        return Err("invalid backup label".into());
    }
    let dir = context.directory.join("backups").join(format!(
        "{}-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        label
    ));
    let mut records = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let exists = path.exists();
        let copy = dir.join(format!("{i}.original"));
        if exists {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            json::atomic_write(&copy, &data)?;
        }
        records.push(object([
            ("path", path.to_string_lossy().into_owned().into()),
            ("backup", copy.to_string_lossy().into_owned().into()),
            ("existed", exists.into()),
        ]));
    }
    json::atomic_write(
        &dir.join("manifest.json"),
        object([("files", Value::Array(records)), ("created", now().into())])
            .stringify()
            .as_bytes(),
    )?;
    Ok(dir)
}
pub fn save_config(context: &Context, text: &str, label: &str) -> Result<(), String> {
    backup(context, std::slice::from_ref(&context.path), label)?;
    json::atomic_write(&context.path, text.as_bytes())
}

/// Replace private configuration files together, restoring prior bytes on failure.
pub fn replace_files(
    context: &Context,
    updates: &[(PathBuf, Option<Vec<u8>>)],
    label: &str,
) -> Result<(), String> {
    let original = updates
        .iter()
        .map(|(path, _)| match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let paths = updates.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    let saved = backup(context, &paths, label)?;
    let write = |path: &Path, bytes: &Option<Vec<u8>>| -> Result<(), String> {
        match bytes {
            Some(bytes) => json::atomic_write(path, bytes),
            None => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            },
        }
    };
    for (index, (path, value)) in updates.iter().enumerate() {
        if let Err(error) = write(path, value) {
            let mut restored = true;
            for n in (0..index).rev() {
                restored &= write(&updates[n].0, &original[n]).is_ok();
            }
            return Err(format!(
                "{error}; rollback {}; backup {}",
                if restored {
                    "completed"
                } else {
                    "requires recovery"
                },
                saved.display()
            ));
        }
    }
    Ok(())
}

pub fn command(
    program: &str,
    args: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<(i32, String, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let reader = |pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(2 * 1024 * 1024).read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out_thread = reader(Box::new(out));
    let err_thread = reader(Box::new(err));
    if let Some(input) = input
        && let Some(mut pipe) = child.stdin.take()
        && let Err(e) = pipe.write_all(input)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{program} input failed: {e}"));
    }
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok((
        status.code().unwrap_or(-1),
        out_thread.join().unwrap_or_default(),
        err_thread.join().unwrap_or_default(),
    ))
}

fn service_state() -> String {
    #[cfg(target_os = "linux")]
    {
        command(
            "systemctl",
            &["--user".into(), "is-active".into(), "zcalld.service".into()],
            None,
            Duration::from_secs(3),
        )
        .map(|(_, o, _)| o.trim().into())
        .unwrap_or_else(|_| "unavailable".into())
    }
    #[cfg(target_os = "macos")]
    {
        let Ok((0, uid, _)) = command("id", &["-u".into()], None, Duration::from_secs(2)) else {
            return "unavailable".into();
        };
        match command(
            "launchctl",
            &[
                "print".into(),
                format!("gui/{}/io.project-z.zcalld", uid.trim()),
            ],
            None,
            Duration::from_secs(3),
        ) {
            Ok((0, out, _)) if out.lines().any(|line| line.trim() == "state = running") => {
                "active".into()
            }
            Ok((0, _, _)) => "inactive".into(),
            Ok(_) => "not-registered".into(),
            Err(_) => "unavailable".into(),
        }
    }
    #[cfg(windows)]
    {
        match command("powershell.exe", &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
            "$t=Get-ScheduledTask -TaskName 'ProjectZ-zcalld' -ErrorAction SilentlyContinue; if ($null -eq $t) {'not-registered'} elseif ($t.State -eq 'Running') {'active'} else {'inactive'}".into()],
            None, Duration::from_secs(5)) {
            Ok((0, output, _)) => output.trim().into(),
            _ => "unavailable".into(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        "unsupported".into()
    }
}
pub fn status(context: &Context) -> Value {
    let state = service_state();
    let (configuration_path, configuration_exists) = configuration_info(&context.path);
    let (service_install_supported, service_installed) = service_install::information(context);
    match config::Config::load(&context.path) {
        Ok(c) => {
            let healthy = transport::connect(&c)
                .and_then(|mut stream| {
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    stream.write_all(
                        b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )?;
                    let mut buf = [0; 128];
                    let n = stream.read(&mut buf)?;
                    Ok(buf[..n].starts_with(b"HTTP/1.1 200 "))
                })
                .unwrap_or(false);
            object([
                ("version", env!("CARGO_PKG_VERSION").into()),
                ("healthy", healthy.into()),
                ("configured", true.into()),
                ("configuration_path", configuration_path.into()),
                ("configuration_exists", configuration_exists.into()),
                (
                    "service_install_supported",
                    service_install_supported.into(),
                ),
                ("service_installed", service_installed),
                (
                    "listen",
                    c.listen.map(|a| a.to_string()).unwrap_or_default().into(),
                ),
                (
                    "socket",
                    c.unix_socket
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                        .into(),
                ),
                ("service", state.into()),
                ("providers", c.providers.len().into()),
                ("routes", c.routes.len().into()),
            ])
        }
        Err(error) => object([
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("healthy", false.into()),
            ("configured", false.into()),
            ("configuration_path", configuration_path.into()),
            ("configuration_exists", configuration_exists.into()),
            (
                "service_install_supported",
                service_install_supported.into(),
            ),
            ("service_installed", service_installed),
            ("listen", "".into()),
            ("socket", "".into()),
            ("service", state.into()),
            ("error", error.into()),
        ]),
    }
}

fn configuration_info(path: &Path) -> (String, bool) {
    let absolute = std::path::absolute(path)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_default();
    // A dangling symlink, unreadable node or non-directory parent is not
    // permission to initialize over an existing user choice.
    let exists = absolute.is_empty()
        || !matches!(
            fs::symlink_metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        );
    (absolute, exists)
}

fn credential_ready(context: &Context, name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty())
        || fs::read_to_string(context.directory.join("credentials"))
            .unwrap_or_default()
            .lines()
            .any(|line| {
                line.split_once('=').is_some_and(|(k, v)| {
                    k.trim() == name && !v.trim().trim_matches(['\'', '"']).is_empty()
                })
            })
}
pub fn gateway_url(context: &Context) -> Result<String, String> {
    let text = config_text(context)?;
    let listen = ini_get(&text, "server", "listen").unwrap_or_else(|| "127.0.0.1:9415".into());
    let address: std::net::SocketAddr = listen
        .parse()
        .map_err(|_| "agent configuration requires a TCP listener")?;
    Ok(format!("http://{}", transport::client_address(address)))
}
pub fn gateway_token(context: &Context) -> Result<String, String> {
    let c = config::Config::load(&context.path)?;
    Ok(c.token.unwrap_or_else(|| "zcalld".into()))
}

/// Keep an exclusive existing reference; otherwise allocate a private slot.
/// Raw name bytes distinguish punctuation and case without a hash collision.
fn provider_credential_env(
    text: &str,
    name: &str,
    credentials: &BTreeMap<String, String>,
) -> String {
    let section = format!("provider {name}");
    let parsed = sections(text);
    let mut referenced = std::collections::BTreeSet::new();
    for (other_section, fields) in &parsed {
        for (field, value) in fields {
            if (field.ends_with("_env") || field.starts_with("header_env."))
                && !(other_section == &section && field == "api_key_env")
            {
                referenced.insert(value.as_str());
            }
        }
    }
    if let Some(existing) = ini_get(text, &section, "api_key_env")
        && config::valid_env_name(&existing)
        && !referenced.contains(existing.as_str())
    {
        return existing;
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut base = String::from("ZCALLD_PROVIDER_");
    for byte in name.bytes() {
        base.push(char::from(HEX[usize::from(byte >> 4)]));
        base.push(char::from(HEX[usize::from(byte & 15)]));
    }
    base.push_str("_KEY");
    let mut selected = base.clone();
    let mut suffix = 2usize;
    while credentials.contains_key(&selected)
        || referenced.contains(selected.as_str())
        || std::env::var_os(&selected).is_some()
    {
        selected = format!("{base}_{suffix}");
        suffix += 1;
    }
    selected
}

fn providers(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let mut text = config_text(context)?;
    match action {
        "list" => {
            let list = sections(&text)
                .into_iter()
                .filter_map(|(section, v)| {
                    section.strip_prefix("provider ").map(|name| {
                        let env = v.get("api_key_env").cloned().unwrap_or_default();
                        object([
                            ("name", name.into()),
                            (
                                "base_url",
                                v.get("base_url").cloned().unwrap_or_default().into(),
                            ),
                            (
                                "protocol",
                                v.get("protocol")
                                    .cloned()
                                    .unwrap_or_else(|| "chat".into())
                                    .into(),
                            ),
                            ("api_key_env", env.clone().into()),
                            (
                                "configured",
                                (env.is_empty() || credential_ready(context, &env)).into(),
                            ),
                            (
                                "plugin",
                                v.get("plugin").cloned().unwrap_or_default().into(),
                            ),
                        ])
                    })
                })
                .collect();
            emit(object([
                ("providers", Value::Array(list)),
                ("oauth_binding", true.into()),
            ]))
        }
        "add" | "edit" => {
            let name = args.get(1).ok_or("provider name required")?;
            if !safe_id(name) {
                return Err(
                    "provider name must use letters, digits, dash, dot or underscore".into(),
                );
            }
            if flag(args, "--api-key-stdin") && option(args, "--api-key-env")?.is_some() {
                return Err("use either --api-key-env or --api-key-stdin, not both".into());
            }
            let mut pending_credentials = None;
            let section = format!("provider {name}");
            let old = sections(&text)
                .into_iter()
                .find(|(s, _)| s == &section)
                .map(|(_, v)| v)
                .unwrap_or_default();
            let url = option(args, "--base-url")?
                .map(str::to_string)
                .or_else(|| old.get("base_url").cloned())
                .ok_or("--base-url required")?;
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || url.contains(['\r', '\n', '@', '?', '#'])
            {
                return Err(
                    "base URL must be HTTP(S) without credentials, query or fragment".into(),
                );
            }
            let protocol = option(args, "--protocol")?
                .map(str::to_string)
                .or_else(|| old.get("protocol").cloned())
                .unwrap_or_else(|| "chat".into());
            let parsed_protocol = crate::protocol::parse_protocol(&protocol)?;
            crate::protocol::endpoint(
                config::parse_ini_value(&url)?,
                parsed_protocol,
                "validation-model",
                false,
            )?;
            text = ini_set(&text, &section, "base_url", &url)?;
            text = ini_set(&text, &section, "protocol", &protocol)?;
            if let Some(plugin) = option(args, "--plugin")? {
                let account = option(args, "--plugin-account")?
                    .ok_or("--plugin-account required for an OAuth provider")?;
                if !valid_name(plugin) || !valid_name(account) {
                    return Err("invalid plugin provider or account ID".into());
                }
                if !old.is_empty() {
                    return Err("a provider with this name already exists".into());
                }
                if flag(args, "--api-key-stdin") || option(args, "--api-key-env")?.is_some() {
                    return Err("a plugin account cannot be combined with an API key".into());
                }
                text = ini_set(&text, &section, "plugin", plugin)?;
                text = ini_set(&text, &section, "plugin_account", account)?;
            } else if option(args, "--plugin-account")?.is_some() {
                return Err("--plugin-account requires --plugin".into());
            }
            if let Some(env) = option(args, "--api-key-env")? {
                if !config::valid_env_name(env) {
                    return Err("invalid environment variable name".into());
                }
                text = ini_set(&text, &section, "api_key_env", env)?;
            }
            if let Some(model) = option(args, "--model")? {
                if !config::valid_model(config::parse_ini_value(model)?) {
                    return Err("model must be nonempty and at most 512 bytes".into());
                }
                let route = option(args, "--route")?
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{name}/{model}"));
                if !valid_name(&route) {
                    return Err("route name contains unsupported characters".into());
                }
                crate::maintenance::check_group_route_mutation(context, [route.as_str()], false)?;
                text = ini_set(&text, &format!("route {route}"), "provider", name)?;
                text = ini_set(&text, &format!("route {route}"), "model", model)?;
            }
            if flag(args, "--api-key-stdin") {
                let mut key = String::new();
                std::io::stdin()
                    .take(16385)
                    .read_to_string(&mut key)
                    .map_err(|e| e.to_string())?;
                let key = key.trim_end_matches(['\r', '\n']);
                if key.is_empty() || key.len() > 16384 || key.chars().any(char::is_control) {
                    return Err("key must be a nonempty single line up to 16 KiB".into());
                }
                let path = context.directory.join("credentials");
                let previous = config::read_credentials(&path)?;
                let credentials = config::parse_credentials(&previous)?;
                let env = provider_credential_env(&text, name, &credentials);
                let mut lines: Vec<String> = previous
                    .lines()
                    .filter(|l| !l.split_once('=').is_some_and(|(k, _)| k.trim() == env))
                    .map(String::from)
                    .collect();
                lines.push(format!("{env}={}", http::quote(key)));
                let content = lines.join("\n") + "\n";
                if content.len() > config::MAX_CREDENTIALS_BYTES {
                    return Err("credentials file exceeds 256 KiB".into());
                }
                pending_credentials = Some((path, content));
                text = ini_set(&text, &section, "api_key_env", &env)?;
            }
            let mut paths = vec![context.path.clone()];
            if let Some((path, _)) = &pending_credentials {
                paths.push(path.clone());
            }
            backup(context, &paths, "provider")?;
            if let Some((path, content)) = &pending_credentials {
                let old = fs::read(path).ok();
                json::atomic_write(path, content.as_bytes())?;
                if let Err(error) = json::atomic_write(&context.path, text.as_bytes()) {
                    if let Some(old) = old {
                        json::atomic_write(path, &old)?;
                    } else {
                        let _ = fs::remove_file(path);
                    }
                    return Err(error);
                }
            } else {
                json::atomic_write(&context.path, text.as_bytes())?;
            }
            emit(object([
                ("ok", true.into()),
                ("name", name.clone().into()),
                ("restart_required", true.into()),
            ]))
        }
        "remove" => {
            let name = args.get(1).ok_or("provider name required")?;
            let section = format!("provider {name}");
            if !sections(&text).iter().any(|(s, _)| s == &section) {
                return Err("provider does not exist".into());
            }
            let routes: Vec<_> = sections(&text)
                .into_iter()
                .filter(|(s, v)| s.starts_with("route ") && v.get("provider") == Some(name))
                .map(|(s, _)| s)
                .collect();
            crate::maintenance::check_group_route_mutation(
                context,
                routes
                    .iter()
                    .filter_map(|section| section.strip_prefix("route ")),
                true,
            )?;
            text = ini_remove(&text, &section);
            for r in &routes {
                text = ini_remove(&text, r);
            }
            save_config(context, &text, "provider-remove")?;
            emit(object([
                ("ok", true.into()),
                ("removed_routes", routes.len().into()),
                ("restart_required", true.into()),
            ]))
        }
        "test" | "models" => {
            let name = args.get(1).ok_or("provider name required")?;
            let c = config::Config::load(&context.path)?;
            let p = c
                .providers
                .iter()
                .find(|p| &p.name == name)
                .ok_or("unknown provider")?;
            if let Some(plugin) = &p.plugin {
                let provider_index = c
                    .providers
                    .iter()
                    .position(|item| item.name == p.name)
                    .unwrap();
                let catalog = json::parse(c.models_json.as_bytes())?;
                let rows = catalog
                    .get("data")
                    .as_array()
                    .iter()
                    .filter(|row| {
                        row.get("id").as_str().is_some_and(|id| {
                            c.routes
                                .iter()
                                .any(|route| route.provider == provider_index && route.name == id)
                        })
                    })
                    .cloned()
                    .collect();
                if action == "models" {
                    return emit(object([
                        ("name", name.clone().into()),
                        ("source", "configured_catalog".into()),
                        (
                            "catalog",
                            object([("object", "list".into()), ("data", Value::Array(rows))]),
                        ),
                    ]));
                }
                let _loaded = crate::plugins::Plugins::load(&c.plugins)?;
                let helper = crate::extensions::helper()?;
                let root = crate::extensions::root(&context.path)?;
                if !root.join("host.json").is_file() {
                    return Err("community provider runtime is not configured".into());
                }
                let (code, out, _) = command(
                    &helper.to_string_lossy(),
                    &[
                        "--root".into(),
                        root.to_string_lossy().into_owned(),
                        "accounts".into(),
                        "list".into(),
                        "--json".into(),
                    ],
                    None,
                    Duration::from_secs(10),
                )?;
                if code != 0 {
                    return Err("cannot validate the private provider account store".into());
                }
                let accounts = json::parse(out.as_bytes())?;
                let account_configured =
                    accounts.get("accounts").as_array().iter().any(|account| {
                        account.get("provider").as_str() == Some(plugin)
                            && account.get("enabled").as_bool() == Some(true)
                            && p.plugin_account
                                .as_deref()
                                .is_none_or(|id| account.get("id").as_str() == Some(id))
                    });
                return emit(object([
                    ("ok", true.into()), ("name", name.clone().into()), ("mode", "local_validation".into()),
                    ("probe_performed", false.into()), ("reachable", Value::Null),
                    ("account_configured", account_configured.into()),
                    ("message", "Local plugin configuration is valid. Network access and generation were not tested.".into()),
                ]));
            }
            let mut base = p.base_url.trim_end_matches('/').to_string();
            for suffix in [
                "/chat/completions",
                "/responses",
                "/messages",
                "/completions",
                "/interactions",
            ] {
                if let Some(prefix) = base.strip_suffix(suffix) {
                    base = prefix.to_owned();
                    break;
                }
            }
            if let Some((prefix, _)) = base.split_once("/models/") {
                base = prefix.to_owned();
            }
            let url = if base.ends_with("/models") {
                base
            } else {
                format!("{base}/models")
            };
            let mut arguments = vec![
                "--silent".into(),
                "--show-error".into(),
                "--max-time".into(),
                "15".into(),
                "--config".into(),
                "-".into(),
                url,
            ];
            let mut secret_config = String::new();
            if let Some(key) = &p.key {
                let (header, prefix) = match p.auth {
                    config::Auth::Bearer => ("Authorization", "Bearer "),
                    config::Auth::ApiKey => ("x-api-key", ""),
                    config::Auth::GoogleKey => ("x-goog-api-key", ""),
                    config::Auth::None => ("", ""),
                };
                if !header.is_empty() {
                    secret_config.push_str(&format!(
                        "header = {}\n",
                        http::quote(&format!("{header}: {prefix}{key}"))
                    ));
                }
            }
            for (header, value) in &p.headers {
                secret_config.push_str(&format!(
                    "header = {}\n",
                    http::quote(&format!("{header}: {value}"))
                ));
            }
            if crate::protocol::protocol_name(p.protocol) == "anthropic"
                && !p
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
            {
                secret_config.push_str("header = \"anthropic-version: 2023-06-01\"\n");
            }
            arguments.extend([
                "--write-out".into(),
                "\nZCALLD_HTTP_STATUS:%{http_code}".into(),
            ]);
            let (code, body, _) = command(
                "curl",
                &arguments,
                Some(secret_config.as_bytes()),
                Duration::from_secs(20),
            )?;
            let (body, status) = body
                .rsplit_once("\nZCALLD_HTTP_STATUS:")
                .ok_or("provider did not return a status")?;
            let status_num = status.trim().parse::<u64>().unwrap_or(0);
            if code != 0 || !(200..300).contains(&status_num) {
                return Err(format!(
                    "provider model discovery returned HTTP {status_num}"
                ));
            }
            if action == "models" {
                let value = json::parse(body.as_bytes())?;
                emit(object([("name", name.clone().into()), ("catalog", value)]))
            } else {
                emit(object([
                    ("ok", true.into()),
                    ("name", name.clone().into()),
                    ("http_status", status_num.into()),
                    (
                        "api_key_env",
                        p.api_key_env
                            .clone()
                            .map(Value::from)
                            .unwrap_or(Value::Null),
                    ),
                ]))
            }
        }
        _ => Err("providers expects list, add, edit, remove, test or models".into()),
    }
}

fn privacy(context: &Context, args: &[String]) -> Result<(), String> {
    let path = crate::extensions::privacy_settings_path(&context.path)?;
    let mut settings = if path.exists() {
        json::read(&path)?
    } else {
        Value::Object(BTreeMap::new())
    };
    let mut restart_required = Value::Null;
    match args.first().map(String::as_str).unwrap_or("show") {
        "show" => {}
        "set" => {
            let mode = args
                .get(1)
                .map(String::as_str)
                .ok_or("privacy mode required: off, secrets, personal")?;
            match mode {
                "off" => {
                    settings.set("zcalldEnabled", false.into());
                    backup(context, std::slice::from_ref(&path), "privacy")?;
                    json::atomic_write(&path, settings.stringify().as_bytes())?;
                    restart_required = false.into();
                }
                "secrets" | "personal" => {
                    settings.set("zcalldEnabled", true.into());
                    settings.set("redact", true.into());
                    settings.set("redactPersonal", (mode == "personal").into());
                    restart_required =
                        crate::extensions::enable_privacy(&context.path, &settings)?.into();
                }
                _ => return Err("privacy mode must be off, secrets or personal".into()),
            }
        }
        _ => return Err("privacy expects show or set".into()),
    }
    let requested = settings.get("zcalldEnabled").as_bool().unwrap_or(true)
        && (settings.get("redact").as_bool().unwrap_or(false)
            || settings.get("redactPersonal").as_bool().unwrap_or(false)
            || !settings.get("redactWords").as_array().is_empty());
    let configured = crate::extensions::privacy_configured(&context.path)?;
    let enabled = requested && configured;
    let personal = settings.get("redactPersonal").as_bool().unwrap_or(false);
    emit(object([
        ("enabled", enabled.into()),
        ("requested_enabled", requested.into()),
        ("configured", configured.into()),
        ("restart_required", restart_required),
        ("runtime_state", "not_checked".into()),
        (
            "mode",
            if !enabled {
                "off"
            } else if personal {
                "personal"
            } else {
                "secrets"
            }
            .into(),
        ),
        ("personal", personal.into()),
        (
            "model_auto",
            (!settings.get("redactModelOff").as_bool().unwrap_or(false)
                && settings
                    .get("redactModelAuto")
                    .as_bool()
                    .unwrap_or_else(|| {
                        settings
                            .get("redactModelName")
                            .as_str()
                            .unwrap_or_default()
                            .is_empty()
                    }))
            .into(),
        ),
        (
            "words_any_case",
            settings
                .get("redactWordsAnyCase")
                .as_bool()
                .unwrap_or(false)
                .into(),
        ),
        (
            "custom_words",
            settings.get("redactWords").as_array().len().into(),
        ),
        ("settings_path", path.to_string_lossy().into_owned().into()),
    ]))
}

fn profiles(context: &Context, args: &[String]) -> Result<(), String> {
    let base = context.directory.join("profiles");
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "list" {
        let list = fs::read_dir(&base)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .map(|e| Value::from(e.file_name().to_string_lossy().into_owned()))
            .collect();
        return emit(object([("profiles", Value::Array(list))]));
    }
    let name = args
        .get(1)
        .filter(|n| safe_id(n))
        .ok_or("safe profile name required")?;
    let target = base.join(name);
    match action {
        "save" | "use" => {
            let saving = action == "save";
            let config_source = if saving {
                context.path.clone()
            } else {
                target.join("config.conf")
            };
            let content = read_text(&config_source)?;
            let mut updates = Vec::new();
            for item in ["credentials", "privacy/settings.json"] {
                let source = if saving {
                    context.directory.join(item)
                } else {
                    target.join(item)
                };
                let destination = if saving {
                    target.join(item)
                } else {
                    context.directory.join(item)
                };
                let bytes = match fs::read(source) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e.to_string()),
                };
                updates.push((destination, bytes));
            }
            updates.push((
                if saving {
                    target.join("config.conf")
                } else {
                    context.path.clone()
                },
                Some(content.into_bytes()),
            ));
            replace_files(
                context,
                &updates,
                if saving {
                    "profile-save"
                } else {
                    "profile-switch"
                },
            )?;
            emit(object([
                ("ok", true.into()),
                ("profile", name.clone().into()),
                ("restart_required", (!saving).into()),
            ]))
        }
        _ => Err("profiles expects list, save or use".into()),
    }
}

fn service(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    if action == "status" {
        return emit(status(context));
    }
    if action == "install" {
        if args.len() != 1 {
            return Err(
                "service install accepts only --config and --json; it is a per-user action".into(),
            );
        }
        return emit(service_install::install(context)?);
    }
    if !matches!(action, "start" | "stop" | "restart" | "enable" | "disable") {
        return Err(
            "service expects status, install, start, stop, restart, enable or disable".into(),
        );
    }
    #[cfg(target_os = "linux")]
    {
        let system = flag(args, "--system");
        let mut cmd = Vec::new();
        if !system {
            cmd.push("--user".into());
        }
        let mut reload = cmd.clone();
        reload.push("daemon-reload".into());
        let (code, _, _) = command("systemctl", &reload, None, Duration::from_secs(10))?;
        if code != 0 {
            return Err("cannot reload the service manager".into());
        }
        cmd.extend([action.into(), "zcalld.service".into()]);
        let (code, _, error) = command("systemctl", &cmd, None, Duration::from_secs(25))?;
        if code != 0 {
            return Err(format!("service {action} failed: {}", error.trim()));
        }
        emit(object([
            ("ok", true.into()),
            ("service", service_state().into()),
        ]))
    }
    #[cfg(target_os = "macos")]
    {
        let plist = context
            .home
            .join("Library/LaunchAgents/io.project-z.zcalld.plist");
        let uid = command("id", &["-u".into()], None, Duration::from_secs(2))?.1;
        let domain = format!("gui/{}", uid.trim());
        if matches!(action, "start" | "restart" | "enable") {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let xml = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
            };
            let content = format!(
                "<?xml version=\"1.0\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>io.project-z.zcalld</string><key>ProgramArguments</key><array><string>{}</string><string>serve</string><string>--config</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>",
                xml(&exe.to_string_lossy()),
                xml(&context.path.to_string_lossy())
            );
            json::atomic_write(&plist, content.as_bytes())?;
        }
        let target = format!("{domain}/io.project-z.zcalld");
        if matches!(action, "stop" | "disable" | "restart") {
            let _ = command(
                "launchctl",
                &["bootout".into(), target],
                None,
                Duration::from_secs(10),
            );
        }
        if matches!(action, "start" | "enable" | "restart") {
            let (code, _, _) = command(
                "launchctl",
                &[
                    "bootstrap".into(),
                    domain,
                    plist.to_string_lossy().into_owned(),
                ],
                None,
                Duration::from_secs(15),
            )?;
            if code != 0 {
                return Err("launchd could not start zcalld".into());
            }
        }
        if action == "disable" {
            let _ = fs::remove_file(plist);
        }
        emit(object([("ok", true.into())]))
    }
    #[cfg(target_os = "windows")]
    {
        let name = "ProjectZ-zcalld";
        if matches!(action, "start" | "restart" | "enable") {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let task = format!(
                "\"{}\" serve --config \"{}\"",
                exe.display(),
                context.path.display()
            );
            let (code, _, _) = command(
                "schtasks",
                &[
                    "/Create".into(),
                    "/F".into(),
                    "/SC".into(),
                    "ONLOGON".into(),
                    "/TN".into(),
                    name.into(),
                    "/TR".into(),
                    task,
                ],
                None,
                Duration::from_secs(15),
            )?;
            if code != 0 {
                return Err("cannot create per-user scheduled service".into());
            }
        }
        if matches!(action, "stop" | "restart" | "disable") {
            let _ = command(
                "schtasks",
                &["/End".into(), "/TN".into(), name.into()],
                None,
                Duration::from_secs(15),
            );
        }
        if matches!(action, "start" | "restart") {
            let (code, _, _) = command(
                "schtasks",
                &["/Run".into(), "/TN".into(), name.into()],
                None,
                Duration::from_secs(15),
            )?;
            if code != 0 {
                return Err("cannot start scheduled service".into());
            }
        }
        if action == "disable" {
            let _ = command(
                "schtasks",
                &["/Delete".into(), "/F".into(), "/TN".into(), name.into()],
                None,
                Duration::from_secs(15),
            );
        }
        emit(object([("ok", true.into())]))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err("service manager unsupported on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ini_edits_keep_unrelated_sections_and_comments() {
        let text =
            "# keep\n[server]\nworkers = 4\n\n[provider keep]\nbase_url = https://example.com/v1\n";
        let next = ini_set(text, "server", "listen", "127.0.0.1:3425").unwrap();
        assert!(next.contains("# keep"));
        assert_eq!(ini_get(&next, "server", "workers").as_deref(), Some("4"));
        assert_eq!(
            ini_get(&next, "provider keep", "base_url").as_deref(),
            Some("https://example.com/v1")
        );
        assert!(!ini_remove(&next, "provider keep").contains("example.com"));
    }
    #[test]
    fn rejects_unsafe_profile_ids() {
        for name in ["../a", "/tmp/a", "a/b", ".hidden", ""] {
            assert!(!safe_id(name));
        }
        assert!(safe_id("work-fast"));
    }
}
