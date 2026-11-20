//! Optional extension management is a short-lived compatibility helper.
//! It is never launched for ordinary gateway requests without configured plugins.
use crate::{
    plugins::PluginSpec,
    state_json::{self, Value, object},
};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use zcalld_compat_client::helpers::{self, Helper};
pub fn root(config_path: &Path) -> Result<PathBuf, String> {
    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(parent)
    };
    Ok(parent.join("extensions"))
}
pub(crate) fn helper() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ZCALLD_PLUGINCTL") {
        let path = helpers::resolve_legacy(Path::new(&path));
        return if helpers::executable(&path) {
            Ok(path)
        } else {
            Err("ZCALLD_PLUGINCTL must name an absolute executable compatibility helper".into())
        };
    }
    let mut candidates = helpers::candidates(Helper::Pluginctl);
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("extensions/compat/bin")
            .join(Helper::Pluginctl.file_name()),
    );
    candidates
        .into_iter()
        .find(|path| helpers::executable(path))
        .ok_or_else(|| {
            "community management requires the optional zcalld-pluginctl compatibility helper"
                .into()
        })
}
pub fn run(args: &[String], config_path: &Path) -> Result<(), String> {
    let root = root(config_path)?;
    let args = if args.is_empty() {
        vec!["list".into()]
    } else {
        args.to_vec()
    };
    let binary = match helper() {
        Ok(p) => p,
        Err(e) if args[0] == "list" => {
            let registry = root.join("registry.json");
            let value = if registry.exists() {
                state_json::read(&registry)?
            } else {
                object([("plugins", Value::Array(Vec::new()))])
            };
            let plugins = value
                .get("plugins")
                .as_array()
                .iter()
                .map(|p| {
                    let mut v = object([]);
                    for key in [
                        "name", "path", "kind", "enabled", "version", "source", "revision",
                        "sha256",
                    ] {
                        v.set(key, p.get(key).clone());
                    }
                    v
                })
                .collect();
            println!(
                "{}",
                object([("plugins", Value::Array(plugins))]).stringify()
            );
            let _ = e;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let status = Command::new(binary)
        .arg("--root")
        .arg(root)
        .args(args)
        .status()
        .map_err(|_| "could not start compatibility helper")?;
    if status.success() {
        Ok(())
    } else {
        Err("plugin operation failed; see the helper diagnostic".into())
    }
}
fn library(kind: &str) -> Result<PathBuf, String> {
    let variable = format!("ZCALLD_{}_PLUGIN", kind.to_ascii_uppercase());
    if let Some(path) = std::env::var_os(variable) {
        let path = PathBuf::from(path);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err("configured extension library must be an absolute regular file".into());
    }
    let name = format!(
        "{}zcalld_{kind}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let mut candidates = vec![
        Path::new("/usr/lib/zcalld/plugins").join(&name),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/release")
            .join(&name),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/debug")
            .join(&name),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.insert(0, dir.join("plugins").join(&name));
        if let Some(prefix) = dir.parent() {
            candidates.insert(0, prefix.join("lib/zcalld/plugins").join(&name));
        }
    }
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!("optional {kind} native plugin is not installed"))
}
/// Explicit managed records augment INI plugin sections, in registry order.
/// Community packages share one native bridge; their JS code stays out of core.
pub fn managed_plugins(config_path: &Path) -> Result<Vec<PluginSpec>, String> {
    let root = root(config_path)?;
    let registry = root.join("registry.json");
    if !registry.exists() {
        return Ok(Vec::new());
    }
    let value = state_json::read(&registry)?;
    let mut specs = Vec::new();
    let mut community = false;
    for p in value.get("plugins").as_array() {
        if p.get("enabled").as_bool() != Some(true) {
            continue;
        }
        match p.get("kind").as_str() {
            Some("community") => community = true,
            Some("native") => {
                let path = PathBuf::from(
                    p.get("path")
                        .as_str()
                        .ok_or("native extension path missing")?,
                );
                if !path.is_absolute() || !path.is_file() {
                    return Err("native extension path unavailable".into());
                }
                let name = p
                    .get("name")
                    .as_str()
                    .ok_or("native extension name missing")?;
                let status = Command::new(helper()?)
                    .arg("--root")
                    .arg(&root)
                    .arg("verify")
                    .arg(name)
                    .arg("--json")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|_| "native extension checksum verifier unavailable")?;
                if !status.success() {
                    return Err("managed native extension checksum mismatch".into());
                }
                let config = if let Some(file) = p.get("config_file").as_str() {
                    let bytes = std::fs::read(file)
                        .map_err(|_| "native extension configuration unavailable")?;
                    if bytes.len() > 65536 {
                        return Err("native extension configuration exceeds limit".into());
                    }
                    String::from_utf8(bytes).map_err(|_| "native extension config is not UTF-8")?
                } else {
                    String::new()
                };
                specs.push(PluginSpec { path, config });
            }
            _ => return Err("unknown managed extension kind".into()),
        }
    }
    if community {
        let file = root.join("host.json");
        let config = state_json::read(&file)?.stringify();
        specs.push(PluginSpec {
            path: library("community")?,
            config,
        });
    }
    Ok(specs)
}
/// Privacy flags stay in their private settings file and are read per request.
pub fn privacy_plugin(config_path: &Path) -> Result<Option<PluginSpec>, String> {
    let config = root(config_path)?
        .parent()
        .ok_or("configuration parent missing")?
        .join("privacy/plugin.json");
    if !config.exists() {
        return Ok(None);
    }
    Ok(Some(PluginSpec {
        path: library("redact")?,
        config: resolved_privacy_binding(state_json::read(&config)?).stringify(),
    }))
}

