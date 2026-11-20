use crate::{
    MAX_METADATA,
    archive::{self, Manifest},
    hash,
    json::{self, Value},
    private, process,
    release::{self, Plan},
};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

// Closing one descriptor does not release a lock while a duplicate or an
// inherited descriptor still refers to it. End each exclusive operation at the
// owner's scope boundary, including error paths after lock acquisition.
struct ExclusiveLock {
    file: File,
}
impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
#[cfg(test)]
impl ExclusiveLock {
    fn try_clone(&self) -> std::io::Result<File> {
        self.file.try_clone()
    }
}

struct Control {
    directory: PathBuf,
    transactions: PathBuf,
}
impl Control {
    fn for_prefix(prefix: &Path, create: bool) -> Result<Self, String> {
        let parent = prefix.parent().ok_or("installation prefix has no parent")?;
        let name = prefix
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid installation prefix")?;
        let directory = parent.join(format!(".{name}.zcalld-installer"));
        let transactions = directory.join("transactions");
        if create {
            private::parents_private(parent)?;
            private::mkdir(&directory)?;
            private::mkdir(&transactions)?;
        } else {
            private::check_dir(&directory)?;
            private::check_dir(&transactions)?;
        }
        Ok(Self {
            directory,
            transactions,
        })
    }
    fn lock(&self) -> Result<ExclusiveLock, String> {
        let file = private::lock_file(&self.directory.join("install.lock"))?;
        file.try_lock()
            .map_err(|_| "another installer operation is active")?;
        let guard = ExclusiveLock { file };
        private::same_file(&guard.file, &self.directory.join("install.lock"))?;
        Ok(guard)
    }
    fn runtime(&self) -> Result<File, String> {
        private::lock_file(&self.directory.join("runtime.lock"))
    }
}

/// A shared lease protects the entire installed cohort, including lazily loaded
/// plugins/helpers. The installer never terminates a process to acquire it.
pub struct RuntimeLease {
    _file: File,
    version: String,
}
impl RuntimeLease {
    pub fn version(&self) -> &str {
        &self.version
    }
}
impl Drop for RuntimeLease {
    fn drop(&mut self) {
        // A fork or duplicated handle can outlive this owner. Release this
        // lease explicitly; closing one descriptor need not release its lock.
        let _ = self._file.unlock();
    }
}
/// Full verification for an explicit operation such as first service setup.
/// Keep this value alive until the setup write finishes. Ordinary status reads
/// use installed_prefix and must not hash the whole cohort on each refresh.
pub struct VerifiedInstallation {
    prefix: PathBuf,
    _lease: RuntimeLease,
}
impl VerifiedInstallation {
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }
}
pub fn verify_installation(executable: &Path) -> Result<Option<VerifiedInstallation>, String> {
    let Some(lease) = runtime_lease(executable)? else {
        return Ok(None);
    };
    if lease.version() != env!("CARGO_PKG_VERSION") {
        return Err("running setup code does not match the installed bundle version".into());
    }
    let executable = executable
        .canonicalize()
        .map_err(|_| "cannot resolve the setup executable")?;
    let prefix = installed_prefix(&executable)?.ok_or("installed setup receipt disappeared")?;
    let expected = prefix.join("bin").join(if cfg!(windows) {
        "zcalld.exe"
    } else {
        "zcalld"
    });
    if executable != expected {
        return Err("service setup requires the installed gateway executable".into());
    }
    let receipt = read_receipt(&prefix)?;
    verify_receipt(&prefix, &receipt)?;
    Ok(Some(VerifiedInstallation {
        prefix,
        _lease: lease,
    }))
}
pub fn installed_prefix(executable: &Path) -> Result<Option<PathBuf>, String> {
    let executable = executable
        .canonicalize()
        .map_err(|_| "cannot resolve the installed executable")?;
    let Some(bin) = executable.parent() else {
        return Ok(None);
    };
    if bin.file_name().and_then(|v| v.to_str()) != Some("bin") {
        return Ok(None);
    }
    let Some(prefix) = bin.parent() else {
        return Ok(None);
    };
    if !prefix.join(".zcalld-install.json").exists() {
        return Ok(None);
    }
    let receipt = read_receipt(prefix)?;
    if receipt.plan.prefix != prefix {
        return Err("installed runtime prefix does not match its receipt".into());
    }
    Ok(Some(prefix.to_path_buf()))
}
pub fn runtime_lease(executable: &Path) -> Result<Option<RuntimeLease>, String> {
    let executable = executable
        .canonicalize()
        .map_err(|_| "cannot resolve the running executable")?;
    let Some(bin) = executable.parent() else {
        return Ok(None);
    };
    if bin.file_name().and_then(|v| v.to_str()) != Some("bin") {
        return Ok(None);
    }
    let Some(prefix) = bin.parent() else {
        return Ok(None);
    };
    if !prefix.join(".zcalld-install.json").exists() {
        return Ok(None);
    }
    let receipt = read_receipt(prefix)?;
    if receipt.plan.prefix != prefix {
        return Err("installed runtime prefix does not match its receipt".into());
    }
    let control = Control::for_prefix(prefix, false)?;
    let file = control.runtime()?;
    file.try_lock_shared()
        .map_err(|_| "installation is being changed; retry after the installer finishes")?;
    private::same_file(&file, &control.directory.join("runtime.lock"))?;
    // Acquire before checking the current receipt again, closing the pre-lock
    // rename race without hashing every plugin on each ordinary CLI invocation.
    if read_receipt(prefix)?.digest != receipt.digest {
        return Err("installation changed while starting the process".into());
    }
    Ok(Some(RuntimeLease {
        _file: file,
        version: receipt.plan.version,
    }))
}

