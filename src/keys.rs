//! Explicit, on-demand caller-key changes. Secrets enter through stdin only.
use crate::{
    config::Config,
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions, Permissions},
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use zcall::server::auth::{ApiKey, CallerKeys, MAX_FILE_BYTES};

const MAX_SECRET: usize = 16 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Action {
    List,
    Add,
    Rotate,
    Revoke,
}
struct Arguments {
    action: Action,
    id: Option<String>,
    name: Option<String>,
    lan: bool,
    workspace: Option<String>,
}

fn parse(args: &[String]) -> Result<Arguments, String> {
    let action = match args.first().map(String::as_str) {
        Some("list") => Action::List,
        Some("add") => Action::Add,
        Some("rotate") => Action::Rotate,
        Some("revoke") => Action::Revoke,
        _ => return Err("keys expects list, add, rotate or revoke".into()),
    };
    let mut result = Arguments {
        action,
        id: None,
        name: None,
        lan: false,
        workspace: None,
    };
    let mut index = 1;
    if action != Action::List {
        let id = args.get(index).ok_or("a caller key ID is required")?;
        if id.is_empty()
            || id.len() > 512
            || id.trim() != id
            || id.starts_with('-')
            || id.chars().any(char::is_control)
        {
            return Err("caller key ID must be nonempty text up to 512 bytes".into());
        }
        result.id = Some(id.clone());
        index += 1;
    }
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let option = args[index].as_str();
        if !seen.insert(option) {
            return Err("duplicate caller key option".into());
        }
        match option {
            "--api-key-stdin" if matches!(action, Action::Add | Action::Rotate) => {}
            "--lan" if action == Action::Add => result.lan = true,
            "--name" | "--workspace" if action == Action::Add => {
                index += 1;
                let value = args
                    .get(index)
                    .filter(|value| !value.starts_with("--"))
                    .ok_or("caller key metadata option requires a value")?;
                if option == "--name" {
                    result.name = Some(value.clone());
                } else {
                    result.workspace = Some(value.clone());
                }
            }
            _ => return Err("unknown or unsupported caller key argument".into()),
        }
        index += 1;
    }
    if matches!(action, Action::Add | Action::Rotate) && !seen.contains("--api-key-stdin") {
        return Err(
            "add and rotate require --api-key-stdin; never put a secret in arguments".into(),
        );
    }
    Ok(result)
}

fn secret(input: impl Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    input
        .take((MAX_SECRET + 3) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read caller key from stdin")?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    let value = String::from_utf8(bytes).map_err(|_| "caller key must be UTF-8")?;
    if value.is_empty()
        || value.len() > MAX_SECRET
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(
            "caller key must be one nonempty line up to 16 KiB without surrounding whitespace"
                .into(),
        );
    }
    Ok(value)
}

pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    let arguments = parse(args)?;
    let supplied = if matches!(arguments.action, Action::Add | Action::Rotate) {
        Some(secret(std::io::stdin().lock())?)
    } else {
        None
    };
    control::emit(execute(context, &arguments, supplied)?)
}

fn empty_list() -> Value {
    object([
        ("configured", false.into()),
        ("count", 0usize.into()),
        ("keys", Value::Array(Vec::new())),
        ("runtime_state", "unknown".into()),
    ])
}

