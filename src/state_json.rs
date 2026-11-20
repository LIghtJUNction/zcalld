//! Owned control-plane values, using zconv's bounded validator and decoder.
//! This module is not used on the gateway request path.
#[cfg(windows)]
#[path = "privacy_windows.rs"]
pub(crate) mod privacy_windows;

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use zconv::json::{self, Kind, Limits, Token};

const MAX_INPUT: usize = 8 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
static NULL: Value = Value::Null;

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::String(v.into())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<usize> for Value {
    fn from(v: usize) -> Self {
        Self::Number(v.to_string())
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::Number(v.to_string())
    }
}

pub fn object<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

impl Value {
    pub fn get(&self, key: &str) -> &Value {
        self.as_object().and_then(|v| v.get(key)).unwrap_or(&NULL)
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        if let Self::Number(v) = self {
            v.parse().ok()
        } else {
            None
        }
    }
    pub fn as_array(&self) -> &[Value] {
        if let Self::Array(v) = self { v } else { &[] }
    }
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut BTreeMap<String, Value>> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn set(&mut self, key: &str, value: Value) {
        if !matches!(self, Self::Object(_)) {
            *self = Self::Object(BTreeMap::new());
        }
        self.as_object_mut().unwrap().insert(key.into(), value);
    }
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.as_object_mut().and_then(|v| v.remove(key))
    }
    pub fn stringify(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(v) => v.to_string(),
            Self::Number(v) => v.clone(),
            Self::String(v) => crate::http::quote(v),
            Self::Array(v) => format!(
                "[{}]",
                v.iter().map(Self::stringify).collect::<Vec<_>>().join(",")
            ),
            Self::Object(v) => format!(
                "{{{}}}",
                v.iter()
                    .map(|(k, v)| format!("{}:{}", crate::http::quote(k), v.stringify()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}

fn owned(value: json::Value<'_, '_>) -> Value {
    match value.kind() {
        Kind::Null => Value::Null,
        Kind::Bool => Value::Bool(value.as_bool().unwrap()),
        Kind::Number => Value::Number(String::from_utf8_lossy(value.raw()).into()),
        Kind::String => {
            Value::String(String::from_utf8(value.as_string().unwrap().bytes().collect()).unwrap())
        }
        Kind::Array => Value::Array(value.array().map(owned).collect()),
        Kind::Object => Value::Object(
            value
                .object()
                .map(|(k, v)| (String::from_utf8(k.bytes().collect()).unwrap(), owned(v)))
                .collect(),
        ),
    }
}

pub fn parse(input: &[u8]) -> Result<Value, String> {
    parse_with_limit(input, MAX_INPUT)
}

pub fn parse_with_limit(input: &[u8], max_input: usize) -> Result<Value, String> {
    let mut count = 256usize;
    loop {
        let mut tokens = vec![Token::EMPTY; count];
        match json::parse(
            input,
            &mut tokens,
            Limits {
                max_input,
                ..Limits::default()
            },
        ) {
            Ok(doc) => return Ok(owned(doc.root())),
            Err(zconv::Error::WorkspaceTooSmall { needed }) if count < 262_144 => {
                count = (count * 2).max(needed).min(262_144)
            }
            Err(error) => return Err(format!("invalid configuration JSON: {error:?}")),
        }
    }
}

pub fn read(path: &Path) -> Result<Value, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.len() > MAX_INPUT as u64 {
        return Err("configuration exceeds 8 MiB".into());
    }
    parse(&fs::read(path).map_err(|e| e.to_string())?)
}

/// Writes a private file in the same directory, fsyncs it, then atomically replaces.
/// Symlink destinations are rejected: the caller must explicitly resolve an allowed target.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_INPUT {
        return Err("configuration exceeds 8 MiB".into());
    }
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("refusing to replace a configuration symlink".into());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(
        ".zcalld-{}-{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
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
            options.access_mode(0x40000000 | 0x00040000); // GENERIC_WRITE | WRITE_DAC
        }
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        privacy_windows::protect(&file)?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Removes JSONC syntax only in the validator's copy; all byte offsets remain stable.
fn jsonc_view(text: &str) -> Result<Vec<u8>, String> {
    let mut bytes = text.as_bytes().to_vec();
    let mut i = 0;
    let mut string = false;
    let mut escape = false;
    while i < bytes.len() {
        if string {
            if escape {
                escape = false;
            } else if bytes[i] == b'\\' {
                escape = true;
            } else if bytes[i] == b'"' {
                string = false;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'"' {
            string = true;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                bytes[i] = b' ';
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            bytes[i] = b' ';
            bytes[i + 1] = b' ';
            i += 2;
            let mut ended = false;
            while i < bytes.len() {
                if bytes[i..].starts_with(b"*/") {
                    bytes[i] = b' ';
                    bytes[i + 1] = b' ';
                    i += 2;
                    ended = true;
                    break;
                }
                if bytes[i] != b'\n' && bytes[i] != b'\r' {
                    bytes[i] = b' ';
                }
                i += 1;
            }
            if !ended {
                return Err("unterminated JSON comment".into());
            }
            continue;
        }
        i += 1;
    }
    let mut string = false;
    let mut escape = false;
    for i in 0..bytes.len() {
        let c = bytes[i];
        if string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                string = false;
            }
            continue;
        }
        if c == b'"' {
            string = true;
            continue;
        }
        if c == b',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(bytes.get(j), Some(b'}' | b']')) {
                bytes[i] = b' ';
            }
        }
    }
    Ok(bytes)
}

pub fn parse_jsonc(text: &str) -> Result<Value, String> {
    parse(&jsonc_view(text)?)
}

