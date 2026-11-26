//! Opt-in desktop settings and exact migration of existing desktop preferences.
//! Neither inspection nor validation activates OS notifications or inhibitors.
use crate::{
    control::{self, Context},
    digest,
    plugins::{PluginSpec, Plugins},
    state_json::{self as json, Value, object},
};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
};
const MAX_CONFIG: usize = 16_384;

fn directory(config: &Path) -> Result<PathBuf, String> {
    Ok(crate::extensions::root(config)?
        .parent()
        .ok_or("desktop configuration parent missing")?
        .to_path_buf())
}
fn path(config: &Path) -> Result<PathBuf, String> {
    Ok(directory(config)?.join("desktop.json"))
}
fn read_bytes(path: &Path, maximum: usize, private: bool) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot inspect desktop settings".into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum as u64 {
        return Err("desktop settings must be a bounded regular file".into());
    }
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("desktop settings must be private (mode 0600)".into());
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let file = File::open(path).map_err(|_| "cannot read desktop settings")?;
    let opened = file
        .metadata()
        .map_err(|_| "cannot inspect opened desktop settings")?;
    if !opened.is_file() || opened.len() > maximum as u64 {
        return Err("desktop settings changed during read".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err("desktop settings changed during read".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read desktop settings")?;
    if bytes.len() > maximum {
        return Err("desktop settings exceed limit".into());
    }
    Ok(Some(bytes))
}
fn read(config: &Path) -> Result<Option<Value>, String> {
    read_bytes(&path(config)?, MAX_CONFIG, true)?
        .map(|bytes| json::parse_with_limit(&bytes, MAX_CONFIG))
        .transpose()
}
fn absolute(value: &Value, field: &str) -> Result<PathBuf, String> {
    let text = value.get(field).as_str().ok_or("desktop path is missing")?;
    let path = PathBuf::from(text);
    if text.len() > 4096
        || text.chars().any(char::is_control)
        || !path.is_absolute()
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(
            "desktop state and inbox paths must be absolute without parent traversal".into(),
        );
    }
    Ok(path)
}
#[cfg(any(windows, target_os = "macos"))]
fn notification_helper() -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "zcalld-desktop-notify.exe";
    #[cfg(target_os = "macos")]
    let name = "zcalld-desktop-notify.app/Contents/MacOS/zcalld-desktop-notify";
    let executable = std::env::current_exe().ok()?;
    let bin = executable.parent()?;
    for prefix in [bin.parent(), Some(bin)].into_iter().flatten() {
        let path = prefix.join("libexec/zcalld").join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink()) {
            return Some(path);
        }
    }
    None
}
fn defaults(mut value: Value, config: &Path) -> Result<Value, String> {
    if value.as_object().is_none() {
        return Err("desktop settings must be an object".into());
    }
    let base = directory(config)?.join("desktop");
    for (field, child) in [("state_dir", "state"), ("inbox", "inbox")] {
        if !value.as_object().unwrap().contains_key(field) {
            value.set(
                field,
                base.join(child).to_string_lossy().into_owned().into(),
            );
        }
        absolute(&value, field)?;
    }
    #[cfg(any(windows, target_os = "macos"))]
    if !value
        .as_object()
        .unwrap()
        .contains_key("notification_helper")
        && let Some(helper) = notification_helper()
    {
        value.set(
            "notification_helper",
            helper.to_string_lossy().into_owned().into(),
        );
    }
    if !value.as_object().unwrap().contains_key("enabled") {
        value.set("enabled", false.into());
    }
    if value.get("enabled").as_bool().is_none() {
        return Err("desktop enabled must be boolean".into());
    }
    if value.stringify().len() > MAX_CONFIG {
        return Err("desktop settings exceed limit".into());
    }
    Ok(value)
}
fn spec(value: &Value) -> Result<PluginSpec, String> {
    Ok(PluginSpec {
        provider_id: None,
        path: crate::extensions::library("desktop")?,
        config: value.stringify(),
    })
}
fn preflight(value: &Value) -> Result<Value, String> {
    let mut plugins = Plugins::load(&[spec(value)?])?;
    if !plugins.has_events() {
        return Err("installed desktop plugin does not support request events".into());
    }
    let status = plugins.event_status()?;
    if status.len() != 1 {
        return Err("desktop plugin returned no static capabilities".into());
    }
    json::parse_with_limit(status[0].as_bytes(), MAX_CONFIG)
}

