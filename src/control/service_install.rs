//! Explicit Linux user-unit setup for a verified private native installation.
use super::{Context, Value};

pub(super) fn information(context: &Context) -> (bool, Value) {
    #[cfg(target_os = "linux")]
    {
        linux::information(context)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = context;
        (false, Value::Null)
    }
}

pub(super) fn install(context: &Context) -> Result<Value, String> {
    #[cfg(target_os = "linux")]
    {
        linux::install(context)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = context;
        Err("service install is available only for a private Linux installation".into())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Context, Value};
    use crate::{
        config,
        control::{command, object},
    };
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
        path::{Component, Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    const UNIT: &str = "zcalld.service";
    const MAX_UNIT: u64 = 16 * 1024;
    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct LoadedUnit {
        state: String,
        fragment: String,
        drop_ins: String,
    }

    fn manager_unit() -> Result<LoadedUnit, String> {
        let (_, output, _) = command(
            "systemctl",
            &[
                "--user",
                "show",
                "--no-pager",
                "--property=LoadState",
                "--property=FragmentPath",
                "--property=DropInPaths",
                UNIT,
            ]
            .map(str::to_owned),
            None,
            Duration::from_secs(3),
        )?;
        if output.len() > 64 * 1024 {
            return Err("service manager returned excessive unit metadata".into());
        }
        let mut state = None;
        let mut fragment = None;
        let mut drop_ins = None;
        for line in output.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let slot = match key {
                "LoadState" => &mut state,
                "FragmentPath" => &mut fragment,
                "DropInPaths" => &mut drop_ins,
                _ => continue,
            };
            if slot.replace(value.to_owned()).is_some() {
                return Err("service manager returned ambiguous unit metadata".into());
            }
        }
        let state = state.ok_or("cannot determine whether a user service already exists")?;
        if !matches!(
            state.as_str(),
            "loaded" | "not-found" | "bad-setting" | "error" | "masked"
        ) {
            return Err("service manager returned an unknown unit state".into());
        }
        Ok(LoadedUnit {
            state,
            fragment: fragment.ok_or("service manager omitted the unit path")?,
            drop_ins: drop_ins.ok_or("service manager omitted unit overrides")?,
        })
    }

    fn config_directory(context: &Context) -> Result<PathBuf, String> {
        let directory = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| context.home.join(".config"));
        if !directory.is_absolute()
            || directory.to_str().is_none()
            || directory
                .components()
                .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
            || ["/etc", "/usr", "/run", "/bin", "/sbin", "/lib", "/lib64"]
                .iter()
                .any(|p| directory.starts_with(p))
        {
            return Err("user service configuration needs an absolute user-owned directory".into());
        }
        Ok(directory)
    }

    fn present(path: &Path) -> bool {
        !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    }

    pub(super) fn information(context: &Context) -> (bool, Value) {
        let supported = std::env::current_exe().ok().is_some_and(|exe| {
            !zcalld_installer::managed_path(&exe)
                && zcalld_installer::installed_prefix(&exe).is_ok_and(|prefix| prefix.is_some())
        });
        let installed = if config_directory(context)
            .is_ok_and(|path| present(&path.join("systemd/user").join(UNIT)))
        {
            Some(true)
        } else {
            manager_unit().ok().map(|unit| unit.state != "not-found")
        };
        (supported, installed.map(Value::from).unwrap_or(Value::Null))
    }

    // These are systemd command arguments, not shell words. ExecStart's colon
    // prefix disables environment substitution; percent specifiers and quotes
    // still need their own escaping.
    fn argument(path: &Path) -> Result<String, String> {
        let value = path.to_str().ok_or("service paths must be UTF-8")?;
        if !path.is_absolute() || value.chars().any(char::is_control) {
            return Err("service paths must be absolute and contain no control characters".into());
        }
        let mut escaped = String::from("\"");
        for character in value.chars() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                '%' => escaped.push_str("%%"),
                other => escaped.push(other),
            }
        }
        escaped.push('"');
        Ok(escaped)
    }

    fn unit_text(executable: &Path, configuration: &Path) -> Result<String, String> {
        if executable
            .to_str()
            .is_none_or(|path| path.contains(['\\', '"', '\'']))
        {
            return Err(
                "systemd cannot use an executable path containing quotes or backslashes".into(),
            );
        }
        Ok(format!(
            "# Created by zcalld service install; exact executable/configuration ownership.\n\
[Unit]\nDescription=zcalld local AI gateway\nAfter=network.target\n\n\
[Service]\nType=notify\nNotifyAccess=main\nExecStart=:{} serve --config {}\n\
Restart=on-failure\nRestartSec=2s\nTimeoutStopSec=60s\nKillMode=mixed\n\
RuntimeDirectory=zcalld\nRuntimeDirectoryMode=0700\nUMask=0077\nNoNewPrivileges=yes\n\n\
[Install]\nWantedBy=default.target\n",
            argument(executable)?,
            argument(configuration)?
        ))
    }

    fn no_symlinks(path: &Path) -> Result<(), String> {
        let mut prefix = PathBuf::new();
        for component in path.components() {
            if matches!(component, Component::ParentDir | Component::CurDir) {
                return Err("service directory contains a noncanonical component".into());
            }
            prefix.push(component.as_os_str());
            match fs::symlink_metadata(&prefix) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => return Err("service directory contains a symlink or non-directory".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect the user service directory".into()),
            }
        }
        Ok(())
    }

    fn ensure_directory(path: &Path, uid: u32) -> Result<(), String> {
        no_symlinks(path)?;
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0 => Ok(()),
            Ok(_) => Err("service directory is not owned and writable only by this user".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or("service directory has no parent")?;
                ensure_directory(parent, uid)?;
                match fs::DirBuilder::new().mode(0o700).create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err("cannot create the user service directory".into()),
                }
                ensure_directory(path, uid)
            }
            Err(_) => Err("cannot inspect the user service directory".into()),
        }
    }

    fn exact_owned_unit(path: &Path, uid: u32, expected: &[u8]) -> Result<bool, String> {
        let meta = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("cannot inspect the existing user service".into()),
            Ok(meta) => meta,
        };
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || meta.uid() != uid
            || meta.mode() & 0o022 != 0
            || meta.nlink() != 1
            || meta.len() > MAX_UNIT
        {
            return Err("an existing user service is not owned by this installation".into());
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(0x20000)
            .open(path)
            .map_err(|_| "cannot safely read the existing user service")?;
        let held = file
            .metadata()
            .map_err(|_| "cannot inspect the existing user service")?;
        if held.dev() != meta.dev() || held.ino() != meta.ino() {
            return Err("the existing service changed while being inspected".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_UNIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read the existing user service")?;
        if bytes != expected {
            return Err(
                "an existing user service belongs to another installation or configuration".into(),
            );
        }
        Ok(true)
    }

    fn search_paths(target: &Path) -> Result<Vec<PathBuf>, String> {
        let (code, output, _) = command(
            "systemd-analyze",
            &["--user".into(), "unit-paths".into()],
            None,
            Duration::from_secs(3),
        )?;
        if code != 0 || output.len() > 64 * 1024 {
            return Err("cannot determine the user service search paths".into());
        }
        let paths: Vec<_> = output
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
        if paths.is_empty()
            || paths.len() > 128
            || paths.iter().any(|path| !path.is_absolute())
            || !paths.iter().any(|path| path == target)
        {
            return Err(
                "the requested user service directory is not in the service search path".into(),
            );
        }
        Ok(paths)
    }

    fn check_existing(
        paths: &[PathBuf],
        target: &Path,
        uid: u32,
        bytes: &[u8],
        loaded: &LoadedUnit,
    ) -> Result<bool, String> {
        let ours = exact_owned_unit(target, uid, bytes)?;
        for directory in paths {
            if present(&directory.join(format!("{UNIT}.d")))
                || present(&directory.join("service.d"))
            {
                return Err("existing service overrides must be reviewed manually".into());
            }
            let candidate = directory.join(UNIT);
            if candidate != target && present(&candidate) {
                return Err(
                    "an existing service from another installation will not be shadowed".into(),
                );
            }
        }
        if !loaded.drop_ins.is_empty() {
            return Err("existing service overrides must be reviewed manually".into());
        }
        match loaded.state.as_str() {
            "not-found" if loaded.fragment.is_empty() => Ok(ours),
            "loaded" if ours && Path::new(&loaded.fragment) == target => Ok(true),
            _ => Err("an existing loaded or masked service will not be replaced".into()),
        }
    }

    fn publish(directory: &Path, bytes: &[u8], uid: u32) -> Result<(), String> {
        no_symlinks(directory)?;
        let dir = File::open(directory).map_err(|_| "cannot open the user service directory")?;
        let held = dir
            .metadata()
            .map_err(|_| "cannot inspect the user service directory")?;
        let current =
            fs::symlink_metadata(directory).map_err(|_| "user service directory disappeared")?;
        if !held.is_dir()
            || held.uid() != uid
            || held.mode() & 0o022 != 0
            || current.file_type().is_symlink()
            || current.dev() != held.dev()
            || current.ino() != held.ino()
        {
            return Err("user service directory changed during setup".into());
        }
        // Anchor both names to the held directory; a concurrent ancestor rename
        // cannot redirect publication into a different directory.
        let anchor = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        let temp = anchor.join(format!(
            ".zcalld-unit-{}-{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| "cannot prepare a private user service file")?;
        let result = (|| {
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "cannot persist the user service file")?;
            fs::hard_link(&temp, anchor.join(UNIT))
                .map_err(|_| "a user service already exists; no file was overwritten")?;
            fs::remove_file(&temp).map_err(|_| "cannot finish user service publication")?;
            dir.sync_all()
                .map_err(|_| "cannot sync the user service directory")?;
            let now = fs::symlink_metadata(directory)
                .map_err(|_| "user service directory moved during setup")?;
            if now.dev() != held.dev() || now.ino() != held.ino() || now.file_type().is_symlink() {
                return Err("user service directory moved during setup".into());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    pub(super) fn install(context: &Context) -> Result<Value, String> {
        let executable =
            std::env::current_exe().map_err(|_| "cannot locate the running executable")?;
        if zcalld_installer::managed_path(&executable) {
            return Err("package-managed installations use their packaged service unit".into());
        }
        let verified = zcalld_installer::verify_installation(&executable)?
            .ok_or("service install requires a verified private native installation")?;
        let uid = fs::metadata(verified.prefix())
            .map_err(|_| "cannot inspect the verified installation")?
            .uid();
        let configuration = std::path::absolute(&context.path)
            .map_err(|_| "cannot resolve the configuration path")?;
        config::Config::load(&configuration)
            .map_err(|_| "create and validate the configuration before installing its service")?;
        let bytes = unit_text(&executable, &configuration)?.into_bytes();
        let directory = config_directory(context)?.join("systemd/user");
        no_symlinks(&directory)?;
        let target = directory.join(UNIT);
        let paths = search_paths(&directory)?;
        let loaded = manager_unit()?;
        let mut created = false;
        if !check_existing(&paths, &target, uid, &bytes, &loaded)? {
            ensure_directory(&directory, uid)?;
            publish(&directory, &bytes, uid)?;
            created = true;
        }
        // The verified-installation guard remains live through publication.
        drop(verified);
        Ok(object([
            ("ok", true.into()),
            ("created", created.into()),
            ("unit_path", target.to_string_lossy().into_owned().into()),
            ("service_started", false.into()),
        ]))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn systemd_arguments_preserve_metacharacters_and_reject_controls() {
            assert_eq!(
                argument(Path::new("/home/space $x/%h/\"quote\"/back\\slash")).unwrap(),
                "\"/home/space $x/%%h/\\\"quote\\\"/back\\\\slash\""
            );
            assert!(argument(Path::new("relative")).is_err());
            assert!(argument(Path::new("/tmp/new\nline")).is_err());
            let text = unit_text(
                Path::new("/home/native/bin/zcalld"),
                Path::new("/home/settings/config.conf"),
            )
            .unwrap();
            assert!(text.contains("Type=notify\n"));
            assert!(text.contains("ExecStart=:\"/home/native/bin/zcalld\" serve --config \"/home/settings/config.conf\"\n"));
            assert!(
                unit_text(
                    Path::new("/home/\"quoted\"/zcalld"),
                    Path::new("/tmp/config")
                )
                .is_err()
            );
            assert!(!text.contains("ExecStartPre"));
        }
    }
}
