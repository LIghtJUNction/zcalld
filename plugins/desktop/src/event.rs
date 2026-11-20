use crate::config::{finite, text};
use zcalld_compat_client::json::{self, Value};

pub const MAX_BYTES: usize = 16_384;
#[derive(Clone, Debug)]
pub struct Window {
    pub id: String,
    pub pool: String,
    pub used: f64,
    pub span: u64,
    pub resets: Option<u64>,
    pub aside: bool,
}
#[derive(Clone, Debug)]
pub struct Quota {
    pub source: String,
    pub observation: String,
    pub observed: u64,
    pub fresh: bool,
    pub success: bool,
    pub windows: Vec<Window>,
    pub balance: Option<(f64, String)>,
    pub reset_credits: Option<(u64, u64)>,
}
#[derive(Clone, Debug)]
pub struct Mask {
    pub id: String,
    pub agent: String,
    pub counts: Vec<(String, u64)>,
}
#[derive(Clone, Debug)]
pub enum Event {
    Quota(Quota),
    Mask(Mask),
}

fn opaque(v: &Value) -> Result<String, String> {
    let s = text(v, 128)
        .filter(|s| !s.is_empty())
        .ok_or("invalid event identifier")?;
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-:.".contains(&b))
    {
        return Err("invalid event identifier".into());
    }
    Ok(s.to_owned())
}
fn opt_id(v: &Value) -> Result<String, String> {
    if matches!(v, Value::Null) || v.as_str() == Some("") {
        Ok(String::new())
    } else {
        opaque(v)
    }
}
fn flag(v: &Value, default: bool) -> Result<bool, String> {
    if matches!(v, Value::Null) {
        Ok(default)
    } else {
        v.as_bool().ok_or("invalid event flag".into())
    }
}
fn array(v: &Value, maximum: usize) -> Result<&[Value], String> {
    match v {
        Value::Null => Ok(&[]),
        Value::Array(a) if a.len() <= maximum => Ok(a),
        _ => Err("event array exceeds limit".into()),
    }
}
fn optional_u64(v: &Value) -> Result<Option<u64>, String> {
    if matches!(v, Value::Null) {
        Ok(None)
    } else {
        v.as_u64().map(Some).ok_or("invalid event time".into())
    }
}
pub fn parse(kind: u32, input: &[u8]) -> Result<Event, String> {
    if input.len() > MAX_BYTES {
        return Err("event exceeds limit".into());
    }
    let v = json::parse(input).map_err(|_| "invalid event JSON")?;
    parse_value(kind, &v)
}
pub fn envelope(input: &[u8]) -> Result<Event, String> {
    if input.len() > MAX_BYTES {
        return Err("event exceeds limit".into());
    }
    let v = json::parse(input).map_err(|_| "invalid event JSON")?;
    let k = v
        .get("kind")
        .as_u64()
        .filter(|k| *k <= u32::MAX as u64)
        .ok_or("invalid event kind")?;
    parse_value(k as u32, v.get("event"))
}
fn parse_value(kind: u32, v: &Value) -> Result<Event, String> {
    if v.get("version").as_u64() != Some(1) {
        return Err("unsupported event version".into());
    }
    match kind {
        1 => {
            let mut windows = Vec::new();
            for w in array(v.get("windows"), 32)? {
                let used = finite(w.get("used_percent"))
                    .filter(|n| *n >= 0.0)
                    .ok_or("invalid window use")?;
                windows.push(Window {
                    id: opaque(w.get("id"))?,
                    pool: opt_id(w.get("pool"))?,
                    used,
                    span: w.get("span_seconds").as_u64().unwrap_or(0),
                    resets: optional_u64(w.get("reset_at_ms"))?,
                    aside: flag(w.get("aside"), false)?,
                });
            }
            let balance = if matches!(v.get("balance"), Value::Null) {
                None
            } else {
                let b = v.get("balance");
                let n = finite(b.get("amount")).ok_or("invalid balance")?;
                let unit = text(b.get("unit"), 16).ok_or("invalid balance unit")?;
                if !unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                {
                    return Err("invalid balance unit".into());
                }
                Some((n, unit.into()))
            };
            let reset_credits = if matches!(v.get("reset_credits"), Value::Null) {
                None
            } else {
                let c = v.get("reset_credits");
                Some((
                    c.get("count")
                        .as_u64()
                        .filter(|n| *n > 0)
                        .ok_or("invalid reset credit count")?,
                    c.get("expires_at_ms")
                        .as_u64()
                        .ok_or("invalid reset credit expiry")?,
                ))
            };
            Ok(Event::Quota(Quota {
                source: opaque(v.get("source_id"))?,
                observation: opaque(v.get("observation_id"))?,
                observed: v
                    .get("observed_at_ms")
                    .as_u64()
                    .ok_or("invalid observation time")?,
                fresh: flag(v.get("fresh"), false)?,
                success: flag(v.get("success"), false)?,
                windows,
                balance,
                reset_credits,
            }))
        }
        2 => {
            let mut counts = Vec::new();
            for c in array(v.get("counts"), 32)? {
                let kind = opaque(c.get("kind"))?;
                let n = c
                    .get("new_values")
                    .as_u64()
                    .filter(|n| *n > 0 && *n <= 1_000_000)
                    .ok_or("invalid mask count")?;
                if counts.iter().any(|(k, _)| *k == kind) {
                    return Err("duplicate mask kind".into());
                }
                counts.push((kind, n));
            }
            Ok(Event::Mask(Mask {
                id: opaque(v.get("event_id"))?,
                agent: opt_id(v.get("agent"))?,
                counts,
            }))
        }
        _ => Err("unsupported event kind".into()),
    }
}