fn execute(
    context: &Context,
    arguments: &Arguments,
    supplied: Option<String>,
) -> Result<Value, String> {
    if !context.path.exists() && fs::symlink_metadata(&context.path).is_err() {
        if arguments.action == Action::List {
            return Ok(empty_list());
        }
        return Err(
            "create a gateway configuration with zcalld init before adding caller keys".into(),
        );
    }
    let directory = fs::canonicalize(&context.directory)
        .map_err(|_| "cannot resolve the gateway configuration directory")?;
    secure_directory(&directory)?;
    let path = directory.join(
        context
            .path
            .file_name()
            .ok_or("configuration path must name a file")?,
    );
    let context = Context {
        path,
        directory,
        home: context.home.clone(),
    };
    let _config_lock = PrivateLock::take(&context.directory.join(".caller-keys.lock"))?;
    let configuration =
        Snapshot::read(&context.path, false)?.ok_or("gateway configuration does not exist")?;
    let text = std::str::from_utf8(&configuration.bytes)
        .map_err(|_| "gateway configuration must be UTF-8")?;
    let binding = caller_key_binding(text)?;
    if binding.is_none() && arguments.action == Action::List {
        return Ok(empty_list());
    }
    if binding.is_none() && arguments.action != Action::Add {
        return Err("no caller key file is configured; add a key first".into());
    }
    let target = binding.as_deref().unwrap_or("caller-keys.json");
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        context.directory.join(target)
    };
    let parent = fs::canonicalize(
        target
            .parent()
            .ok_or("caller key file must have a parent directory")?,
    )
    .map_err(|_| "caller key directory must already exist")?;
    secure_directory(&parent)?;
    let filename = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("caller key file must have a UTF-8 filename")?;
    let target = parent.join(filename);
    if target == context.path || target == context.directory.join(".caller-keys.lock") {
        return Err("caller key file must not replace the configuration or its lock".into());
    }
    let _keys_lock = PrivateLock::take(&parent.join(format!(".{filename}.zcalld-keys.lock")))?;
    let previous = Snapshot::read(&target, true)?;
    if binding.is_some() && previous.is_none() && arguments.action != Action::Add {
        return Err("configured caller key file is missing".into());
    }
    let mut keys = previous
        .as_ref()
        .map(|snapshot| CallerKeys::from_json(&snapshot.bytes))
        .transpose()?
        .unwrap_or_default();
    if arguments.action == Action::List {
        let entries = keys.entries().map(metadata).collect::<Vec<_>>();
        return Ok(object([
            ("configured", true.into()),
            ("count", entries.len().into()),
            ("keys", Value::Array(entries)),
            ("runtime_state", "unknown".into()),
        ]));
    }

    let id = arguments.id.as_deref().expect("parsed mutation has an ID");
    match arguments.action {
        Action::Add => {
            let mut key =
                ApiKey::new(id, supplied.ok_or("stdin key missing")?)?.with_lan(arguments.lan);
            if let Some(name) = &arguments.name {
                key = key.with_name(name)?;
            }
            if let Some(workspace) = &arguments.workspace {
                key = key.with_workspace(workspace)?;
            }
            keys.insert(key)?;
        }
        Action::Rotate => keys.rotate(id, supplied.ok_or("stdin key missing")?)?,
        Action::Revoke if !keys.revoke(id) => {
            return Err(
                "caller key ID not found; anonymous legacy keys have no revocable ID".into(),
            );
        }
        Action::Revoke => {}
        Action::List => unreachable!(),
    }
    let next_keys = keys.to_json()?;
    let next_config = if binding.is_none() {
        let updated = control::ini_set(text, "server", "caller_keys_file", "caller-keys.json")?;
        if text.contains("\r\n") && !text.replace("\r\n", "").contains('\n') {
            updated.replace('\n', "\r\n")
        } else {
            updated
        }
    } else {
        text.to_string()
    };
    validate_startup(&context, &next_config, &next_keys, keys.allows_lan())?;
    let mut changes = vec![Change {
        path: target,
        before: previous,
        after: next_keys,
    }];
    if binding.is_none() {
        changes.push(Change {
            path: context.path.clone(),
            before: Some(configuration),
            after: next_config.into_bytes(),
        });
    } else {
        configuration.check(&context.path, false)?;
    }
    commit(&context, &changes, json::atomic_write)?;
    Ok(object([
        ("ok", true.into()),
        (
            "action",
            match arguments.action {
                Action::Add => "add",
                Action::Rotate => "rotate",
                Action::Revoke => "revoke",
                Action::List => unreachable!(),
            }
            .into(),
        ),
        ("id", id.into()),
        ("count", keys.entries().len().into()),
        ("saved", true.into()),
        ("applied", false.into()),
        ("restart_required", true.into()),
        (
            "message",
            "Saved caller keys. Restart the gateway explicitly to apply this change.".into(),
        ),
    ]))
}