/// Replaces our member of a JSONC array while preserving every other raw member.
pub fn upsert_array(
    text: &str,
    path: &[&str],
    key: &str,
    value: &str,
    replacement: &Value,
) -> Result<String, String> {
    let view = jsonc_view(text)?;
    let mut capacity = 256;
    loop {
        let mut tape = vec![Token::EMPTY; capacity];
        let doc = match json::parse(
            &view,
            &mut tape,
            Limits {
                max_input: MAX_INPUT,
                ..Limits::default()
            },
        ) {
            Ok(d) => d,
            Err(zconv::Error::WorkspaceTooSmall { needed }) if capacity < 262_144 => {
                capacity = (capacity * 2).max(needed).min(262_144);
                continue;
            }
            Err(e) => return Err(format!("invalid JSONC array: {e:?}")),
        };
        let mut array = doc.root();
        for part in path {
            match array.get(part) {
                Some(v) => array = v,
                None => return patch_json(text, path, &Value::Array(vec![replacement.clone()])),
            }
        }
        if array.kind() != Kind::Array {
            return Err("configuration array has another type".into());
        }
        for item in array.array() {
            if item
                .get(key)
                .and_then(|v| v.as_string())
                .is_some_and(|s| s.eq_str(value))
            {
                let start = item.raw().as_ptr() as usize - view.as_ptr() as usize;
                return Ok(format!(
                    "{}{}{}",
                    &text[..start],
                    replacement.stringify(),
                    &text[start + item.raw().len()..]
                ));
            }
        }
        let last = array.array().last();
        let at = last
            .map(|v| v.raw().as_ptr() as usize - view.as_ptr() as usize + v.raw().len())
            .unwrap_or(
                array.raw().as_ptr() as usize - view.as_ptr() as usize + array.raw().len() - 1,
            );
        return Ok(format!(
            "{}{}\n{}\n{}",
            &text[..at],
            if last.is_some() { "," } else { "" },
            replacement.stringify(),
            &text[at..]
        ));
    }
}

/// Changes one JSON/JSONC path without reformatting surrounding keys or comments.
pub fn patch_json(text: &str, path: &[&str], replacement: &Value) -> Result<String, String> {
    if path.is_empty() {
        return Ok(replacement.stringify() + "\n");
    }
    let view = jsonc_view(text)?;
    let mut capacity = 256;
    loop {
        let mut tape = vec![Token::EMPTY; capacity];
        let doc = match json::parse(
            &view,
            &mut tape,
            Limits {
                max_input: MAX_INPUT,
                ..Limits::default()
            },
        ) {
            Ok(d) => d,
            Err(zconv::Error::WorkspaceTooSmall { needed }) if capacity < 262_144 => {
                capacity = (capacity * 2).max(needed).min(262_144);
                continue;
            }
            Err(e) => return Err(format!("cannot edit invalid JSON configuration: {e:?}")),
        };
        let mut value = doc.root();
        for (depth, key) in path.iter().enumerate() {
            if value.kind() != Kind::Object {
                return Err(format!(
                    "configuration path {} is not an object",
                    path[..depth].join(".")
                ));
            }
            if let Some(child) = value.get(key) {
                if depth + 1 == path.len() {
                    let start = child.raw().as_ptr() as usize - view.as_ptr() as usize;
                    let end = start + child.raw().len();
                    return Ok(format!(
                        "{}{}{}",
                        &text[..start],
                        replacement.stringify(),
                        &text[end..]
                    ));
                }
                value = child;
            } else {
                let mut added = replacement.clone();
                for k in path[depth + 1..].iter().rev() {
                    added = object([(k, added)]);
                }
                let last = value.object().last().map(|(_, v)| v);
                let end = last
                    .map(|v| v.raw().as_ptr() as usize - view.as_ptr() as usize + v.raw().len())
                    .unwrap_or(
                        value.raw().as_ptr() as usize - view.as_ptr() as usize + value.raw().len()
                            - 1,
                    );
                let comma = if last.is_some() { "," } else { "" };
                let insertion = format!(
                    "{comma}\n  {}: {}\n",
                    crate::http::quote(key),
                    added.stringify()
                );
                return Ok(format!("{}{}{}", &text[..end], insertion, &text[end..]));
            }
        }
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_large_numbers_unicode_and_duplicate_rejection() {
        let input = br#"{"n":123456789012345678901234567890,"s":"\ud83d\ude80","x":[true,null]}"#;
        let value = parse(input).unwrap();
        assert_eq!(value.get("s").as_str(), Some("🚀"));
        assert_eq!(parse(value.stringify().as_bytes()).unwrap(), value);
        assert!(parse(br#"{"x":1,"x":2}"#).is_err());
    }
    #[test]
    fn patches_jsonc_without_losing_comments_or_unknown_fields() {
        let input = "{\n // user's comment\n \"unknown\": 12345678901234567890,\n \"env\": {\"kept\":true,},\n}\n";
        let result = patch_json(
            input,
            &["env", "BASE"],
            &Value::from("http://127.0.0.1:3425"),
        )
        .unwrap();
        assert!(result.contains("// user's comment"));
        assert!(result.contains("12345678901234567890"));
        let parsed = parse(&jsonc_view(&result).unwrap()).unwrap();
        assert_eq!(parsed.get("env").get("kept").as_bool(), Some(true));
        assert_eq!(
            parsed.get("env").get("BASE").as_str(),
            Some("http://127.0.0.1:3425")
        );
    }
    #[test]
    fn edits_only_selected_subtree() {
        let original = "{\"a\": {\"model\":\"old\"}, \"model\":\"leave\"}";
        let changed = patch_json(original, &["a", "model"], &Value::from("new")).unwrap();
        assert_eq!(changed, "{\"a\": {\"model\":\"new\"}, \"model\":\"leave\"}");
    }
}
