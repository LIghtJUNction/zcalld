//! Owned control-plane values, using zconv's bounded validator and decoder.
//! This module is not used on the gateway request path.
use std::collections::BTreeMap;
use zconv::json::{self, Kind, Limits, Token};

const MAX_INPUT: usize = 32 * 1024 * 1024;
static NULL: Value = Value::Null;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Default for Value {
    fn default() -> Self {
        Self::Null
    }
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
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.as_object_mut().and_then(|v| v.get_mut(key))
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
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        if let Self::Array(v) = self {
            Some(v)
        } else {
            None
        }
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
            Self::String(v) => crate::quote(v),
            Self::Array(v) => format!(
                "[{}]",
                v.iter().map(Self::stringify).collect::<Vec<_>>().join(",")
            ),
            Self::Object(v) => format!(
                "{{{}}}",
                v.iter()
                    .map(|(k, v)| format!("{}:{}", crate::quote(k), v.stringify()))
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
    let mut count = 256usize;
    loop {
        let mut tokens = vec![Token::EMPTY; count];
        match json::parse(
            input,
            &mut tokens,
            Limits {
                max_input: MAX_INPUT,
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