struct Receipt {
    raw: Vec<u8>,
    digest: String,
    plan: Plan,
    manifest_sha256: String,
}
fn read_receipt(prefix: &Path) -> Result<Receipt, String> {
    let raw = private::read(&prefix.join(".zcalld-install.json"), MAX_METADATA)?;
    parse_receipt(raw)
}
fn parse_receipt(raw: Vec<u8>) -> Result<Receipt, String> {
    let value = crate::strict_json(&raw, MAX_METADATA)?;
    if value.get("schema").as_u64() != Some(1)
        || value.get("owner").as_str() != Some("zcalld-installer")
        || !value
            .get("transaction_id")
            .as_str()
            .is_some_and(|s| crate::hex(s, 32))
    {
        return Err("directory is not owned by the zcalld installer".into());
    }
    let plan = Plan::parse(value.get("plan").stringify().as_bytes())?;
    let manifest_sha256 = crate::string(&value, "manifest_sha256")?;
    if !crate::hex(&manifest_sha256, 64) {
        return Err("invalid installed manifest identity".into());
    }
    Ok(Receipt {
        digest: hash::sha256(&raw),
        raw,
        plan,
        manifest_sha256,
    })
}
fn verify_receipt(prefix: &Path, receipt: &Receipt) -> Result<Manifest, String> {
    let manifest = archive::read_manifest(prefix, &receipt.plan)?;
    if manifest.digest() != receipt.manifest_sha256 {
        return Err("installed manifest changed outside the installer".into());
    }
    archive::verify_tree(prefix, &manifest, true)?;
    Ok(manifest)
}