/// Config loading adds no plugin or per-request work unless explicitly enabled.
pub fn plugin(config: &Path) -> Result<Option<PluginSpec>, String> {
    let Some(value) = read(config)? else {
        return Ok(None);
    };
    let value = defaults(value, config)?;
    if value.get("enabled").as_bool() != Some(true) {
        return Ok(None);
    }
    Ok(Some(spec(&value)?))
}
/// Only explicit opt-in enables the privacy worker's bounded notification inbox.
pub fn inbox(config: &Path) -> Result<Option<PathBuf>, String> {
    let Some(value) = read(config)? else {
        return Ok(None);
    };
    if value.get("enabled").as_bool() != Some(true)
        || value.get("mask_alert").as_bool() != Some(true)
    {
        return Ok(None);
    }
    Ok(Some(absolute(&defaults(value, config)?, "inbox")?))
}
fn private_dir(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("desktop state path is not a regular directory".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("desktop state directory must be private (mode 0700)".into());
            }
        }
    } else {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(path)
            .map_err(|_| "cannot create private desktop state directory")?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .access_mode(0x0002_0000 | 0x0004_0000)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(path)
            .map_err(|_| "cannot protect desktop directory")?;
        crate::state_json::privacy_windows::protect(&file)?;
    }
    Ok(())
}
fn lock(config: &Path) -> Result<File, String> {
    let parent = directory(config)?;
    private_dir(&parent)?;
    let path = parent.join(".desktop.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err("desktop settings lock is invalid".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.access_mode(0x8000_0000 | 0x4000_0000 | 0x0004_0000);
    }
    let file = options
        .open(path)
        .map_err(|_| "cannot open desktop settings lock")?;
    #[cfg(windows)]
    crate::state_json::privacy_windows::protect(&file)?;
    file.try_lock()
        .map_err(|_| "desktop settings are being edited; retry after that operation")?;
    Ok(file)
}
fn write(context: &Context, value: &Value) -> Result<(), String> {
    // Preflight is read-only: no notification, inhibitor, inbox consumption or
    // background thread. Make runtime paths private before publishing settings.
    preflight(value)?;
    if value.get("enabled").as_bool() == Some(true) {
        private_dir(&absolute(value, "state_dir")?)?;
        private_dir(&absolute(value, "inbox")?)?;
    }
    control::replace_files(
        context,
        &[(path(&context.path)?, Some(value.stringify().into_bytes()))],
        "desktop",
    )?;
    control::emit(object([
        ("ok", true.into()),
        ("enabled", value.get("enabled").clone()),
        ("restart_required", true.into()),
    ]))
}
fn boolean(settings: &Value, source: &str) -> Result<bool, String> {
    match settings.as_object().and_then(|v| v.get(source)) {
        None => Ok(false),
        Some(Value::Bool(v)) => Ok(*v),
        _ => Err("source desktop boolean is malformed".into()),
    }
}
fn threshold(settings: &Value, source: &str, maximum: u64) -> Result<u64, String> {
    match settings.as_object().and_then(|v| v.get(source)) {
        None => Ok(0),
        Some(v) => v
            .as_u64()
            .filter(|v| *v <= maximum)
            .ok_or_else(|| "source desktop threshold is malformed".into()),
    }
}
fn source_preferences(settings: &Value) -> Result<Value, String> {
    if settings.as_object().is_none() {
        return Err("source settings must be an object".into());
    }
    let keep = boolean(settings, "keepAwake")?;
    let display = boolean(settings, "keepAwakeDisplay")?;
    let usage = threshold(settings, "usageAlert", 100)?;
    let reset = threshold(settings, "resetReminder", 24 * 366)?;
    let turn = threshold(settings, "turnDoneAlert", 86_400)?;
    let mask = boolean(settings, "maskAlert")?;
    let balance = match settings.as_object().unwrap().get("balanceAlert") {
        None => Value::Number("0".into()),
        Some(Value::Number(s)) if s.parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0) => {
            Value::Number(s.clone())
        }
        _ => return Err("source balance threshold is malformed".into()),
    };
    let positive_balance =
        matches!(&balance, Value::Number(s) if s.parse::<f64>().is_ok_and(|n| n > 0.0));
    let language = match settings.as_object().unwrap().get("language") {
        None => "en",
        Some(Value::String(s))
            if ["en", "zh", "zh-CN", "zh-TW", "ja", "de"].contains(&s.as_str()) =>
        {
            s
        }
        _ => return Err("source desktop language is unsupported".into()),
    };
    Ok(object([
        (
            "enabled",
            (keep || usage > 0 || reset > 0 || turn > 0 || mask || positive_balance).into(),
        ),
        ("keep_awake", keep.into()),
        ("keep_display", display.into()),
        ("usage_alert", usage.into()),
        ("balance_alert", balance),
        ("reset_reminder_hours", reset.into()),
        ("turn_done_seconds", turn.into()),
        ("mask_alert", mask.into()),
        ("language", language.into()),
    ]))
}
fn plan(context: &Context, source: &str) -> Result<(Value, String), String> {
    let path = Path::new(source);
    if !path.is_absolute() {
        return Err("desktop source must be an absolute Magpie configuration directory".into());
    }
    let source = fs::canonicalize(path).map_err(|_| "desktop source directory is unavailable")?;
    let bytes = read_bytes(&source.join("settings.json"), 8 << 20, false)?
        .ok_or("source settings.json is missing")?;
    let settings = json::parse(&bytes)?;
    let preferences = defaults(source_preferences(&settings)?, &context.path)?;
    let previous = read_bytes(&self::path(&context.path)?, MAX_CONFIG, true)?;
    let previous = previous
        .as_deref()
        .map(digest::sha256)
        .map(Value::from)
        .unwrap_or(Value::Null);
    let fingerprint = object([
        ("version", 1u64.into()),
        ("source", source.to_string_lossy().into_owned().into()),
        ("settings", preferences.clone()),
        ("previous", previous),
    ]);
    let hash = digest::sha256(fingerprint.stringify().as_bytes());
    Ok((preferences, hash))
}
fn arguments<'a>(
    args: &'a [String],
    allowed: &[&str],
) -> Result<std::collections::BTreeMap<&'a str, &'a str>, String> {
    let mut result = std::collections::BTreeMap::new();
    let mut i = 1;
    while i < args.len() {
        let name = args[i].as_str();
        if !allowed.contains(&name) || result.contains_key(name) {
            return Err("unknown or duplicate desktop option".into());
        }
        i += 1;
        let value = args
            .get(i)
            .filter(|v| !v.starts_with("--"))
            .ok_or("desktop option requires a value")?;
        result.insert(name, value.as_str());
        i += 1;
    }
    Ok(result)
}
pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("show");
    match action {
        "show" => {
            arguments(args, &[])?;
            let value = read(&context.path)?.map(|v| defaults(v, &context.path)).transpose()?
                .unwrap_or(defaults(object([("enabled", false.into())]), &context.path)?);
            let (capabilities, error) = match preflight(&value) {
                Ok(v) => (v, Value::Null),
                Err(_) => (Value::Null, "desktop plugin unavailable or settings invalid".into()),
            };
            control::emit(object([("settings", value), ("capabilities", capabilities),
                ("scope", "configuration; not live daemon counters".into()), ("error", error)]))
        }
        "configure" => {
            let opts = arguments(args, &["--file"])?;
            let input = opts.get("--file").ok_or("desktop configure requires --file PATH")?;
            let bytes = read_bytes(Path::new(input), MAX_CONFIG, false)?.ok_or("desktop settings file missing")?;
            let value = defaults(json::parse_with_limit(&bytes, MAX_CONFIG)?, &context.path)?;
            let _guard = lock(&context.path)?;
            write(context, &value)
        }
        "disable" => {
            arguments(args, &[])?;
            let _guard = lock(&context.path)?;
            let mut value = defaults(read(&context.path)?.unwrap_or(object([])), &context.path)?;
            value.set("enabled", false.into());
            // Disabling remains possible if an optional library was uninstalled.
            control::replace_files(context, &[(path(&context.path)?, Some(value.stringify().into_bytes()))], "desktop")?;
            control::emit(object([("ok", true.into()), ("enabled", false.into()), ("restart_required", true.into())]))
        }
        "plan" | "sync" => {
            let opts = arguments(args, if action == "plan" { &["--source"] } else { &["--source", "--expect-plan"] })?;
            let source = opts.get("--source").ok_or("desktop plan/sync requires --source PATH")?;
            if action == "plan" {
                let (value, hash) = plan(context, source)?;
                return control::emit(object([("plan_hash", hash.into()), ("settings", value), ("writes", false.into())]));
            }
            let expected = opts.get("--expect-plan").ok_or("desktop sync requires --expect-plan HASH")?;
            let _guard = lock(&context.path)?;
            let (value, hash) = plan(context, source)?;
            if *expected != hash {
                return Err("desktop plan changed; inspect a new plan before synchronizing".into());
            }
            write(context, &value)
        }
        _ => Err("desktop expects show, configure --file PATH, disable, plan --source PATH, or sync --source PATH --expect-plan HASH".into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_preferences_are_exact_and_do_not_copy_unrelated_private_fields() {
        let settings = json::parse(br#"{"keepAwake":true,"keepAwakeDisplay":true,"usageAlert":80,"balanceAlert":3.5,"resetReminder":24,"turnDoneAlert":30,"maskAlert":true,"language":"zh-CN","token":"private","codexNoCredits":{"private":true}}"#).unwrap();
        let result = source_preferences(&settings).unwrap();
        assert_eq!(result.get("usage_alert").as_u64(), Some(80));
        assert_eq!(result.get("balance_alert").stringify(), "3.5");
        assert_eq!(result.get("keep_display").as_bool(), Some(true));
        assert_eq!(result.get("reset_reminder_hours").as_u64(), Some(24));
        assert!(!result.stringify().contains("private"));
        assert_eq!(
            source_preferences(&object([]))
                .unwrap()
                .get("enabled")
                .as_bool(),
            Some(false)
        );
        let longest =
            source_preferences(&json::parse(br#"{"resetReminder":8784}"#).unwrap()).unwrap();
        assert_eq!(longest.get("reset_reminder_hours").as_u64(), Some(8784));
        for input in [
            r#"{"keepAwake":null}"#,
            r#"{"usageAlert":101}"#,
            r#"{"usageAlert":-1}"#,
            r#"{"balanceAlert":"3"}"#,
            r#"{"balanceAlert":1e999}"#,
            r#"{"resetReminder":8785}"#,
            r#"{"turnDoneAlert":86401}"#,
        ] {
            assert!(source_preferences(&json::parse(input.as_bytes()).unwrap()).is_err());
        }
    }
    #[test]
    fn paths_and_command_options_reject_ambiguous_input() {
        let cfg = std::env::temp_dir().join("zcalld-desktop/config.conf");
        let value = defaults(object([]), &cfg).unwrap();
        assert!(absolute(&value, "inbox").unwrap().is_absolute());
        assert_eq!(value.get("enabled").as_bool(), Some(false));
        let mut bad = value.clone();
        bad.set("inbox", "relative".into());
        assert!(defaults(bad, &cfg).is_err());
        let args = ["configure", "--file", "one", "--file", "two"].map(String::from);
        assert!(arguments(&args, &["--file"]).is_err());
    }
}
