//! Only configurations containing advanced rules use the optional Go worker.
//! The native gateway and simple groups do not start a helper process.
use super::{digest, json};
use json::{Value, object};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use zcalld_compat_client::{Host, json as wire};
use zplugin::{Error, Result};

static START: Mutex<()> = Mutex::new(());
static VALIDATED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

pub fn required(rule: &Value) -> bool {
    ["time", "where", "folders", "exceptFolders"]
        .iter()
        .any(|key| match rule.get(key) {
            Value::Null => false,
            Value::Array(values) => !values.is_empty(),
            _ => true,
        })
}

fn to_wire(value: &Value) -> wire::Value {
    match value {
        Value::Null => wire::Value::Null,
        Value::Bool(v) => wire::Value::Bool(*v),
        Value::Number(v) => wire::Value::Number(v.clone()),
        Value::String(v) => wire::Value::String(v.clone()),
        Value::Array(v) => wire::Value::Array(v.iter().map(to_wire).collect()),
        Value::Object(v) => wire::Value::Object(
            v.iter()
                .map(|(key, value)| (key.clone(), to_wire(value)))
                .collect(),
        ),
    }
}

pub struct Evaluator {
    program: PathBuf,
    key: String,
    config: wire::Value,
    groups: BTreeSet<String>,
    host: Option<Host>,
}

impl Evaluator {
    pub fn prepare(path: &Path, settings: &Value) -> Result<Option<Self>> {
        let selected: Vec<_> = settings
            .get("groups")
            .as_array()
            .iter()
            .filter(|group| {
                group
                    .get("original")
                    .get("rules")
                    .as_array()
                    .iter()
                    .any(required)
            })
            .collect();
        if selected.is_empty() {
            return Ok(None);
        }
        let program = settings
            .get("rules_worker")
            .as_str()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("ZCALLD_PLUGINCTL").map(PathBuf::from));
        let program = if let Some(path) = program {
            zcalld_compat_client::helpers::resolve_legacy(&path)
        } else {
            zcalld_compat_client::helpers::find(zcalld_compat_client::helpers::Helper::Pluginctl)
                .ok_or(Error::InvalidArgument)?
        };
        if !zcalld_compat_client::helpers::executable(&program) {
            return Err(Error::InvalidArgument);
        }
        let config = object([(
            "groups",
            Value::Array(
                selected
                    .iter()
                    .map(|group| {
                        object([
                            ("name", group.get("name").clone()),
                            ("rules", group.get("original").get("rules").clone()),
                        ])
                    })
                    .collect(),
            ),
        )]);
        let meta = std::fs::metadata(&program).map_err(|_| Error::InvalidArgument)?;
        let key = format!(
            "routing:{}:{}:{:?}:{}:{}",
            path.display(),
            program.display(),
            meta.modified().ok(),
            meta.len(),
            digest::sha256(config.stringify().as_bytes())
        );
        let groups = selected
            .iter()
            .filter_map(|group| group.get("name").as_str())
            .map(str::to_owned)
            .collect();
        let mut evaluator = Self {
            program,
            key,
            config: to_wire(&config),
            groups,
            host: None,
        };
        // Configuration is checked once in a short-lived worker. It is not kept
        // resident until a request actually selects an advanced-rule group.
        let mut validated = VALIDATED
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| Error::PluginStatus(8))?;
        if !validated.contains(&evaluator.key) {
            let worker = evaluator.start("validate")?;
            drop(worker);
            if validated.len() >= 1024 {
                validated.clear();
            }
            validated.insert(evaluator.key.clone());
        }
        Ok(Some(evaluator))
    }

    fn start(&mut self, purpose: &str) -> Result<Host> {
        let _guard = START.lock().map_err(|_| Error::PluginStatus(8))?;
        let key = format!("{}:{purpose}", self.key);
        let (host, fresh) = Host::shared(&key, &self.program, &["routing-worker".into()], None)
            .map_err(|_| Error::PluginStatus(8))?;
        if fresh {
            let result = host
                .rpc(wire::object([
                    ("op", "configure".into()),
                    ("config", self.config.clone()),
                ]))
                .map_err(|_| Error::InvalidArgument)?;
            if result.get("version").as_u64() != Some(1) {
                return Err(Error::InvalidArgument);
            }
        }
        Ok(host)
    }

    pub fn evaluate(
        &mut self,
        group: &str,
        request: &Value,
        context: &Value,
        tokens: u64,
        message: &str,
    ) -> Result<Option<Vec<bool>>> {
        if !self.groups.contains(group) {
            return Ok(None);
        }
        let host = if let Some(host) = &self.host {
            host.clone()
        } else {
            let host = self.start("runtime")?;
            self.host = Some(host.clone());
            host
        };
        let at = context.get("at").as_u64().unwrap_or_else(super::now);
        let result = host
            .rpc(wire::object([
                ("op", "evaluate".into()),
                ("group", group.into()),
                ("body", to_wire(request)),
                ("message", message.into()),
                (
                    "agent",
                    context
                        .get("routing_agent")
                        .as_str()
                        .or_else(|| context.get("agent").as_str())
                        .unwrap_or("")
                        .into(),
                ),
                ("tokens", tokens.into()),
                ("at", at.into()),
                (
                    "subagent",
                    context.get("subagent").as_bool().unwrap_or(false).into(),
                ),
                (
                    "compact",
                    context.get("compact").as_bool().unwrap_or(false).into(),
                ),
                // The raw cwd hint is intentionally not forwarded.
                (
                    "workspace",
                    context
                        .get("trusted_workspace")
                        .as_str()
                        .unwrap_or("")
                        .into(),
                ),
                ("peer", to_wire(context.get("peer"))),
            ]))
            .map_err(|_| Error::PluginStatus(8))?;
        let wire::Value::Array(values) = result.get("matches") else {
            return Err(Error::PluginStatus(8));
        };
        let matches: Option<Vec<bool>> = values.iter().map(wire::Value::as_bool).collect();
        matches.map(Some).ok_or(Error::PluginStatus(8))
    }
}
