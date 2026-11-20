use std::path::PathBuf;
use zcalld_compat_client::json::{self, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub enabled: bool,
    pub keep_awake: bool,
    pub keep_display: bool,
    pub usage_alert: u32,
    pub balance_alert: f64,
    pub reset_reminder_hours: u32,
    pub turn_done_seconds: u32,
    pub mask_alert: bool,
    pub language: String,
    pub state_dir: PathBuf,
    pub inbox: PathBuf,
    pub record_only: bool,
    pub notification_helper: Option<PathBuf>,
}

fn boolean(v: &Value, name: &str, default: bool) -> Result<bool, String> {
    match v.get(name) {
        Value::Null => Ok(default),
        Value::Bool(b) => Ok(*b),
        _ => Err("invalid desktop boolean".into()),
    }
}
fn integer(v: &Value, name: &str, maximum: u64) -> Result<u32, String> {
    let n = match v.get(name) {
        Value::Null => 0,
        n => n.as_u64().ok_or("invalid desktop integer")?,
    };
    if n > maximum {
        return Err("desktop threshold exceeds limit".into());
    }
    Ok(n as u32)
}
pub fn finite(v: &Value) -> Option<f64> {
    match v {
        Value::Number(s) => s.parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}
pub fn text(v: &Value, maximum: usize) -> Option<&str> {
    v.as_str()
        .filter(|s| s.len() <= maximum && !s.chars().any(char::is_control))
}
fn path(v: &Value, name: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(text(v.get(name), 4096).ok_or("invalid desktop path")?);
    if !p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("desktop paths must be absolute".into());
    }
    Ok(p)
}
impl Config {
    pub fn parse(input: &str) -> Result<Self, String> {
        let bytes = if let Some(file) = input.strip_prefix("config_file=") {
            crate::private::read_config(&PathBuf::from(file))?
        } else {
            input.as_bytes().to_vec()
        };
        if bytes.len() > 16_384 {
            return Err("desktop configuration is too large".into());
        }
        let v = json::parse(&bytes).map_err(|_| "invalid desktop configuration")?;
        if v.as_object().is_none() {
            return Err("desktop configuration must be an object".into());
        }
        let allowed = [
            "enabled",
            "keep_awake",
            "keep_display",
            "usage_alert",
            "balance_alert",
            "reset_reminder_hours",
            "turn_done_seconds",
            "mask_alert",
            "language",
            "state_dir",
            "inbox",
            "backend",
            "notification_helper",
        ];
        if v.as_object()
            .unwrap()
            .keys()
            .any(|k| !allowed.contains(&k.as_str()))
        {
            return Err("unknown desktop configuration field".into());
        }
        let balance = match v.get("balance_alert") {
            Value::Null => 0.0,
            n => finite(n)
                .filter(|n| *n >= 0.0)
                .ok_or("invalid balance threshold")?,
        };
        let record_only = match v.get("backend").as_str() {
            None | Some("native") => false,
            Some("record") => true,
            _ => return Err("unknown desktop backend".into()),
        };
        let language = match text(v.get("language"), 16).unwrap_or("en") {
            "en" | "zh" | "zh-CN" | "zh-TW" | "ja" | "de" => {
                v.get("language").as_str().unwrap_or("en").to_owned()
            }
            _ => return Err("unsupported desktop language".into()),
        };
        let state_dir = path(&v, "state_dir")?;
        let inbox = path(&v, "inbox")?;
        if state_dir == inbox {
            return Err("desktop inbox and state directory must differ".into());
        }
        let notification_helper = if matches!(v.get("notification_helper"), Value::Null) {
            None
        } else {
            Some(path(&v, "notification_helper")?)
        };
        Ok(Self {
            enabled: boolean(&v, "enabled", false)?,
            keep_awake: boolean(&v, "keep_awake", false)?,
            keep_display: boolean(&v, "keep_display", false)?,
            usage_alert: integer(&v, "usage_alert", 100)?,
            balance_alert: balance,
            reset_reminder_hours: integer(&v, "reset_reminder_hours", 24 * 366)?,
            turn_done_seconds: integer(&v, "turn_done_seconds", 86_400)?,
            mask_alert: boolean(&v, "mask_alert", false)?,
            language,
            state_dir,
            inbox,
            record_only,
            notification_helper,
        })
    }
}
