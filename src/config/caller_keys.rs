//! Private inbound gateway credentials, separate from every upstream credential.
use crate::{http, state_json::Value};
use std::{fs, io::Read, path::Path};

#[derive(Clone)]
pub struct CallerKeys {
    keys: Vec<CallerKey>,
}

#[derive(Clone)]
struct CallerKey {
    secret: String,
    lan: bool,
}

impl CallerKeys {
    pub fn allows_lan(&self) -> bool {
        self.keys.iter().any(|key| key.lan)
    }

    pub fn permits(&self, supplied: &str, loopback: bool) -> bool {
        // Examine every key even after one matches. No key metadata or matched
        // index is sent upstream or returned to the caller.
        let mut accepted = false;
        for key in &self.keys {
            let matches = http::constant_eq(supplied.as_bytes(), key.secret.as_bytes());
            accepted |= matches & (loopback || key.lan);
        }
        accepted
    }
}

pub fn load(path: &Path) -> Result<CallerKeys, String> {
    let file = fs::File::open(path).map_err(|e| format!("caller keys file: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("caller keys file: {e}"))?;
    if !metadata.is_file() {
        return Err("caller keys path must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("caller keys file must have mode 0600".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("caller keys file: {e}"))?;
    if bytes.len() > 256 * 1024 {
        return Err("caller keys file exceeds 256 KiB".into());
    }
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Result<CallerKeys, String> {
    let value = crate::state_json::parse(bytes).map_err(|_| "caller keys file is invalid JSON")?;
    let Value::Array(entries) = value else {
        return Err("caller keys file must contain an array".into());
    };
    if entries.len() > 256 {
        return Err("caller keys file exceeds 256 keys".into());
    }
    let mut keys: Vec<CallerKey> = Vec::with_capacity(entries.len());
    let mut ids = std::collections::BTreeSet::new();
    for entry in entries {
        let fields = entry.as_object().ok_or("caller key must be an object")?;
        if fields
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "name" | "lan" | "secret"))
        {
            return Err("caller key contains an unknown field".into());
        }
        for name in ["id", "name"] {
            if let Some(value) = fields.get(name) {
                let text = value
                    .as_str()
                    .ok_or("caller key metadata must be strings")?;
                if text.len() > 512 || text.chars().any(char::is_control) {
                    return Err("caller key metadata is invalid".into());
                }
                if name == "id" && (text.is_empty() || !ids.insert(text.to_string())) {
                    return Err("caller key identifiers must be nonempty and unique".into());
                }
            }
        }
        let secret = entry
            .get("secret")
            .as_str()
            .ok_or("caller key secret must be a string")?;
        if secret.is_empty() || secret.len() > 16384 || secret.bytes().any(|b| b.is_ascii_control())
        {
            return Err("caller key secret is empty or invalid".into());
        }
        if keys
            .iter()
            .any(|key| http::constant_eq(key.secret.as_bytes(), secret.as_bytes()))
        {
            return Err("caller key secrets must be unique".into());
        }
        let lan = match entry.get("lan") {
            Value::Bool(value) => *value,
            Value::Null => false,
            _ => return Err("caller key lan must be a boolean".into()),
        };
        keys.push(CallerKey {
            secret: secret.to_string(),
            lan,
        });
    }
    Ok(CallerKeys { keys })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_keys_preserve_peer_scope_and_empty_set_denies() {
        let keys = parse(br#"[{"id":"local","name":"Local","secret":"fixture-local","lan":false},{"id":"lan","name":"LAN","secret":"fixture-lan","lan":true}]"#).unwrap();
        assert!(keys.permits("fixture-local", true));
        assert!(!keys.permits("fixture-local", false));
        assert!(keys.permits("fixture-lan", true));
        assert!(keys.permits("fixture-lan", false));
        assert!(!keys.permits("fixture-laz", true));
        assert!(!keys.permits("", true));
        assert!(keys.allows_lan());
        let empty = parse(b"[]").unwrap();
        assert!(!empty.permits("fixture-local", true));
        assert!(!empty.allows_lan());
    }

    #[test]
    fn invalid_or_ambiguous_key_files_fail_without_printing_secrets() {
        for input in [
            r#"{}"#,
            r#"[{"secret":"fixture-secret","lan":"true"}]"#,
            r#"[{"secret":"fixture-secret","unknown":true}]"#,
            r#"[{"secret":"fixture-secret"},{"secret":"fixture-secret"}]"#,
            r#"[{"secret":"fixture-secret","secret":"other"}]"#,
            r#"[{"secret":"fixture\nsecret"}]"#,
        ] {
            let error = match parse(input.as_bytes()) {
                Ok(_) => panic!("invalid key input was accepted"),
                Err(error) => error,
            };
            assert!(!error.contains("fixture-secret"));
        }
    }
}