fn metadata(value: zcall::server::auth::KeyMetadata<'_>) -> Value {
    let optional = |value: Option<&str>| value.map(Value::from).unwrap_or(Value::Null);
    object([
        ("id", optional(value.id)),
        ("name", optional(value.name)),
        ("lan", value.lan.into()),
        ("workspace", optional(value.workspace)),
        ("manageable", value.id.is_some().into()),
    ])
}

/// Reject ambiguous server fields before selecting a file, including on list.
fn caller_key_binding(text: &str) -> Result<Option<String>, String> {
    let mut server = false;
    let mut seen_server = false;
    let mut fields = BTreeSet::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            server = section.trim() == "server";
            if server && seen_server {
                return Err("duplicate server configuration section".into());
            }
            seen_server |= server;
        } else if server {
            let (key, _) = line.split_once('=').ok_or("invalid server configuration")?;
            if !fields.insert(key.trim()) {
                return Err("duplicate server configuration field".into());
            }
        }
    }
    let value = control::ini_get(text, "server", "caller_keys_file");
    if value.as_ref().is_some_and(|value| value.is_empty()) {
        return Err("caller_keys_file must name a private JSON file".into());
    }
    Ok(value)
}

fn validate_startup(
    context: &Context,
    text: &str,
    bytes: &[u8],
    allows_lan: bool,
) -> Result<(), String> {
    let listen =
        control::ini_get(text, "server", "listen").unwrap_or_else(|| "127.0.0.1:9415".into());
    if listen != "none"
        && listen
            .parse::<SocketAddr>()
            .is_ok_and(|address| !address.ip().is_loopback())
        && !allows_lan
        && control::ini_get(text, "server", "token_env").is_none_or(|name| name.is_empty())
    {
        return Err(
            "cannot remove the last LAN-capable key while a non-loopback listener has no token_env"
                .into(),
        );
    }
    // Validate against a private temporary key file before changing either real file.
    // Config::parse reads local configuration/credentials; it starts no plugins or I/O clients.
    let temporary = Temporary::create(&context.directory, bytes)?;
    let path = temporary
        .path
        .to_str()
        .ok_or("temporary key path must be UTF-8")?;
    let validation = control::ini_set(text, "server", "caller_keys_file", path)?;
    Config::parse(&validation, &context.directory)
        .map_err(|error| format!("caller key change would leave invalid configuration: {error}"))?;
    Ok(())
}

struct Snapshot {
    bytes: Vec<u8>,
    permissions: Permissions,
}
impl Snapshot {
    fn read(path: &Path, private: bool) -> Result<Option<Self>, String> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("cannot inspect caller key configuration file".into()),
        };
        secure_file(&metadata, private)?;
        let mut options = OpenOptions::new();
        options.read(true);
        no_follow(&mut options);
        let file = options
            .open(path)
            .map_err(|_| "cannot open caller key configuration file")?;
        let held = file
            .metadata()
            .map_err(|_| "cannot inspect opened configuration file")?;
        secure_file(&held, private)?;
        same_file(&metadata, &held)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read caller key configuration file")?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err("caller key configuration exceeds 256 KiB".into());
        }
        Ok(Some(Self {
            bytes,
            permissions: held.permissions(),
        }))
    }

    fn check(&self, path: &Path, private: bool) -> Result<(), String> {
        if Snapshot::read(path, private)?.is_none_or(|current| current.bytes != self.bytes) {
            return Err(
                "configuration changed during the key operation; no files were changed".into(),
            );
        }
        Ok(())
    }
}

