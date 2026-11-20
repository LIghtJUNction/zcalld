//! One installer implementation for the core CLI and the optional GUI bootstrap.
//! It never loads gateway configuration, contacts providers, or starts a service.
mod archive;
pub mod cli;
mod hash;
mod private;
mod process;
mod release;
mod transaction;

// Share the existing bounded owned JSON representation rather than introducing
// another parser or a serialization dependency into the standalone helper.
#[path = "../../zcalld-compat-client/src/json.rs"]
pub mod json;

pub use release::{Plan, default_prefix, host_target, managed_path, plan_official};
pub use transaction::{RuntimeLease, apply, installed_prefix, resume, rollback, runtime_lease, status};
pub use transaction::{resume_wait, spawn_resume};
pub use hash::sha256_file;

pub const REPOSITORY: &str = "LIghtJUNction/zcalld";
pub const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
pub const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
pub const MAX_FILE: u64 = 128 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 2048;
pub const MAX_METADATA: usize = 1024 * 1024;

pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn strict_json(bytes: &[u8], limit: usize) -> Result<json::Value, String> {
    use std::collections::BTreeSet;
    use zconv::json::{Kind, Limits, Token};
    fn check(value: zconv::json::Value<'_, '_>) -> Result<(), String> {
        match value.kind() {
            Kind::Object => {
                let mut keys = BTreeSet::new();
                for (key, value) in value.object() {
                    if !keys.insert(key.bytes().collect::<Vec<_>>()) {
                        return Err("duplicate JSON member".into());
                    }
                    check(value)?;
                }
            }
            Kind::Array => { for value in value.array() { check(value)?; } }
            _ => {}
        }
        Ok(())
    }
    if bytes.len() > limit { return Err("installation metadata exceeds its limit".into()); }
    let mut count = 256;
    loop {
        let mut tokens = vec![Token::EMPTY; count];
        match zconv::json::parse(bytes, &mut tokens, Limits { max_input: limit, ..Limits::default() }) {
            Ok(document) => { check(document.root())?; return json::parse(bytes); }
            Err(zconv::Error::WorkspaceTooSmall { needed }) if count < 262_144 => {
                count = (count * 2).max(needed).min(262_144);
            }
            Err(_) => return Err("invalid installation JSON".into()),
        }
    }
}

fn string(value: &json::Value, name: &str) -> Result<String, String> {
    value.get(name).as_str().map(str::to_owned).ok_or_else(|| format!("missing installation field: {name}"))
}

fn hex(text: &str, count: usize) -> bool {
    text.len() == count && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn version(text: &str) -> bool {
    let parts = text.split('.').collect::<Vec<_>>();
    parts.len() == 3 && parts.iter().all(|v| !v.is_empty() && v.len() <= 8 &&
        (v.len() == 1 || !v.starts_with('0')) && v.bytes().all(|b| b.is_ascii_digit()))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}