/// Group routing runs after privacy, so an optional decision provider sees masked text.
pub fn routing_plugin(config_path: &Path) -> Result<Option<PluginSpec>, String> {
    let path = root(config_path)?
        .parent()
        .ok_or("configuration parent missing")?
        .join("routing.json");
    if !path.exists() {
        return Ok(None);
    }
    let value = state_json::read(&path)?;
    if value.get("groups").as_array().is_empty() {
        return Ok(None);
    }
    Ok(Some(PluginSpec {
        path: library("plugin_routing")?,
        config: path.to_string_lossy().into_owned(),
    }))
}

/// Resolve the settings actually bound to the plugin, including migrated paths.
pub(crate) fn privacy_settings_path(config_path: &Path) -> Result<PathBuf, String> {
    let base = root(config_path)?
        .parent()
        .ok_or("configuration parent missing")?
        .join("privacy");
    let binding = base.join("plugin.json");
    if binding.exists() {
        let value = state_json::read(&binding)?;
        let path = PathBuf::from(
            value
                .get("settings")
                .as_str()
                .ok_or("privacy binding has no settings path")?,
        );
        if !path.is_absolute() {
            return Err("privacy settings path must be absolute".into());
        }
        Ok(path)
    } else {
        Ok(base.join("settings.json"))
    }
}

fn privacy_worker() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ZCALLD_PRIVACY_WORKER") {
        let path = helpers::resolve_legacy(Path::new(&path));
        return if helpers::executable(&path) {
            Ok(path)
        } else {
            Err("ZCALLD_PRIVACY_WORKER must name an absolute executable privacy worker; settings were not enabled".into())
        };
    }
    let mut candidates = helpers::candidates(Helper::Privacy);
    if let Ok(helper) = helper()
        && let Some(dir) = helper.parent()
    {
        candidates.insert(0, dir.join(Helper::Privacy.file_name()));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("extensions/compat/bin")
            .join(Helper::Privacy.file_name()),
    );
    candidates.into_iter().find(|path| helpers::executable(path))
        .ok_or_else(|| "privacy requires the optional zcalld-privacy worker from zcalld-compat; settings were not enabled".into())
}

fn resolved_privacy_binding(mut value: Value) -> Value {
    if let Some(worker) = value.get("worker").as_str() {
        let resolved = helpers::resolve_legacy(Path::new(worker));
        value.set("worker", resolved.to_string_lossy().into_owned().into());
    }
    value
}

fn create_privacy_key(path: &Path) -> Result<(), String> {
    use std::io::Write;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() < 32 {
            return Err(
                "existing privacy key is invalid; restore it instead of generating a replacement"
                    .into(),
            );
        }
        return Ok(());
    }
    let mut bytes = [0u8; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(|_| "operating system random source is unavailable")?;
    }
    #[cfg(windows)]
    privacy_windows::random(&mut bytes)?;
    #[cfg(not(any(unix, windows)))]
    return Err("privacy key creation is unsupported on this platform".into());
    let parent = path.parent().ok_or("privacy key parent missing")?;
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(parent).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
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
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create private privacy key: {e}"))?;
    let write = (|| {
        #[cfg(windows)]
        privacy_windows::protect(&file)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })();
    bytes.fill(0);
    drop(file);
    if write.is_err() {
        let _ = std::fs::remove_file(path);
    }
    write
}