struct Change {
    path: PathBuf,
    before: Option<Snapshot>,
    after: Vec<u8>,
}
fn commit(
    context: &Context,
    changes: &[Change],
    mut write: impl FnMut(&Path, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    for change in changes {
        match &change.before {
            Some(before) => before.check(&change.path, change.path != context.path)?,
            None if fs::symlink_metadata(&change.path).is_ok() => {
                return Err(
                    "configuration appeared during the key operation; no files were changed".into(),
                );
            }
            None => {}
        }
    }
    let backup_directory = context.directory.join("backups");
    if fs::symlink_metadata(&backup_directory).is_ok() {
        secure_directory(&backup_directory)?;
    }
    let paths = changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    let backup = control::backup(context, &paths, "keys")?;
    for (index, change) in changes.iter().enumerate() {
        if write(&change.path, &change.after).is_err() {
            let mut restored = true;
            // Include the failed write: rename may succeed before a directory fsync fails.
            for change in changes[..=index].iter().rev() {
                let result = match &change.before {
                    Some(before) => {
                        json::atomic_write(&change.path, &before.bytes).and_then(|()| {
                            fs::set_permissions(&change.path, before.permissions.clone())
                                .map_err(|e| e.to_string())
                        })
                    }
                    None => match fs::remove_file(&change.path) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(_) => Err("cannot remove newly written key configuration".into()),
                    },
                };
                restored &= result.is_ok();
            }
            return Err(if restored {
                "caller key update failed; previous configuration was restored".into()
            } else {
                format!(
                    "caller key update failed; recovery is required from {}",
                    backup.display()
                )
            });
        }
    }
    Ok(())
}