struct Store {
    directory: PathBuf,
    id: String,
    plan: Plan,
    state: Value,
}
impl Store {
    fn create(control: &Control, plan: &Plan) -> Result<Self, String> {
        let mut count = 0;
        for entry in fs::read_dir(&control.transactions)
            .map_err(|_| "cannot inspect installation transactions")?
        {
            count += 1;
            if count > 64 {
                return Err(
                    "too many installation transactions; inspect retained recovery records".into(),
                );
            }
            let entry = entry.map_err(|_| "cannot inspect installation transaction")?;
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|s| crate::hex(s, 32))
            {
                return Err("unexpected file in installer transaction directory".into());
            }
            let state = crate::strict_json(
                &private::read(&entry.path().join("state.json"), MAX_METADATA)?,
                MAX_METADATA,
            )?;
            if !matches!(
                state.get("phase").as_str(),
                Some("committed" | "rolled_back" | "failed")
            ) {
                return Err(
                    "an unfinished installation must be resumed or rolled back first".into(),
                );
            }
        }
        let id = private::random_id()?;
        let directory = control.transactions.join(&id);
        private::mkdir(&directory)?;
        let plan_bytes = plan.value().stringify();
        private::atomic(&directory.join("plan.json"), plan_bytes.as_bytes())?;
        let state = json::object([
            ("schema", 1usize.into()),
            ("id", id.clone().into()),
            ("plan_sha256", hash::sha256(plan_bytes.as_bytes()).into()),
            ("phase", "preparing".into()),
            ("previous_receipt_sha256", Value::Null),
            ("manifest_sha256", Value::Null),
            ("error_code", Value::Null),
            ("updated_at_ms", crate::now_ms().into()),
        ]);
        let store = Self {
            directory,
            id,
            plan: plan.clone(),
            state,
        };
        store.save()?;
        Ok(store)
    }
    fn load(directory: &Path) -> Result<Self, String> {
        private::check_dir(directory)?;
        let directory = directory
            .canonicalize()
            .map_err(|_| "cannot resolve installation transaction")?;
        let id = directory
            .file_name()
            .and_then(|v| v.to_str())
            .filter(|s| crate::hex(s, 32))
            .ok_or("invalid transaction ID")?
            .to_owned();
        let bytes = private::read(&directory.join("plan.json"), MAX_METADATA)?;
        let plan = Plan::parse(&bytes)?;
        let control = Control::for_prefix(&plan.prefix, false)?;
        if control.transactions.join(&id) != directory {
            return Err("transaction path is not bound to its installation prefix".into());
        }
        let state = crate::strict_json(
            &private::read(&directory.join("state.json"), MAX_METADATA)?,
            MAX_METADATA,
        )?;
        if state.get("schema").as_u64() != Some(1)
            || state.get("id").as_str() != Some(&id)
            || state.get("plan_sha256").as_str() != Some(&hash::sha256(&bytes))
        {
            return Err("installation journal identity does not match its plan".into());
        }
        if !matches!(
            state.get("phase").as_str(),
            Some(
                "preparing"
                    | "prepared"
                    | "pending_exit"
                    | "activating"
                    | "old_saved"
                    | "installed"
                    | "committed"
                    | "rolled_back"
                    | "failed"
                    | "needs_attention"
                    | "rollback_pending"
            )
        ) {
            return Err("unknown installation journal phase".into());
        }
        Ok(Self {
            directory,
            id,
            plan,
            state,
        })
    }
    fn save(&self) -> Result<(), String> {
        private::atomic(
            &self.directory.join("state.json"),
            self.state.stringify().as_bytes(),
        )
    }
    fn set_phase(&mut self, phase: &str) -> Result<(), String> {
        self.state.set("phase", phase.into());
        self.state.set("updated_at_ms", crate::now_ms().into());
        self.save()
    }
    fn phase(&self) -> &str {
        self.state
            .get("phase")
            .as_str()
            .unwrap_or("needs_attention")
    }
    fn old(&self) -> Result<Option<Receipt>, String> {
        match self.state.get("previous_receipt_sha256") {
            Value::Null => Ok(None),
            Value::String(expected) => {
                let receipt = parse_receipt(private::read(
                    &self.directory.join("previous.json"),
                    MAX_METADATA,
                )?)?;
                if &receipt.digest != expected {
                    return Err("previous installation receipt changed".into());
                }
                Ok(Some(receipt))
            }
            _ => Err("invalid previous installation identity".into()),
        }
    }
    fn manifest(&self, root: &Path) -> Result<Manifest, String> {
        let manifest = archive::read_manifest(root, &self.plan)?;
        if self.state.get("manifest_sha256").as_str() != Some(&manifest.digest()) {
            return Err("prepared installation manifest changed".into());
        }
        archive::verify_tree(root, &manifest, true)?;
        let receipt = read_receipt(root)?;
        if receipt.plan.value() != self.plan.value() || receipt.manifest_sha256 != manifest.digest()
        {
            return Err("prepared installation receipt changed".into());
        }
        Ok(manifest)
    }
    fn outcome(&self) -> Value {
        json::object([
            ("ok", true.into()),
            ("phase", self.phase().into()),
            ("committed", (self.phase() == "committed").into()),
            (
                "pending_exit",
                matches!(self.phase(), "pending_exit" | "rollback_pending").into(),
            ),
            ("restart_required", (self.phase() == "committed").into()),
            (
                "transaction",
                self.directory.to_string_lossy().into_owned().into(),
            ),
            (
                "prefix",
                self.plan.prefix.to_string_lossy().into_owned().into(),
            ),
            (
                "binary_path",
                self.plan
                    .prefix
                    .join("bin")
                    .join(if cfg!(windows) {
                        "zcalld.exe"
                    } else {
                        "zcalld"
                    })
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            ),
            ("version", self.plan.version.clone().into()),
            ("target", self.plan.target.clone().into()),
            ("source", self.plan.source.clone().into()),
            (
                "verified_source",
                if self.plan.local_archive.is_some() {
                    "local-sha256"
                } else {
                    "github-release"
                }
                .into(),
            ),
            ("error_code", self.state.get("error_code").clone()),
        ])
    }
}