#[cfg(windows)]
mod privacy_windows {
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32)
        -> i32;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn GetTokenInformation(
            token: *mut c_void,
            class: u32,
            buffer: *mut c_void,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            descriptor: *mut c_void,
            present: *mut i32,
            acl: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn SetSecurityInfo(
            handle: *mut c_void,
            kind: u32,
            information: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    struct Token(*mut c_void);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    pub fn random(bytes: &mut [u8; 32]) -> Result<(), String> {
        // SAFETY: the system-preferred RNG writes exactly the live buffer length.
        if unsafe { BCryptGenRandom(ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) } == 0 {
            Ok(())
        } else {
            Err("Windows system random source is unavailable".into())
        }
    }
    pub fn protect(file: &std::fs::File) -> Result<(), String> {
        let error =
            || "cannot restrict the privacy key to this Windows account and SYSTEM".to_string();
        let mut token = ptr::null_mut();
        // SAFETY: handles and output buffers follow the synchronous Win32 contracts.
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), 8, &mut token) == 0 {
                return Err(error());
            }
            let token = Token(token);
            let mut size = 0;
            GetTokenInformation(token.0, 1, ptr::null_mut(), 0, &mut size);
            if size == 0 || size > 65536 {
                return Err(error());
            }
            let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetTokenInformation(token.0, 1, storage.as_mut_ptr().cast(), size, &mut size) == 0 {
                return Err(error());
            }
            let sid = *(storage.as_ptr().cast::<*mut c_void>());
            let mut raw = ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut raw) == 0 || raw.is_null() {
                return Err(error());
            }
            let sid_memory = Local(raw.cast());
            let mut length = 0;
            while length < 256 && *raw.add(length) != 0 {
                length += 1;
            }
            if length == 256 {
                return Err(error());
            }
            let sid =
                String::from_utf16(std::slice::from_raw_parts(raw, length)).map_err(|_| error())?;
            drop(sid_memory);
            let text = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
                .encode_utf16()
                .chain([0])
                .collect::<Vec<_>>();
            let mut descriptor = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            ) == 0
            {
                return Err(error());
            }
            let descriptor = Local(descriptor);
            let mut present = 0;
            let mut defaulted = 0;
            let mut acl = ptr::null_mut();
            if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) == 0
                || present == 0
                || acl.is_null()
            {
                return Err(error());
            }
            if SetSecurityInfo(
                file.as_raw_handle(),
                1,
                0x80000004,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null_mut(),
            ) != 0
            {
                return Err(error());
            }
        }
        Ok(())
    }
}

/// Preflight the optional worker/ABI against staged settings before publishing an
/// enabled binding. A missing original key is never silently replaced.
pub(crate) fn enable_privacy(config_path: &Path, settings: &Value) -> Result<bool, String> {
    static PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let base = root(config_path)?
        .parent()
        .ok_or("configuration parent missing")?
        .join("privacy");
    let binding_path = base.join("plugin.json");
    let settings_path = privacy_settings_path(config_path)?;
    let created = !binding_path.exists();
    let native = library("redact")?;
    let binding = if created {
        let worker = privacy_worker()?;
        let key = base.join("redact.key");
        create_privacy_key(&key)?;
        object([
            ("worker", worker.to_string_lossy().into_owned().into()),
            (
                "settings",
                settings_path.to_string_lossy().into_owned().into(),
            ),
            ("key", key.to_string_lossy().into_owned().into()),
            (
                "model_dir",
                base.join("models").to_string_lossy().into_owned().into(),
            ),
            (
                "history_dir",
                base.join("history").to_string_lossy().into_owned().into(),
            ),
        ])
    } else {
        resolved_privacy_binding(state_json::read(&binding_path)?)
    };
    for field in ["worker", "key"] {
        let path = binding
            .get(field)
            .as_str()
            .map(Path::new)
            .ok_or("privacy binding is incomplete")?;
        if !path.is_absolute() || !path.is_file() {
            return Err(format!(
                "privacy {field} is unavailable; existing binding and key were preserved"
            ));
        }
    }
    let probe = base.join(format!(
        ".settings-probe-{}-{}.json",
        std::process::id(),
        PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    state_json::atomic_write(&probe, settings.stringify().as_bytes())?;
    let mut preview = binding.clone();
    preview.set("settings", probe.to_string_lossy().into_owned().into());
    let preflight = (|| {
        let mut plugins = crate::plugins::Plugins::load(&[PluginSpec {
            path: native,
            config: preview.stringify(),
        }])?;
        plugins.request(
            zcalld_plugin_api::protocol::OPENAI_CHAT,
            br#"{"messages":[]}"#,
            4096,
        )?;
        Ok::<(), String>(())
    })();
    let _ = std::fs::remove_file(&probe);
    preflight
        .map_err(|e| format!("privacy runtime preflight failed; settings were not enabled: {e}"))?;
    let mut changes = vec![(settings_path, Some(settings.stringify().into_bytes()))];
    if created {
        changes.push((binding_path, Some(binding.stringify().into_bytes())));
    }
    let directory = base
        .parent()
        .ok_or("configuration parent missing")?
        .to_path_buf();
    let context = crate::control::Context {
        path: config_path.to_path_buf(),
        directory,
        home: crate::control::home()?,
    };
    crate::control::replace_files(&context, &changes, "privacy-enable")?;
    Ok(created)
}

pub(crate) fn privacy_configured(config_path: &Path) -> Result<bool, String> {
    let Ok(Some(spec)) = privacy_plugin(config_path) else {
        return Ok(false);
    };
    let binding = state_json::parse(spec.config.as_bytes())?;
    Ok(["worker", "settings", "key"].into_iter().all(|field| {
        binding.get(field).as_str().is_some_and(|p| {
            if field == "worker" {
                helpers::executable(Path::new(p))
            } else {
                Path::new(p).is_absolute() && Path::new(p).is_file()
            }
        })
    }))
}
