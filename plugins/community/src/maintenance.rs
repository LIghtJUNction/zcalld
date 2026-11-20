//! Optional one-shot maintenance bridge. Paths and account selectors come from
//! host configuration; the helper revalidates policy, identity and its ledger.
use super::{Community, Context, Stream};
use std::{
    cell::RefCell,
    collections::HashSet,
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use zcalld_compat_client::{
    Call, Host,
    json::{self, Value, object},
};
use zcalld_plugin_api::{RecoveryPlugin, recovery, status};
use zplugin::{Error, OwnedResource, ResourceHandler, Result};

const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REGISTRY_BYTES: u64 = 8 * 1024 * 1024;
const FRESH_QUOTA: &str = "zcalld_fresh_quota_account";

#[derive(Clone)]
struct Binding {
    root: PathBuf,
    registry: PathBuf,
    helper: Option<PathBuf>,
}
impl Binding {
    fn from_config(config: &Value) -> Option<Self> {
        let registry = PathBuf::from(config.get("registry").as_str()?);
        if !registry.is_absolute() {
            return None;
        }
        let root = registry.parent()?.to_path_buf();
        let helper = config
            .get("pluginctl")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Some(Self {
            root,
            registry,
            helper,
        })
    }
    fn enabled(&self, account: &str) -> bool {
        if !account
            .strip_prefix("builtin:codex#")
            .is_some_and(|suffix| {
                suffix.len() == 20
                    && suffix
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
            || !private_directory(&self.root)
        {
            return false;
        }
        let directory = self.root.join("maintenance");
        if !private_directory(&directory) {
            return false;
        }
        let Some(state) = local_json(&directory.join("state.json"), MAX_STATE_BYTES) else {
            return false;
        };
        if state.get("version").as_u64() != Some(1)
            || state.get("policy").get("enabled").as_bool() != Some(true)
            || state
                .get("policy")
                .get("accounts")
                .get(account)
                .get("weekly_reset")
                .as_bool()
                != Some(true)
        {
            return false;
        }
        let Some(registry) = local_json(&self.registry, MAX_REGISTRY_BYTES) else {
            return false;
        };
        let entry = registry.get("accounts").get(account);
        // Registry AccountMeta stores enabled/label; provider identity is the
        // explicit opaque key prefix, revalidated by the helper before acting.
        if entry.get("enabled").as_bool() != Some(true) {
            return false;
        }
        let mut builtins = registry.get("plugins").as_array().iter().filter(|p| {
            p.get("name").as_str() == Some("builtin-codex")
                || p.get("source").as_str() == Some("builtin-codex")
        });
        let Some(plugin) = builtins.next() else {
            return false;
        };
        builtins.next().is_none()
            && plugin.get("name").as_str() == Some("builtin-codex")
            && plugin.get("source").as_str() == Some("builtin-codex")
            && plugin.get("kind").as_str() == Some("community")
            && plugin.get("enabled").as_bool() == Some(true)
    }
    fn start(&self, account: &str, trigger: &str, deadline: Instant) -> Result<Option<Pending>> {
        if !self.enabled(account) {
            return Ok(None);
        }
        let helper = self
            .helper
            .as_ref()
            .filter(|p| p.is_absolute() && p.is_file())
            .ok_or(Error::PluginStatus(status::PROVIDER_REQUEST_FAILED))?;
        let phase_deadline = deadline.min(Instant::now() + Duration::from_secs(60));
        remaining(phase_deadline)?;
        let host = Host::isolated_until(
            helper,
            &[
                "--root".into(),
                self.root.to_string_lossy().into_owned(),
                "maintenance".into(),
                "worker".into(),
            ],
            Some(&self.root),
            phase_deadline,
        )
        .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?;
        let budget = remaining(phase_deadline)?;
        let call = host
            .start(object([
                ("method", "weekly".into()),
                (
                    "params",
                    object([
                        ("account", account.into()),
                        ("trigger", trigger.into()),
                        ("timeout_ms", budget.into()),
                    ]),
                ),
            ]))
            .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?;
        Ok(Some(Pending {
            call: Some(call),
            deadline: phase_deadline,
        }))
    }
}

// This bounded read is only an optimization that avoids launching a disabled
// worker. The helper always repeats platform ownership/ACL checks under its
// transaction lock before a side effect. No local hint authorizes a reset.
fn local_json(path: &Path, limit: u64) -> Option<Value> {
    let before = fs::symlink_metadata(path).ok()?;
    if !before.is_file() || before.len() > limit || !private_metadata(&before) {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let held = file.metadata().ok()?;
    if !same_file(&before, &held) {
        return None;
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file).take(limit + 1).read_to_end(&mut bytes).ok()?;
    let after = fs::symlink_metadata(path).ok()?;
    if bytes.len() as u64 > limit
        || !after.is_file()
        || !private_metadata(&after)
        || !same_file(&before, &after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return None;
    }
    json::parse(&bytes).ok()
}
fn private_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && private_metadata(&meta))
}
fn private_metadata(meta: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        meta.uid() == unsafe { geteuid() } && meta.mode() & 0o077 == 0
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 == 0
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = meta;
        false
    }
}
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.is_file() == b.is_file() && a.len() == b.len() && a.created().ok() == b.created().ok()
    }
}
fn remaining(deadline: Instant) -> Result<u64> {
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    if millis == 0 {
        Err(Error::PluginStatus(status::PROVIDER_TIMED_OUT))
    } else {
        Ok(millis.min(3_600_000) as u64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Reset,
    NotSent,
    Unknown,
}
struct Pending {
    call: Option<Call>,
    deadline: Instant,
}
impl Pending {
    fn poll(&mut self) -> Result<Option<Outcome>> {
        if Instant::now() >= self.deadline {
            self.call.take(); // abort, EOF and the existing bounded credential drain
            return Ok(Some(Outcome::Unknown));
        }
        let Some(call) = self.call.as_mut() else {
            return Ok(Some(Outcome::Unknown));
        };
        let event = match call.poll() {
            Ok(Some(event)) => event,
            Ok(None) => return Ok(None),
            Err(_) => {
                self.call.take();
                return Ok(Some(Outcome::Unknown));
            }
        };
        if !matches!(event.get("error"), Value::Null) {
            self.call.take();
            return Ok(Some(Outcome::Unknown));
        }
        if !event.as_object().is_some_and(|v| v.contains_key("result")) {
            return Ok(None);
        }
        if Instant::now() >= self.deadline {
            self.call.take();
            return Ok(Some(Outcome::Unknown));
        }
        let result = event.get("result");
        if result.stringify().len() > recovery::MAX_RESULT_BYTES {
            self.call.take();
            return Ok(Some(Outcome::Unknown));
        }
        let outcome = match (result.get("status").as_str(), result.get("code").as_str()) {
            (Some("completed"), Some("reset")) => Outcome::Reset,
            (Some("not_sent"), Some(code)) if code.len() <= 64 => Outcome::NotSent,
            _ => Outcome::Unknown,
        };
        call.finish();
        self.call.take();
        Ok(Some(outcome))
    }
}

struct Planned {
    binding: Binding,
    account: String,
    started: bool,
    pending: Option<Pending>,
}
impl Planned {
    fn poll(&mut self, deadline: Instant) -> Result<Option<Outcome>> {
        if !self.started {
            self.started = true;
            self.pending = self
                .binding
                .start(&self.account, "before_credits", deadline)?;
        }
        match self.pending.as_mut() {
            Some(pending) => pending.poll(),
            None => Ok(Some(Outcome::NotSent)),
        }
    }
}

/// Do not queue the normal generation RPC until initialization and the optional
/// before-credits operation have both finished within the original deadline.
pub(super) struct Fetch {
    host: Host,
    initialization: Option<Call>,
    plan: Option<Planned>,
    params: Option<Value>,
    stream: Option<Stream>,
    deadline: Instant,
}
impl Fetch {
    pub(super) fn new(
        community: &mut Community,
        mut params: Value,
        explicit_account: Option<&str>,
        deadline: Instant,
    ) -> Result<Self> {
        // Never accept an invalidation hint from the incoming provider envelope.
        // Only this request's successful recovery resource can create one.
        params.remove(FRESH_QUOTA);
        if let Some(account) = community.ctx.borrow_mut().after_reset.take()
            && params.get("provider").as_str() == Some("builtin:codex")
            && explicit_account == Some(account.as_str())
        {
            params.set(FRESH_QUOTA, account.into());
        }
        let plan = if params.get("provider").as_str() == Some("builtin:codex") {
            explicit_account.and_then(|account| {
                let binding = Binding::from_config(&community.config)?;
                binding.enabled(account).then(|| Planned {
                    binding,
                    account: account.into(),
                    started: false,
                    pending: None,
                })
            })
        } else {
            None
        };
        let (host, initialization) = community.host_start()?;
        // A reused shared host may still be initializing or have failed init.
        // Verify the same ready promise before any maintenance side effect.
        let initialization = if plan.is_some() && initialization.is_none() {
            Some(
                host.start(object([("method", "nativeReady".into())]))
                    .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?,
            )
        } else {
            initialization
        };
        Ok(Self {
            host,
            initialization,
            plan,
            params: Some(params),
            stream: None,
            deadline,
        })
    }
    fn advance(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        remaining(self.deadline)?;
        if let Some(call) = self.initialization.as_mut() {
            let Some(event) = call
                .poll()
                .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?
            else {
                return Err(Error::PluginStatus(3));
            };
            if !matches!(event.get("error"), Value::Null) {
                return Err(Error::PluginStatus(status::PROVIDER_REQUEST_FAILED));
            }
            if !event.as_object().is_some_and(|v| v.contains_key("result")) {
                return Err(Error::PluginStatus(3));
            }
            call.finish();
            self.initialization = None;
        }
        if let Some(plan) = self.plan.as_mut() {
            match plan.poll(self.deadline)? {
                None => return Err(Error::PluginStatus(3)),
                Some(Outcome::Unknown) => {
                    return Err(Error::PluginStatus(status::PROVIDER_REQUEST_FAILED));
                }
                Some(Outcome::Reset) => {
                    let account = plan.account.clone();
                    self.params
                        .as_mut()
                        .ok_or(Error::InvalidArgument)?
                        .set(FRESH_QUOTA, account.into());
                    self.plan = None;
                }
                Some(Outcome::NotSent) => self.plan = None,
            }
        }
        let budget = remaining(self.deadline)?;
        let mut params = self.params.take().ok_or(Error::InvalidArgument)?;
        params.set("timeout_ms", budget.into());
        let call = self
            .host
            .start(object([("method", "fetch".into()), ("params", params)]))
            .map_err(|_| Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED))?;
        self.stream = Some(Stream {
            call,
            initialization: None,
            head: None,
            bytes: Vec::new(),
            offset: 0,
            ended: false,
            deadline: self.deadline,
        });
        Ok(())
    }
}
impl ResourceHandler for Fetch {
    const ID: &'static str = "dev.zcalld.provider-stream";
    const VERSION: u32 = 1;
    fn call(&mut self, operation: u32, input: &[u8], output: &mut [u8]) -> Result<usize> {
        if operation == 3 {
            self.plan = None;
            self.params = None;
            self.initialization = None;
            return self
                .stream
                .as_mut()
                .map_or(Ok(0), |s| s.call(operation, input, output));
        }
        if !matches!(operation, 1 | 2) {
            return Err(Error::Unsupported);
        }
        self.advance()?;
        self.stream
            .as_mut()
            .ok_or(Error::InvalidArgument)?
            .call(operation, input, output)
    }
}

#[derive(Clone, Debug)]
struct Candidate {
    index: usize,
    account: String,
}
fn candidates(metadata: &[u8]) -> Result<(u64, Vec<Candidate>)> {
    if metadata.len() > recovery::MAX_METADATA_BYTES {
        return Err(Error::InvalidArgument);
    }
    let value = json::parse(metadata).map_err(|_| Error::InvalidArgument)?;
    if value.get("phase").as_str() != Some("routes_exhausted") {
        return Err(Error::InvalidArgument);
    }
    let timeout = value
        .get("timeout_ms")
        .as_u64()
        .filter(|v| (1..=3_600_000).contains(v))
        .ok_or(Error::InvalidArgument)?
        .min(60_000);
    let attempts = match value.get("attempts") {
        Value::Array(a) => a,
        _ => return Err(Error::InvalidArgument),
    };
    if attempts.len() > recovery::MAX_ATTEMPTS {
        return Err(Error::InvalidArgument);
    }
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    for (index, attempt) in attempts.iter().enumerate().rev() {
        if !matches!(attempt.get("status").as_u64(), Some(429 | 503)) {
            return Err(Error::InvalidArgument);
        }
        if attempt.get("provider").as_str() != Some("builtin:codex") {
            continue;
        }
        let Some(account) = attempt.get("account").as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if account.len() > 512 || account.chars().any(char::is_control) {
            return Err(Error::InvalidArgument);
        }
        if seen.insert(account.to_owned()) {
            selected.push(Candidate {
                index,
                account: account.into(),
            });
        }
    }
    Ok((timeout, selected))
}
struct Recovery {
    binding: Option<Binding>,
    context: Option<Rc<RefCell<Context>>>,
    candidates: Vec<Candidate>,
    next: usize,
    pending: Option<(usize, Pending)>,
    result: Option<Vec<u8>>,
    deadline: Instant,
}
impl Recovery {
    fn new(config: &Value, metadata: &[u8]) -> Result<Self> {
        let (timeout, candidates) = candidates(metadata)?;
        Ok(Self {
            binding: Binding::from_config(config),
            context: None,
            candidates,
            next: 0,
            pending: None,
            result: None,
            deadline: Instant::now() + Duration::from_millis(timeout),
        })
    }
    fn complete(&mut self, candidate: Option<usize>) {
        let mut result = object([("recovered", candidate.is_some().into())]);
        if let Some(index) = candidate {
            result.set("candidate", index.into());
            if let Some(context) = &self.context
                && let Some(chosen) = self.candidates.iter().find(|c| c.index == index)
            {
                context.borrow_mut().after_reset = Some(chosen.account.clone());
            }
        }
        self.result = Some(result.stringify().into_bytes());
    }
    fn poll(&mut self) -> Result<()> {
        if self.result.is_some() {
            return Ok(());
        }
        if remaining(self.deadline).is_err() {
            self.pending = None;
            self.complete(None);
            return Ok(());
        }
        if let Some((index, pending)) = self.pending.as_mut() {
            match pending.poll()? {
                None => return Err(Error::PluginStatus(3)),
                Some(Outcome::Reset) => {
                    let index = *index;
                    self.pending = None;
                    self.complete(Some(index));
                    return Ok(());
                }
                Some(Outcome::Unknown) => {
                    self.pending = None;
                    self.complete(None);
                    return Ok(());
                }
                Some(Outcome::NotSent) => self.pending = None,
            }
        }
        while self.next < self.candidates.len() {
            let candidate = &self.candidates[self.next];
            self.next += 1;
            let Some(binding) = &self.binding else { break };
            match binding.start(&candidate.account, "routes_exhausted", self.deadline) {
                Ok(Some(pending)) => {
                    self.pending = Some((candidate.index, pending));
                    return Err(Error::PluginStatus(3));
                }
                Ok(None) => {}
                Err(_) => {
                    self.complete(None);
                    return Ok(());
                }
            }
        }
        self.complete(None);
        Ok(())
    }
}
impl ResourceHandler for Recovery {
    const ID: &'static str = recovery::RESOURCE_ID;
    const VERSION: u32 = recovery::RESOURCE_VERSION;
    fn call(&mut self, operation: u32, input: &[u8], output: &mut [u8]) -> Result<usize> {
        if operation != recovery::POLL || !input.is_empty() {
            return Err(Error::Unsupported);
        }
        self.poll()?;
        let result = self.result.as_ref().ok_or(Error::PluginStatus(3))?;
        if output.len() < result.len() {
            return Err(Error::BufferTooSmall);
        }
        output[..result.len()].copy_from_slice(result);
        Ok(result.len())
    }
}
impl RecoveryPlugin for Community {
    fn configure(&mut self, config: &str) -> Result<()> {
        self.configure_host(config)
    }
    fn accepts(&mut self) -> Result<u32> {
        Ok(1)
    }
    fn begin(&mut self, metadata: &[u8]) -> Result<OwnedResource> {
        let mut planned = Recovery::new(&self.config, metadata)?;
        planned.context = Some(Rc::clone(&self.ctx));
        OwnedResource::new(planned)
    }
    fn resume(&mut self, _: u32, _: &str, _: &[u8], _: &mut [u8]) -> Result<u64> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(attempts: Value) -> Vec<u8> {
        object([
            ("phase", "routes_exhausted".into()),
            ("timeout_ms", 2000u64.into()),
            ("attempts", attempts),
        ])
        .stringify()
        .into_bytes()
    }
    fn attempt(account: &str, status: u64) -> Value {
        object([
            ("provider", "builtin:codex".into()),
            ("account", account.into()),
            ("route", "public-route".into()),
            ("model", "upstream-model".into()),
            ("status", status.into()),
        ])
    }
    #[test]
    fn recovery_uses_only_explicit_accounts_in_reverse_order_once() {
        let bytes = metadata(Value::Array(vec![
            attempt("builtin:codex#aaaaaaaaaaaaaaaaaaaa", 429),
            attempt("builtin:codex#bbbbbbbbbbbbbbbbbbbb", 503),
            attempt("", 429),
            attempt("builtin:codex#aaaaaaaaaaaaaaaaaaaa", 503),
        ]));
        let (_, selected) = candidates(&bytes).unwrap();
        assert_eq!(
            selected.iter().map(|v| v.index).collect::<Vec<_>>(),
            vec![3, 1]
        );
        assert_eq!(selected[0].account, "builtin:codex#aaaaaaaaaaaaaaaaaaaa");
    }
    #[test]
    fn recovery_rejects_non_rejection_and_unbounded_requests() {
        assert!(
            candidates(&metadata(Value::Array(vec![attempt(
                "builtin:codex#a",
                200
            )])))
            .is_err()
        );
        let repeated = vec![attempt("builtin:codex#a", 429); recovery::MAX_ATTEMPTS + 1];
        assert!(candidates(&metadata(Value::Array(repeated))).is_err());
        for body in [
            br#"{"phase":"before_credits","timeout_ms":1,"attempts":[]}"#.as_slice(),
            br#"{"phase":"routes_exhausted","timeout_ms":0,"attempts":[]}"#,
            br#"{"phase":"routes_exhausted","timeout_ms":1,"attempts":null}"#,
        ] {
            assert!(candidates(body).is_err());
        }
    }
    #[test]
    fn disabled_recovery_is_immediate_and_replays_small_buffer() {
        let bytes = metadata(Value::Array(vec![attempt(
            "builtin:codex#aaaaaaaaaaaaaaaaaaaa",
            429,
        )]));
        let mut recovery = Recovery::new(&Value::Null, &bytes).unwrap();
        assert!(matches!(
            recovery.call(1, &[], &mut []),
            Err(Error::BufferTooSmall)
        ));
        let mut out = [0u8; 256];
        let n = recovery.call(1, &[], &mut out).unwrap();
        assert_eq!(&out[..n], br#"{"recovered":false}"#);
        assert_eq!(recovery.call(1, &[], &mut out).unwrap(), n);
    }
}

#[cfg(all(test, unix))]
mod worker_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    const A: &str = "builtin:codex#aaaaaaaaaaaaaaaaaaaa";
    const B: &str = "builtin:codex#bbbbbbbbbbbbbbbbbbbb";
    struct Fixture {
        root: PathBuf,
        config: Value,
    }
    fn shell(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
    impl Fixture {
        fn new(script: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "zcalld-weekly-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let root = root.canonicalize().unwrap();
            let directory = root.join("maintenance");
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let helper = root.join("worker");
            let text = format!(
                "#!/bin/sh\nset -eu\nIFS= read -r line || exit 0\nprintf '%s\\n' \"$line\" >> {}\n{}\ncat >/dev/null\n",
                shell(&root.join("calls")),
                script
            );
            fs::write(&helper, text).unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
            let registry = root.join("registry.json");
            let config = object([
                ("registry", registry.to_string_lossy().into_owned().into()),
                ("pluginctl", helper.to_string_lossy().into_owned().into()),
            ]);
            let fixture = Self { root, config };
            fixture.write_state(true);
            fixture.write_registry(true);
            fixture
        }
        fn write(&self, name: &str, value: Value) {
            let path = self.root.join(name);
            fs::write(&path, value.stringify()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fn write_state(&self, enabled: bool) {
            self.write(
                "maintenance/state.json",
                object([
                    ("version", 1u64.into()),
                    (
                        "policy",
                        object([
                            ("enabled", enabled.into()),
                            (
                                "accounts",
                                object([
                                    (A, object([("weekly_reset", true.into())])),
                                    (B, object([("weekly_reset", true.into())])),
                                ]),
                            ),
                        ]),
                    ),
                ]),
            );
        }
        fn write_registry(&self, enabled: bool) {
            self.write(
                "registry.json",
                object([
                    (
                        "plugins",
                        Value::Array(vec![object([
                            ("name", "builtin-codex".into()),
                            ("source", "builtin-codex".into()),
                            ("kind", "community".into()),
                            ("enabled", true.into()),
                            (
                                "path",
                                self.root
                                    .join("adapter.mjs")
                                    .to_string_lossy()
                                    .into_owned()
                                    .into(),
                            ),
                        ])]),
                    ),
                    (
                        "accounts",
                        object([
                            (
                                A,
                                object([
                                    ("label", "Fixture account".into()),
                                    ("enabled", enabled.into()),
                                ]),
                            ),
                            (
                                B,
                                object([
                                    ("label", "Fixture account".into()),
                                    ("enabled", true.into()),
                                ]),
                            ),
                        ]),
                    ),
                ]),
            );
        }
        fn binding(&self) -> Binding {
            Binding::from_config(&self.config).unwrap()
        }
        fn calls(&self) -> Vec<Value> {
            fs::read_to_string(self.root.join("calls"))
                .unwrap_or_default()
                .lines()
                .map(|line| json::parse(line.as_bytes()).unwrap())
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn response(status: &str, code: &str) -> String {
        format!(
            "printf '%s\\n' '{{\"id\":1,\"result\":{{\"status\":\"{status}\",\"code\":\"{code}\"}}}}'"
        )
    }
    fn poll(pending: &mut Pending) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(outcome) = pending.poll().unwrap() {
                return outcome;
            }
            assert!(Instant::now() < deadline);
        }
    }
    #[test]
    fn default_disabled_invalid_or_unbound_policy_starts_no_worker() {
        let fixture = Fixture::new(&response("completed", "reset"));
        fixture.write_state(false);
        assert!(
            fixture
                .binding()
                .start(A, "before_credits", Instant::now() + Duration::from_secs(2))
                .unwrap()
                .is_none()
        );
        fixture.write_state(true);
        fixture.write_registry(false);
        assert!(
            fixture
                .binding()
                .start(A, "before_credits", Instant::now() + Duration::from_secs(2))
                .unwrap()
                .is_none()
        );
        fixture.write_registry(true);
        assert!(
            fixture
                .binding()
                .start(
                    "builtin:codex#cccccccccccccccccccc",
                    "before_credits",
                    Instant::now() + Duration::from_secs(2)
                )
                .unwrap()
                .is_none()
        );
        fs::set_permissions(
            fixture.root.join("maintenance/state.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(!fixture.binding().enabled(A));
        assert!(fixture.calls().is_empty());
    }
    #[test]
    fn one_shot_worker_uses_only_fixed_rpc_and_releases_stdin() {
        let fixture = Fixture::new(&response("completed", "reset"));
        let mut pending = fixture
            .binding()
            .start(A, "before_credits", Instant::now() + Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(poll(&mut pending), Outcome::Reset);
        let calls = fixture.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].get("method").as_str(), Some("weekly"));
        assert_eq!(calls[0].get("params").get("account").as_str(), Some(A));
        assert_eq!(
            calls[0].get("params").get("trigger").as_str(),
            Some("before_credits")
        );
        let budget = calls[0].get("params").get("timeout_ms").as_u64().unwrap();
        assert!((1..=2000).contains(&budget));
    }
    #[test]
    fn dropping_pending_worker_delivers_abort_and_eof_before_return() {
        let fixture = Fixture::new(
            "IFS= read -r cancelled || cancelled=EOF\nprintf '%s\\n' \"$cancelled\" > cancelled\ncat >/dev/null\nprintf drained > drained\nexit 0",
        );
        let pending = fixture
            .binding()
            .start(A, "before_credits", Instant::now() + Duration::from_secs(2))
            .unwrap()
            .unwrap();
        drop(pending);
        let cancelled = json::parse(&fs::read(fixture.root.join("cancelled")).unwrap()).unwrap();
        assert_eq!(cancelled.get("method").as_str(), Some("abort"));
        assert_eq!(fs::read(fixture.root.join("drained")).unwrap(), b"drained");
    }
    #[test]
    fn recovery_stops_on_unknown_and_replays_success_without_second_worker() {
        for status in ["unknown", "completed"] {
            let fixture = Fixture::new(&response(
                status,
                if status == "completed" {
                    "reset"
                } else {
                    "uncertain"
                },
            ));
            let metadata = object([
                ("phase", "routes_exhausted".into()),
                ("timeout_ms", 2000u64.into()),
                (
                    "attempts",
                    Value::Array(vec![
                        object([
                            ("provider", "builtin:codex".into()),
                            ("account", A.into()),
                            ("status", 429u64.into()),
                        ]),
                        object([
                            ("provider", "builtin:codex".into()),
                            ("account", B.into()),
                            ("status", 503u64.into()),
                        ]),
                    ]),
                ),
            ])
            .stringify();
            let mut recovery = Recovery::new(&fixture.config, metadata.as_bytes()).unwrap();
            let limit = Instant::now() + Duration::from_secs(3);
            loop {
                match recovery.call(1, &[], &mut []) {
                    Err(Error::PluginStatus(3)) => assert!(Instant::now() < limit),
                    Err(Error::BufferTooSmall) => break,
                    other => panic!("unexpected recovery: {other:?}"),
                }
            }
            let mut bytes = [0; 256];
            let n = recovery.call(1, &[], &mut bytes).unwrap();
            let result = json::parse(&bytes[..n]).unwrap();
            assert_eq!(
                result.get("recovered").as_bool(),
                Some(status == "completed")
            );
            if status == "completed" {
                assert_eq!(result.get("candidate").as_u64(), Some(1));
            }
            recovery.call(1, &[], &mut bytes).unwrap();
            assert_eq!(fixture.calls().len(), 1);
            assert_eq!(
                fixture.calls()[0].get("params").get("account").as_str(),
                Some(B)
            );
        }
    }
    fn mock_host(fixture: &Fixture) -> Community {
        let script = r#"#!/bin/sh
set -eu
IFS= read -r first || exit 0
printf '%s\n' "$first" >> @LOG@
printf '%s\n' '{"id":1,"result":{}}'
while IFS= read -r line; do
 printf '%s\n' "$line" >> @LOG@
 case "$line" in *'"method":"fetch"'*)
  printf '%s\n' '{"id":2,"event":"head","status":200,"headers":{"content-type":"application/json"}}'
  printf '%s\n' '{"id":2,"result":{}}'
 ;; esac
done
"#
        .replace("@LOG@", &shell(&fixture.root.join("native-calls")));
        let script_path = fixture.root.join("host.sh");
        fs::write(&script_path, script).unwrap();
        let pi = fixture.root.join("pi.js");
        fs::write(&pi, "// fixture").unwrap();
        let mut config = fixture.config.clone();
        config.set("bun", "/bin/sh".into());
        config.set("host", script_path.to_string_lossy().into_owned().into());
        config.set("pi", pi.to_string_lossy().into_owned().into());
        let mut community = Community::default();
        community.configure_host(&config.stringify()).unwrap();
        community
    }
    fn params() -> Value {
        object([
            ("provider", "builtin:codex".into()),
            ("account", A.into()),
            ("model", "fixture-model".into()),
            ("body", zcalld_compat_client::encode(b"{}").into()),
            ("timeout_ms", 4000u64.into()),
            ("window", 524288u64.into()),
        ])
    }
    fn native_calls(fixture: &Fixture) -> Vec<Value> {
        fs::read_to_string(fixture.root.join("native-calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| json::parse(line.as_bytes()).unwrap())
            .collect()
    }
    #[test]
    fn generation_waits_for_preflight_and_uses_only_remaining_deadline() {
        let fixture = Fixture::new(
            "while [ ! -f release ]; do sleep 0.01; done\nprintf '%s\\n' '{\"id\":1,\"result\":{\"status\":\"completed\",\"code\":\"reset\"}}'",
        );
        let mut community = mock_host(&fixture);
        let mut fetch = Fetch::new(
            &mut community,
            params(),
            Some(A),
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let mut head = [0u8; 16384];
        let start_limit = Instant::now() + Duration::from_secs(2);
        while fixture.calls().is_empty() {
            assert!(matches!(
                fetch.call(1, &[], &mut head),
                Err(Error::PluginStatus(3))
            ));
            assert!(Instant::now() < start_limit);
        }
        let calls = native_calls(&fixture);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].get("method").as_str(), Some("init"));
        std::thread::sleep(Duration::from_millis(25));
        fs::write(fixture.root.join("release"), b"go").unwrap();
        let limit = Instant::now() + Duration::from_secs(3);
        let n = loop {
            match fetch.call(1, &[], &mut head) {
                Ok(n) => break n,
                Err(Error::PluginStatus(3)) => assert!(Instant::now() < limit),
                other => panic!("unexpected fetch state {other:?}"),
            }
        };
        assert_eq!(
            json::parse(&head[..n]).unwrap().get("status").as_u64(),
            Some(200)
        );
        let calls = native_calls(&fixture);
        let fetches: Vec<_> = calls
            .iter()
            .filter(|c| c.get("method").as_str() == Some("fetch"))
            .collect();
        assert_eq!(fetches.len(), 1);
        assert!(fetches[0].get("params").get("timeout_ms").as_u64().unwrap() < 4000);
        assert_eq!(fixture.calls().len(), 1);
        drop(fetch);
        drop(community);
    }
    #[test]
    fn cancelled_preflight_never_queues_generation() {
        let fixture = Fixture::new(
            "IFS= read -r cancelled || cancelled=EOF\nprintf '%s\\n' \"$cancelled\" > cancelled\ncat >/dev/null\nprintf drained > drained\nexit 0",
        );
        let mut community = mock_host(&fixture);
        let mut fetch = Fetch::new(
            &mut community,
            params(),
            Some(A),
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let start_limit = Instant::now() + Duration::from_secs(2);
        while fixture.calls().is_empty() {
            assert!(matches!(
                fetch.call(1, &[], &mut [0u8; 1024]),
                Err(Error::PluginStatus(3))
            ));
            assert!(Instant::now() < start_limit);
        }
        drop(fetch);
        assert_eq!(fs::read(fixture.root.join("drained")).unwrap(), b"drained");
        assert!(
            native_calls(&fixture)
                .iter()
                .all(|c| c.get("method").as_str() != Some("fetch"))
        );
        drop(community);
    }
    #[test]
    fn an_implicit_generation_account_never_starts_weekly() {
        let fixture = Fixture::new(&response("completed", "reset"));
        let mut community = mock_host(&fixture);
        let mut fetch = Fetch::new(
            &mut community,
            params(),
            None,
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let limit = Instant::now() + Duration::from_secs(3);
        loop {
            match fetch.call(1, &[], &mut [0u8; 1024]) {
                Ok(_) => break,
                Err(Error::PluginStatus(3)) => assert!(Instant::now() < limit),
                other => panic!("unexpected fetch state {other:?}"),
            }
        }
        assert!(fixture.calls().is_empty());
        drop(fetch);
        drop(community);
    }
    #[test]
    fn shared_failed_init_never_starts_maintenance_or_generation() {
        let fixture = Fixture::new(&response("completed", "reset"));
        let mut community = mock_host(&fixture);
        let script = r#"#!/bin/sh
set -eu
IFS= read -r first || exit 0
printf '%s\n' "$first" >> @LOG@
IFS= read -r second || exit 0
printf '%s\n' "$second" >> @LOG@
printf '%s\n' '{"id":1,"error":{"message":"synthetic init failure"}}'
printf '%s\n' '{"id":2,"error":{"message":"synthetic init failure"}}'
cat >/dev/null
"#
        .replace("@LOG@", &shell(&fixture.root.join("native-calls")));
        fs::write(fixture.root.join("host.sh"), script).unwrap();
        let first = Fetch::new(
            &mut community,
            params(),
            Some(A),
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let mut second = Fetch::new(
            &mut community,
            params(),
            Some(A),
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let limit = Instant::now() + Duration::from_secs(3);
        loop {
            match second.call(1, &[], &mut [0u8; 1024]) {
                Err(Error::PluginStatus(status::PROVIDER_REQUEST_FAILED)) => break,
                Err(Error::PluginStatus(3)) => assert!(Instant::now() < limit),
                other => panic!("unexpected readiness state {other:?}"),
            }
        }
        let calls = native_calls(&fixture);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].get("method").as_str(), Some("nativeReady"));
        assert!(fixture.calls().is_empty());
        drop(first);
        drop(second);
        drop(community);
    }
    #[test]
    fn shared_ready_host_preflights_then_sends_generation_once() {
        let fixture = Fixture::new(&response("not_sent", "not_on_credits"));
        let mut community = mock_host(&fixture);
        let script = r#"#!/bin/sh
set -eu
IFS= read -r first || exit 0
printf '%s\n' "$first" >> @LOG@
printf '%s\n' '{"id":1,"result":{}}'
while IFS= read -r line; do
 printf '%s\n' "$line" >> @LOG@
 case "$line" in
 *'"method":"nativeReady"'*) printf '%s\n' '{"id":2,"result":null}' ;;
 *'"method":"fetch"'*)
  printf '%s\n' '{"id":3,"event":"head","status":200,"headers":{"content-type":"application/json"}}'
  printf '%s\n' '{"id":3,"result":{}}'
 ;; esac
done
"#
        .replace("@LOG@", &shell(&fixture.root.join("native-calls")));
        fs::write(fixture.root.join("host.sh"), script).unwrap();
        let first = Fetch::new(
            &mut community,
            params(),
            Some(A),
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let mut second = Fetch::new(
            &mut community,
            params(),
            Some(A),
            Instant::now() + Duration::from_secs(4),
        )
        .unwrap();
        let limit = Instant::now() + Duration::from_secs(3);
        loop {
            match second.call(1, &[], &mut [0u8; 1024]) {
                Ok(_) => break,
                Err(Error::PluginStatus(3)) => assert!(Instant::now() < limit),
                other => panic!("unexpected readiness state {other:?}"),
            }
        }
        let calls = native_calls(&fixture);
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.get("method").as_str() == Some("fetch"))
                .count(),
            1
        );
        assert_eq!(calls[1].get("method").as_str(), Some("nativeReady"));
        assert_eq!(fixture.calls().len(), 1);
        drop(first);
        drop(second);
        drop(community);
    }
    #[test]
    fn only_a_confirmed_recovery_attaches_one_account_scoped_fresh_intent() {
        for outcome in ["completed", "not_sent", "unknown"] {
            let fixture = Fixture::new(&response(
                outcome,
                if outcome == "completed" {
                    "reset"
                } else {
                    "no_reset"
                },
            ));
            let mut community = mock_host(&fixture);
            let metadata = object([
                ("phase", "routes_exhausted".into()),
                ("timeout_ms", 2000u64.into()),
                (
                    "attempts",
                    Value::Array(vec![object([
                        ("provider", "builtin:codex".into()),
                        ("account", A.into()),
                        ("status", 429u64.into()),
                    ])]),
                ),
            ])
            .stringify();
            let mut recovery = Recovery::new(&fixture.config, metadata.as_bytes()).unwrap();
            recovery.context = Some(Rc::clone(&community.ctx));
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match recovery.call(1, &[], &mut [0u8; 256]) {
                    Ok(_) => break,
                    Err(Error::PluginStatus(3)) => assert!(Instant::now() < deadline),
                    other => panic!("unexpected recovery state {other:?}"),
                }
            }
            fixture.write_state(false);
            let mut forged = params();
            forged.set(FRESH_QUOTA, B.into());
            let mut fetch = Fetch::new(
                &mut community,
                forged,
                Some(A),
                Instant::now() + Duration::from_secs(4),
            )
            .unwrap();
            loop {
                match fetch.call(1, &[], &mut [0u8; 1024]) {
                    Ok(_) => break,
                    Err(Error::PluginStatus(3)) => assert!(Instant::now() < deadline),
                    other => panic!("unexpected generation state {other:?}"),
                }
            }
            let calls = native_calls(&fixture);
            let sent = calls
                .iter()
                .find(|v| v.get("method").as_str() == Some("fetch"))
                .unwrap();
            assert_eq!(
                sent.get("params").get(FRESH_QUOTA).as_str(),
                (outcome == "completed").then_some(A)
            );
            let mut forged = params();
            forged.set(FRESH_QUOTA, A.into());
            let second = Fetch::new(
                &mut community,
                forged,
                Some(A),
                Instant::now() + Duration::from_secs(4),
            )
            .unwrap();
            assert!(matches!(
                second.params.as_ref().unwrap().get(FRESH_QUOTA),
                Value::Null
            ));
            drop(second);
            drop(fetch);
            drop(community);
        }
    }
    #[test]
    fn a_reset_intent_cannot_follow_another_or_implicit_account() {
        for explicit in [Some(B), None] {
            let fixture = Fixture::new(&response("completed", "reset"));
            fixture.write_state(false);
            let mut community = mock_host(&fixture);
            community.ctx.borrow_mut().after_reset = Some(A.into());
            let mut request = params();
            request.set(FRESH_QUOTA, A.into());
            let fetch = Fetch::new(
                &mut community,
                request,
                explicit,
                Instant::now() + Duration::from_secs(4),
            )
            .unwrap();
            assert!(matches!(
                fetch.params.as_ref().unwrap().get(FRESH_QUOTA),
                Value::Null
            ));
            assert!(community.ctx.borrow().after_reset.is_none());
            drop(fetch);
            drop(community);
        }
    }
}