fn probe(root: &Path, version: &str) -> Result<(), String> {
    for name in ["zcalld", "zcalld-installer"] {
        let program = root
            .join("bin")
            .join(format!("{name}{}", if cfg!(windows) { ".exe" } else { "" }));
        let mut command = process::command(program);
        command.arg("--version");
        let out = process::capture(command, 4096, Duration::from_secs(5))?;
        let out = std::str::from_utf8(&out)
            .map_err(|_| "installed executable has invalid version output")?;
        if out.trim() != format!("{name} {version}") {
            return Err("installed executable version does not match its manifest".into());
        }
    }
    Ok(())
}
fn prepare(store: &mut Store, probe: fn(&Path, &str) -> Result<(), String>) -> Result<(), String> {
    if store.plan.prefix.exists() {
        let old = read_receipt(&store.plan.prefix)?;
        if old.plan.prefix != store.plan.prefix {
            return Err("existing installation belongs to a different prefix".into());
        }
        verify_receipt(&store.plan.prefix, &old)?;
        private::atomic(&store.directory.join("previous.json"), &old.raw)?;
        store
            .state
            .set("previous_receipt_sha256", old.digest.into());
        store.save()?;
    }
    release::obtain(&store.plan, &store.directory.join("archive"))?;
    let stage = store.directory.join("stage");
    let manifest = archive::unpack(&store.directory.join("archive"), &stage, &store.plan)?;
    probe(&stage, &store.plan.version)?;
    let receipt = json::object([
        ("schema", 1usize.into()),
        ("owner", "zcalld-installer".into()),
        ("transaction_id", store.id.clone().into()),
        ("manifest_sha256", manifest.digest().into()),
        ("plan", store.plan.value()),
    ]);
    private::atomic(
        &stage.join(".zcalld-install.json"),
        receipt.stringify().as_bytes(),
    )?;
    store.state.set("manifest_sha256", manifest.digest().into());
    store.save()?;
    copy_helper(&stage, store)?;
    private::sync_dir(&stage)?;
    store.set_phase("prepared")
}
fn copy_helper(stage: &Path, store: &Store) -> Result<(), String> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let source = stage.join("bin").join(format!("zcalld-installer{suffix}"));
    let destination = store.directory.join(format!("runner{suffix}"));
    let input = File::open(source).map_err(|_| "cannot open verified installer helper")?;
    let mut output = private::create(&destination)?;
    std::io::copy(&mut input.take(crate::MAX_FILE + 1), &mut output)
        .map_err(|_| "cannot stage independent installer helper")?;
    private::mode(&output, 0o700)?;
    output
        .sync_all()
        .map_err(|_| "cannot sync independent installer helper")?;
    Ok(())
}
fn wait_runtime(control: &Control, wait: Duration) -> Result<Option<ExclusiveLock>, String> {
    let file = control.runtime()?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => {
                let guard = ExclusiveLock { file };
                private::same_file(&guard.file, &control.directory.join("runtime.lock"))?;
                return Ok(Some(guard));
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(_) => return Err("cannot acquire installation runtime lease".into()),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn activate(store: &mut Store, control: &Control, wait: Duration) -> Result<Value, String> {
    if matches!(store.phase(), "committed" | "rolled_back") {
        return Ok(store.outcome());
    }
    if !matches!(
        store.phase(),
        "prepared" | "pending_exit" | "activating" | "old_saved" | "installed"
    ) {
        return Err("transaction is not ready for activation".into());
    }
    let Some(_runtime) = wait_runtime(control, wait)? else {
        store.set_phase("pending_exit")?;
        return Ok(store.outcome());
    };
    let stage = store.directory.join("stage");
    let previous = store.directory.join("previous");
    let prefix = store.plan.prefix.clone();
    let old = store.old()?;
    // A crash after the second rename leaves a complete new tree. Finish only
    // after checking both the manifest and any saved previous cohort.
    if !stage.exists() && prefix.exists() {
        store.manifest(&prefix)?;
        if let Some(old) = &old {
            verify_receipt(&previous, old)?;
        } else if previous.exists() {
            return Err("unexpected previous installation directory".into());
        }
        store.set_phase("committed")?;
        return Ok(store.outcome());
    }
    store.manifest(&stage)?;
    if previous.exists() {
        let old = old
            .as_ref()
            .ok_or("unexpected previous installation directory")?;
        verify_receipt(&previous, old)?;
        if prefix.exists() {
            return Err("both previous and active directories exist before activation".into());
        }
    } else if let Some(old) = &old {
        let current = read_receipt(&prefix)?;
        if current.digest != old.digest {
            return Err("existing installation changed after preparation".into());
        }
        verify_receipt(&prefix, old)?;
    } else if prefix.exists() {
        return Err("refusing to replace an unowned installation directory".into());
    }
    store.set_phase("activating")?;
    if old.is_some() && !previous.exists() {
        if let Err(error) = fs::rename(&prefix, &previous) {
            if cfg!(windows)
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                )
            {
                store.set_phase("pending_exit")?;
                return Ok(store.outcome());
            }
            return Err("cannot preserve the previous installation".into());
        }
        private::sync_dir(prefix.parent().unwrap())?;
        private::sync_dir(&store.directory)?;
    }
    let result = (|| {
        store.set_phase("old_saved")?;
        fault("old_saved")?;
        fs::rename(&stage, &prefix).map_err(|_| "cannot activate the complete native bundle")?;
        private::sync_dir(prefix.parent().unwrap())?;
        store.set_phase("installed")?;
        fault("installed")?;
        store.manifest(&prefix)?;
        store.set_phase("committed")?;
        Ok(store.outcome())
    })();
    if result.is_err() {
        store.state.set("error_code", "activation_failed".into());
        if restore(store).is_err() {
            let _ = store.set_phase("needs_attention");
        }
    }
    result
}
fn restore(store: &mut Store) -> Result<(), String> {
    let prefix = store.plan.prefix.clone();
    let previous = store.directory.join("previous");
    let failed = store.directory.join("failed");
    let old = store.old()?;
    if let Some(old) = &old {
        if previous.exists() {
            verify_receipt(&previous, old)?;
        } else {
            let current = read_receipt(&prefix)?;
            if current.digest != old.digest {
                return Err("previous installation is unavailable for rollback".into());
            }
            verify_receipt(&prefix, old)?;
            return store.set_phase("rolled_back");
        }
    }
    if prefix.exists() {
        store.manifest(&prefix)?;
        if failed.exists() {
            return Err("rollback recovery directory already exists".into());
        }
        fs::rename(&prefix, &failed)
            .map_err(|_| "cannot preserve the candidate during rollback")?;
    }
    if old.is_some() {
        fs::rename(&previous, &prefix)
            .map_err(|_| "cannot restore the previous complete installation")?;
    }
    private::sync_dir(prefix.parent().unwrap())?;
    store.set_phase("rolled_back")
}

pub fn apply(plan: &Plan) -> Result<Value, String> {
    apply_with_probe(plan, probe)
}
fn apply_with_probe(
    plan: &Plan,
    probe: fn(&Path, &str) -> Result<(), String>,
) -> Result<Value, String> {
    plan.validate()?;
    let control = Control::for_prefix(&plan.prefix, true)?;
    let _lock = control.lock()?;
    if plan.prefix.exists() && !plan.prefix.join(".zcalld-install.json").exists() {
        return Err(
            "existing directory has no installer receipt; choose a new private prefix".into(),
        );
    }
    let mut store = Store::create(&control, plan)?;
    if let Err(error) = prepare(&mut store, probe) {
        store.state.set("error_code", "preparation_failed".into());
        let _ = store.set_phase("failed");
        return Err(format!(
            "{error}; installation journal: {}",
            store.directory.display()
        ));
    }
    activate(&mut store, &control, Duration::ZERO).map_err(|error| {
        format!(
            "{error}; installation journal: {}",
            store.directory.display()
        )
    })
}
pub fn status(transaction: &Path) -> Result<Value, String> {
    Ok(Store::load(transaction)?.outcome())
}
pub fn resume(transaction: &Path) -> Result<Value, String> {
    resume_wait(transaction, Duration::ZERO)
}
pub fn resume_wait(transaction: &Path, wait: Duration) -> Result<Value, String> {
    if wait > Duration::from_secs(120) {
        return Err("installer resume wait is limited to 120 seconds".into());
    }
    let mut store = Store::load(transaction)?;
    let control = Control::for_prefix(&store.plan.prefix, false)?;
    let _lock = control.lock()?;
    if store.phase() == "rollback_pending" {
        let Some(_runtime) = wait_runtime(&control, wait)? else {
            return Ok(store.outcome());
        };
        restore(&mut store)?;
        Ok(store.outcome())
    } else {
        activate(&mut store, &control, wait)
    }
}
pub fn rollback(transaction: &Path) -> Result<Value, String> {
    let mut store = Store::load(transaction)?;
    let control = Control::for_prefix(&store.plan.prefix, false)?;
    let _lock = control.lock()?;
    if store.phase() == "rolled_back" {
        return Ok(store.outcome());
    }
    let Some(_runtime) = wait_runtime(&control, Duration::ZERO)? else {
        store.set_phase("rollback_pending")?;
        return Ok(store.outcome());
    };
    restore(&mut store)?;
    Ok(store.outcome())
}
pub fn spawn_resume(transaction: &Path) -> Result<Value, String> {
    let store = Store::load(transaction)?;
    if store.phase() != "pending_exit" {
        return Err("only a pending installation can start a deferred helper".into());
    }
    let source = store.directory.join(if cfg!(windows) {
        "runner.exe"
    } else {
        "runner"
    });
    let stage = store.directory.join("stage");
    let manifest = store.manifest(&stage)?;
    let name = if cfg!(windows) {
        "bin/zcalld-installer.exe"
    } else {
        "bin/zcalld-installer"
    };
    let expected = manifest
        .files
        .get(name)
        .ok_or("verified helper is missing")?;
    if hash::sha256_file(&source, expected.size)? != expected.sha256 {
        return Err("deferred helper checksum mismatch".into());
    }
    let mut command = process::command(source);
    command
        .args(["resume", "--transaction"])
        .arg(&store.directory)
        .args(["--wait-ms", "120000"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
        .spawn()
        .map_err(|_| "cannot start verified deferred installer")?;
    let mut out = store.outcome();
    out.set("helper_started", true.into());
    Ok(out)
}

#[cfg(test)]
thread_local! {static FAULT:std::cell::RefCell<Option<(&'static str,bool)>>=const{std::cell::RefCell::new(None)};}
fn fault(_point: &str) -> Result<(), String> {
    #[cfg(test)]
    {
        let value = FAULT.with(|f| *f.borrow());
        if let Some((point, crash)) = value
            && point == _point
        {
            if crash {
                panic!("simulated installer interruption");
            }
            return Err("simulated installer I/O failure".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "zcalld-installer-test-{}",
                private::random_id().unwrap()
            ));
            private::mkdir(&root).unwrap();
            Self(root)
        }
        fn prefix(&self) -> PathBuf {
            self.0.join("runtime")
        }
        fn bundle(&self, version: &str, marker: &str) -> Plan {
            let stem = format!("zcalld-{version}-{}", release::host_target());
            let input = self.0.join(format!("input-{version}"));
            private::mkdir(&input).unwrap();
            let root = input.join(&stem);
            private::mkdir(&root).unwrap();
            let mut names = archive::required(release::host_target());
            names.push("share/doc/zcalld/README.md".into());
            names.push("etc/zcalld/config.conf".into());
            if cfg!(windows) {
                names.push("bin/libcurl.dll".into());
            }
            let mut files = Vec::new();
            for name in &names {
                let path = root.join(name);
                private::parents_private(path.parent().unwrap()).unwrap();
                let bytes = format!("{marker}:{name}\n").into_bytes();
                let mut file = private::create(&path).unwrap();
                file.write_all(&bytes).unwrap();
                let mode = if name.starts_with("bin/") {
                    0o755
                } else {
                    0o644
                };
                private::mode(&file, mode).unwrap();
                files.push(json::object([
                    ("path", name.clone().into()),
                    ("size", bytes.len().into()),
                    ("sha256", hash::sha256(&bytes).into()),
                    ("mode", (mode as u64).into()),
                ]));
            }
            let manifest = json::object([
                ("schema", 1usize.into()),
                ("product", "zcalld".into()),
                ("version", version.into()),
                ("target", release::host_target().into()),
                ("source", "a".repeat(40).into()),
                ("files", Value::Array(files)),
                (
                    "required",
                    Value::Array(
                        archive::required(release::host_target())
                            .into_iter()
                            .map(Value::from)
                            .collect(),
                    ),
                ),
            ]);
            let mut file = private::create(&root.join("INSTALL.json")).unwrap();
            file.write_all(manifest.stringify().as_bytes()).unwrap();
            private::mode(&file, 0o644).unwrap();
            drop(file);
            let extension = if cfg!(windows) { "zip" } else { "tar" };
            let output = self.0.join(format!("bundle-{version}.{extension}"));
            let status = std::process::Command::new(archive::tar().unwrap())
                .current_dir(&self.0)
                .arg(if cfg!(windows) { "-caf" } else { "-cf" })
                .arg(output.file_name().unwrap())
                .arg("-C")
                .arg(input.file_name().unwrap())
                .arg(&stem)
                .status()
                .unwrap();
            assert!(status.success(), "native archive creation failed: {status}");
            Plan::local(
                &output,
                &hash::sha256_file(&output, crate::MAX_ARCHIVE).unwrap(),
                version,
                &"a".repeat(40),
                &self.prefix(),
            )
            .unwrap()
        }
        fn last(&self, phase: &str) -> PathBuf {
            let control = Control::for_prefix(&self.prefix(), false).unwrap();
            fs::read_dir(control.transactions)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| Store::load(p).unwrap().phase() == phase)
                .unwrap()
        }
        fn core(&self) -> Vec<u8> {
            fs::read(self.prefix().join("bin").join(if cfg!(windows) {
                "zcalld.exe"
            } else {
                "zcalld"
            }))
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            FAULT.with(|f| *f.borrow_mut() = None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fake_probe(_: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn assert_exclusive_guard_releases<G>(guard: G, duplicate: File, path: &Path) {
        let contender = private::lock_file(path).unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(guard);
        contender
            .try_lock()
            .expect("exclusive guard must unlock while an old duplicate remains open");
        drop(duplicate);
        let observer = private::lock_file(path).unwrap();
        assert!(matches!(
            observer.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        contender.unlock().unwrap();
        observer.try_lock().unwrap();
        observer.unlock().unwrap();
    }

    #[test]
    fn installer_operation_unlocks_while_an_old_duplicate_is_open() {
        let f = Fixture::new();
        let control = Control::for_prefix(&f.prefix(), true).unwrap();
        let guard = control.lock().unwrap();
        let duplicate = guard.try_clone().unwrap();
        assert_exclusive_guard_releases(guard, duplicate, &control.directory.join("install.lock"));
    }

    #[test]
    fn installer_activation_unlocks_while_an_old_duplicate_is_open() {
        let f = Fixture::new();
        let control = Control::for_prefix(&f.prefix(), true).unwrap();
        let guard = wait_runtime(&control, Duration::ZERO).unwrap().unwrap();
        let duplicate = guard.try_clone().unwrap();
        assert_exclusive_guard_releases(guard, duplicate, &control.directory.join("runtime.lock"));
    }

    #[test]
    fn service_setup_verifies_cohort_and_holds_runtime_lease() {
        let f = Fixture::new();
        let plan = f.bundle(env!("CARGO_PKG_VERSION"), "setup");
        apply_with_probe(&plan, fake_probe).unwrap();
        let exe = f.prefix().join("bin").join(if cfg!(windows) {
            "zcalld.exe"
        } else {
            "zcalld"
        });
        let verified = verify_installation(&exe).unwrap().unwrap();
        assert_eq!(verified.prefix(), f.prefix());
        let control = Control::for_prefix(&f.prefix(), false).unwrap();
        let exclusive = control.runtime().unwrap();
        assert!(matches!(
            exclusive.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        let old_duplicate = verified._lease._file.try_clone().unwrap();
        let other = runtime_lease(&exe).unwrap().unwrap();
        drop(verified);
        assert!(matches!(
            exclusive.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(other);
        exclusive
            .try_lock()
            .expect("runtime lease must unlock while an old duplicate handle remains open");
        drop(old_duplicate);
        let contender = control.runtime().unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        exclusive.unlock().unwrap();
        contender.try_lock().unwrap();
        contender.unlock().unwrap();
        fs::write(
            f.prefix().join("share/doc/zcalld/README.md"),
            b"changed after install",
        )
        .unwrap();
        assert!(verify_installation(&exe).is_err());
    }
    #[test]
    fn complete_cohort_update_and_rollback_preserve_external_config() {
        let f = Fixture::new();
        let config = f.0.join("user-config");
        fs::write(&config, b"user data must survive").unwrap();
        let old = f.bundle("1.0.0", "old");
        assert_eq!(
            apply_with_probe(&old, fake_probe)
                .unwrap()
                .get("phase")
                .as_str(),
            Some("committed")
        );
        let bytes = f.core();
        let new = f.bundle("1.0.1", "new");
        let result = apply_with_probe(&new, fake_probe).unwrap();
        assert_ne!(f.core(), bytes);
        let tx = PathBuf::from(result.get("transaction").as_str().unwrap());
        assert_eq!(
            rollback(&tx).unwrap().get("phase").as_str(),
            Some("rolled_back")
        );
        assert_eq!(f.core(), bytes);
        assert_eq!(fs::read(config).unwrap(), b"user data must survive");
        verify_receipt(&f.prefix(), &read_receipt(&f.prefix()).unwrap()).unwrap();
    }
    #[test]
    fn a_live_runtime_holds_the_whole_old_cohort_until_explicit_resume() {
        let f = Fixture::new();
        let old = f.bundle("1.0.0", "old");
        apply_with_probe(&old, fake_probe).unwrap();
        let bytes = f.core();
        let executable = f.prefix().join("bin").join(if cfg!(windows) {
            "zcalld.exe"
        } else {
            "zcalld"
        });
        let lease = runtime_lease(&executable).unwrap().unwrap();
        let new = f.bundle("1.0.1", "new");
        let result = apply_with_probe(&new, fake_probe).unwrap();
        assert_eq!(result.get("phase").as_str(), Some("pending_exit"));
        assert_eq!(f.core(), bytes);
        let tx = PathBuf::from(result.get("transaction").as_str().unwrap());
        assert_eq!(status(&tx).unwrap().get("committed").as_bool(), Some(false));
        drop(lease);
        assert_eq!(
            resume(&tx).unwrap().get("phase").as_str(),
            Some("committed")
        );
        assert_ne!(f.core(), bytes);
    }
    #[test]
    fn interrupted_directory_switch_resumes_without_mixing_components() {
        let f = Fixture::new();
        let old = f.bundle("1.0.0", "old");
        apply_with_probe(&old, fake_probe).unwrap();
        let new = f.bundle("1.0.1", "new");
        FAULT.with(|v| *v.borrow_mut() = Some(("old_saved", true)));
        assert!(std::panic::catch_unwind(|| apply_with_probe(&new, fake_probe)).is_err());
        FAULT.with(|v| *v.borrow_mut() = None);
        assert!(!f.prefix().exists());
        let tx = f.last("old_saved");
        assert_eq!(
            resume(&tx).unwrap().get("phase").as_str(),
            Some("committed")
        );
        verify_receipt(&f.prefix(), &read_receipt(&f.prefix()).unwrap()).unwrap();
        assert!(String::from_utf8(f.core()).unwrap().starts_with("new:"));
    }
    #[test]
    fn post_activation_io_failure_rolls_back_every_native_component() {
        let f = Fixture::new();
        let old = f.bundle("1.0.0", "old");
        apply_with_probe(&old, fake_probe).unwrap();
        let bytes = f.core();
        let new = f.bundle("1.0.1", "new");
        FAULT.with(|v| *v.borrow_mut() = Some(("installed", false)));
        let error = apply_with_probe(&new, fake_probe).unwrap_err();
        FAULT.with(|v| *v.borrow_mut() = None);
        let journal = PathBuf::from(
            error
                .rsplit_once("; installation journal: ")
                .expect("failed activation must expose its recovery journal")
                .1,
        );
        assert_eq!(journal, f.last("rolled_back"));
        assert_eq!(
            status(&journal).unwrap().get("phase").as_str(),
            Some("rolled_back")
        );
        assert_eq!(f.core(), bytes);
        let tx = f.last("rolled_back");
        assert!(tx.join("failed").exists());
        verify_receipt(&f.prefix(), &read_receipt(&f.prefix()).unwrap()).unwrap();
    }
    #[test]
    fn unknown_prefix_and_modified_owned_files_are_preserved() {
        let f = Fixture::new();
        let old = f.bundle("1.0.0", "old");
        private::mkdir(&f.prefix()).unwrap();
        fs::write(f.prefix().join("user-file"), b"keep").unwrap();
        assert!(apply_with_probe(&old, fake_probe).is_err());
        assert_eq!(fs::read(f.prefix().join("user-file")).unwrap(), b"keep");
        fs::remove_file(f.prefix().join("user-file")).unwrap();
        fs::remove_dir(f.prefix()).unwrap();
        apply_with_probe(&old, fake_probe).unwrap();
        let core = f.prefix().join("bin").join(if cfg!(windows) {
            "zcalld.exe"
        } else {
            "zcalld"
        });
        fs::write(&core, b"edited locally").unwrap();
        let new = f.bundle("1.0.1", "new");
        assert!(apply_with_probe(&new, fake_probe).is_err());
        assert_eq!(fs::read(core).unwrap(), b"edited locally");
    }
    #[test]
    fn stable_installer_lock_excludes_a_second_operation() {
        let f = Fixture::new();
        let control = Control::for_prefix(&f.prefix(), true).unwrap();
        let first = control.lock().unwrap();
        assert!(control.lock().is_err());
        drop(first);
        assert!(control.lock().is_ok());
    }
}