struct PrivateLock(File);
impl PrivateLock {
    fn take(path: &Path) -> Result<Self, String> {
        secure_directory(path.parent().ok_or("key lock parent is missing")?)?;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            secure_file(&metadata, true)?;
            if metadata.len() != 0 {
                return Err("caller key lock must be an empty private file".into());
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        no_follow(&mut options);
        private_options(&mut options);
        let file = options
            .open(path)
            .map_err(|_| "cannot open private caller key lock")?;
        #[cfg(windows)]
        crate::extensions::privacy_windows::protect(&file)?;
        let held = file
            .metadata()
            .map_err(|_| "cannot inspect caller key lock")?;
        secure_file(&held, true)?;
        file.try_lock()
            .map_err(|_| "another caller key operation is running; retry after it finishes")?;
        let named = fs::symlink_metadata(path).map_err(|_| "caller key lock changed")?;
        secure_file(&named, true)?;
        same_file(&named, &held)?;
        Ok(Self(file))
    }
}
impl Drop for PrivateLock {
    fn drop(&mut self) {
        // The stable lock inode is never unlinked.
        let _ = self.0.unlock();
    }
}

struct Temporary {
    path: PathBuf,
}
impl Temporary {
    fn create(directory: &Path, bytes: &[u8]) -> Result<Self, String> {
        for _ in 0..32 {
            let path = directory.join(format!(
                ".caller-keys-{}-{}.validate",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            private_options(&mut options);
            match options.open(&path) {
                Ok(file) => {
                    drop(file);
                    let temporary = Self { path };
                    json::atomic_write(&temporary.path, bytes)?;
                    return Ok(temporary);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err("cannot create private caller key validation file".into()),
            }
        }
        Err("cannot reserve a caller key validation file".into())
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn private_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.access_mode(0xC0040000); // GENERIC_READ | GENERIC_WRITE | WRITE_DAC
    }
}
fn no_follow(options: &mut OpenOptions) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000); // O_NOFOLLOW
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100); // O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000).share_mode(3); // OPEN_REPARSE_POINT; no DELETE sharing
    }
}
fn secure_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "cannot inspect key directory")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("key directory must be a real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != effective_uid() || metadata.mode() & 0o022 != 0 {
            return Err(
                "key directory must be owned by the current user and not writable by other users"
                    .into(),
            );
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("key directory must not be a reparse point".into());
        }
    }
    Ok(())
}
fn secure_file(metadata: &fs::Metadata, private: bool) -> Result<(), String> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("caller key configuration must be a regular file without links".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let denied = if private { 0o077 } else { 0o022 };
        if metadata.uid() != effective_uid()
            || metadata.nlink() != 1
            || metadata.mode() & denied != 0
        {
            return Err("caller key files and locks must be private, owned by the current user and have one link".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let _ = private;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("caller key configuration must not be a reparse point".into());
        }
    }
    Ok(())
}
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.dev() != right.dev() || left.ino() != right.ino() {
            return Err("caller key path changed while opening".into());
        }
    }
    #[cfg(not(unix))]
    let _ = (left, right); // Windows opens exclude DELETE sharing and reparse-point traversal.
    Ok(())
}
#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments and does not access Rust memory.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Fixture {
        context: Context,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zcalld-key-test-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            }
            #[cfg(not(unix))]
            fs::create_dir(&path).unwrap();
            let directory = fs::canonicalize(path).unwrap();
            let context = Context {
                path: directory.join("config.conf"),
                home: directory.clone(),
                directory,
            };
            json::atomic_write(
                &context.path,
                b"# retained comment\n[server]\nlisten = 127.0.0.1:19415\n[provider local]\nbase_url = http://127.0.0.1:9\nprotocol = responses\n[route demo]\nprovider = local\nmodel = demo\n",
            )
            .unwrap();
            Self { context }
        }
        fn action(&self, args: &[&str], value: Option<&str>) -> Result<Value, String> {
            execute(
                &self.context,
                &parse(
                    &args
                        .iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>(),
                )?,
                value.map(str::to_owned),
            )
        }
        fn configured(&self, keys: &[u8], lan: bool) {
            json::atomic_write(&self.context.directory.join("caller-keys.json"), keys).unwrap();
            let text = fs::read_to_string(&self.context.path).unwrap();
            let text =
                control::ini_set(&text, "server", "caller_keys_file", "caller-keys.json").unwrap();
            let text = if lan {
                control::ini_set(&text, "server", "listen", "0.0.0.0:19415").unwrap()
            } else {
                text
            };
            json::atomic_write(&self.context.path, text.as_bytes()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.context.directory);
        }
    }

    #[test]
    fn stdin_and_arguments_are_strict_without_secret_errors() {
        assert!(parse(&["list".into(), "--api-key-stdin".into()]).is_err());
        assert!(parse(&["add".into(), "id".into()]).is_err());
        assert!(
            parse(&[
                "add".into(),
                "id".into(),
                "--api-key-stdin".into(),
                "--api-key-stdin".into()
            ])
            .is_err()
        );
        for value in [
            b"private-canary\nsecond".as_slice(),
            b" private-canary",
            b"private-canary\n\n",
        ] {
            let error = secret(Cursor::new(value)).unwrap_err();
            assert!(!error.contains("private-canary"));
        }
        assert_eq!(secret(Cursor::new(b"valid\r\n")).unwrap(), "valid");
        assert_eq!(
            secret(Cursor::new(vec![b'x'; MAX_SECRET])).unwrap().len(),
            MAX_SECRET
        );
        assert!(secret(Cursor::new(vec![b'x'; MAX_SECRET + 1])).is_err());
    }

    #[test]
    fn add_rotate_revoke_preserve_anonymous_and_named_metadata() {
        let fixture = Fixture::new();
        fixture.configured(
            br#"[{"secret":"legacy-canary","name":"legacy","lan":true},{"id":"keep","name":"Keep me","secret":"keep-canary","lan":false}]"#,
            false,
        );
        let config_before = fs::read(&fixture.context.path).unwrap();
        let result = fixture
            .action(
                &[
                    "add",
                    "new",
                    "--api-key-stdin",
                    "--name",
                    "New key",
                    "--lan",
                ],
                Some("new-canary"),
            )
            .unwrap();
        assert_eq!(result.get("restart_required").as_bool(), Some(true));
        assert_eq!(result.get("applied").as_bool(), Some(false));
        assert!(!result.stringify().contains("new-canary"));
        fixture
            .action(
                &["rotate", "new", "--api-key-stdin"],
                Some("rotated-canary"),
            )
            .unwrap();
        let path = fixture.context.directory.join("caller-keys.json");
        let keys = CallerKeys::from_json(&fs::read(&path).unwrap()).unwrap();
        assert!(!keys.permits("new-canary", true));
        assert!(keys.permits("rotated-canary", false));
        let listing = fixture.action(&["list"], None).unwrap();
        assert_eq!(listing.get("keys").as_array().len(), 3);
        assert!(!listing.stringify().contains("canary"));
        fixture.action(&["revoke", "new"], None).unwrap();
        let keys = CallerKeys::from_json(&fs::read(path).unwrap()).unwrap();
        assert!(keys.permits("legacy-canary", false));
        assert!(keys.permits("keep-canary", true));
        let entries = keys.entries().collect::<Vec<_>>();
        assert_eq!(entries[0].id, None);
        assert_eq!(entries[0].name, Some("legacy"));
        assert_eq!(entries[1].name, Some("Keep me"));
        assert_eq!(fs::read(&fixture.context.path).unwrap(), config_before);
    }

    #[test]
    fn failed_mutations_preserve_existing_files_and_create_no_backup() {
        let fixture = Fixture::new();
        fixture.configured(
            br#"[{"id":"only","secret":"protected-canary","lan":true}]"#,
            true,
        );
        let config = fs::read(&fixture.context.path).unwrap();
        let path = fixture.context.directory.join("caller-keys.json");
        let original = fs::read(&path).unwrap();
        assert!(
            fixture
                .action(&["revoke", "only"], None)
                .unwrap_err()
                .contains("last LAN")
        );
        assert!(
            fixture
                .action(
                    &["add", "duplicate", "--api-key-stdin"],
                    Some("protected-canary")
                )
                .is_err()
        );
        assert!(
            fixture
                .action(
                    &["rotate", "missing", "--api-key-stdin"],
                    Some("new-canary")
                )
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), original);
        assert_eq!(fs::read(&fixture.context.path).unwrap(), config);
        assert!(!fixture.context.directory.join("backups").exists());
    }

    #[test]
    fn shared_token_allows_last_lan_key_revocation() {
        let fixture = Fixture::new();
        fixture.configured(br#"[{"id":"only","secret":"key-canary","lan":true}]"#, true);
        json::atomic_write(
            &fixture.context.directory.join("credentials"),
            b"ZCALLD_KEY_TEST_SHARED_TOKEN=\"shared-canary\"\n",
        )
        .unwrap();
        let text = fs::read_to_string(&fixture.context.path).unwrap();
        let text =
            control::ini_set(&text, "server", "token_env", "ZCALLD_KEY_TEST_SHARED_TOKEN").unwrap();
        json::atomic_write(&fixture.context.path, text.as_bytes()).unwrap();
        fixture.action(&["revoke", "only"], None).unwrap();
        assert!(
            Config::load(&fixture.context.path)
                .unwrap()
                .caller_keys
                .unwrap()
                .entries()
                .next()
                .is_none()
        );
    }

    #[test]
    fn first_add_preserves_orphan_file_and_binds_default_path() {
        let fixture = Fixture::new();
        let path = fixture.context.directory.join("caller-keys.json");
        json::atomic_write(&path, br#"[{"secret":"orphan-canary","lan":false}]"#).unwrap();
        assert_eq!(
            fixture
                .action(&["list"], None)
                .unwrap()
                .get("configured")
                .as_bool(),
            Some(false)
        );
        fixture
            .action(&["add", "new", "--api-key-stdin"], Some("new-canary"))
            .unwrap();
        let config = fs::read_to_string(&fixture.context.path).unwrap();
        assert_eq!(
            control::ini_get(&config, "server", "caller_keys_file").as_deref(),
            Some("caller-keys.json")
        );
        let keys = CallerKeys::from_json(&fs::read(path).unwrap()).unwrap();
        assert!(keys.permits("orphan-canary", true));
        assert!(keys.permits("new-canary", true));
        assert!(config.starts_with("# retained comment\n"));
    }

    #[test]
    fn concurrent_lock_rejects_operation_before_mutation() {
        let fixture = Fixture::new();
        let original = fs::read(&fixture.context.path).unwrap();
        let held = PrivateLock::take(&fixture.context.directory.join(".caller-keys.lock")).unwrap();
        let error = fixture
            .action(&["add", "new", "--api-key-stdin"], Some("new-canary"))
            .unwrap_err();
        assert!(error.contains("another caller key operation"));
        assert_eq!(fs::read(&fixture.context.path).unwrap(), original);
        assert!(!fixture.context.directory.join("caller-keys.json").exists());
        drop(held);
        fixture
            .action(&["add", "new", "--api-key-stdin"], Some("new-canary"))
            .unwrap();
    }

    #[test]
    fn rollback_includes_failed_post_rename_write_and_removes_new_file() {
        let fixture = Fixture::new();
        let original = Snapshot::read(&fixture.context.path, false)
            .unwrap()
            .unwrap();
        let bytes = original.bytes.clone();
        let new_path = fixture.context.directory.join("caller-keys.json");
        let changes = [
            Change {
                path: new_path.clone(),
                before: None,
                after: b"[]\n".to_vec(),
            },
            Change {
                path: fixture.context.path.clone(),
                before: Some(original),
                after: b"changed\n".to_vec(),
            },
        ];
        let error = commit(&fixture.context, &changes, |path, contents| {
            json::atomic_write(path, contents)?;
            if path == fixture.context.path {
                Err("simulated post-rename directory sync failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.contains("previous configuration was restored"));
        assert_eq!(fs::read(&fixture.context.path).unwrap(), bytes);
        assert!(!new_path.exists());
        assert!(fixture.context.directory.join("backups").is_dir());
    }

    #[test]
    fn invalid_configuration_is_rejected_before_default_file_is_bound() {
        let fixture = Fixture::new();
        let mut original = fs::read(&fixture.context.path).unwrap();
        original.extend_from_slice(b"unexpected_field = true\n");
        json::atomic_write(&fixture.context.path, &original).unwrap();
        assert!(
            fixture
                .action(&["add", "new", "--api-key-stdin"], Some("canary"))
                .is_err()
        );
        assert_eq!(fs::read(&fixture.context.path).unwrap(), original);
        assert!(!fixture.context.directory.join("caller-keys.json").exists());
        assert!(!fixture.context.directory.join("backups").exists());
        assert!(
            !fs::read_dir(&fixture.context.directory)
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".validate"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hardlink_and_public_secret_file_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let target = fixture.context.directory.join("target.json");
        json::atomic_write(&target, b"[]").unwrap();
        let path = fixture.context.directory.join("caller-keys.json");
        symlink(&target, &path).unwrap();
        assert!(
            fixture
                .action(&["add", "new", "--api-key-stdin"], Some("canary"))
                .is_err()
        );
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(
            fixture
                .action(&["add", "new", "--api-key-stdin"], Some("canary"))
                .is_err()
        );
        fs::remove_file(&path).unwrap();
        fs::rename(&target, &path).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(
            fixture
                .action(&["add", "new", "--api-key-stdin"], Some("canary"))
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), b"[]");
    }
}
