use super::Capability;
use crate::logic::Notice;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// A native identity-bearing helper is optional on systems whose notification API
// requires an installed application identity. No shell, interpreter or browser.
pub fn send(path: Option<&Path>, notice: &Notice) -> Capability {
    let Some(path) = path else {
        return Capability::NoIdentity;
    };
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Capability::Unavailable;
    };
    if !path.is_absolute() || !meta.is_file() || meta.file_type().is_symlink() {
        return Capability::Unavailable;
    }
    let bytes = notice.json().stringify().into_bytes();
    if bytes.len() > 2048 {
        return Capability::Failed;
    }
    let mut command = Command::new(path);
    command
        .arg("--notify-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear();
    // Only OS identity/session essentials, never API/provider environment values.
    for key in [
        "SystemRoot",
        "WINDIR",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "HOME",
        "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    let Ok(mut child) = command.spawn() else {
        return Capability::Unavailable;
    };
    let deadline = Instant::now() + Duration::from_millis(750);
    if child
        .stdin
        .take()
        .is_none_or(|mut input| input.write_all(&bytes).is_err())
    {
        let _ = child.kill();
        let _ = child.wait();
        return Capability::Failed;
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Capability::Failed;
            }
        }
    };
    if !status.success() {
        return match status.code() {
            Some(3) => Capability::Denied,
            Some(4) => Capability::NoIdentity,
            _ => Capability::Failed,
        };
    }
    let mut out = Vec::new();
    if child
        .stdout
        .take()
        .is_none_or(|output| output.take(1025).read_to_end(&mut out).is_err())
        || out.len() > 1024
    {
        return Capability::Failed;
    }
    if out == b"accepted\n" {
        Capability::Available
    } else {
        Capability::Failed
    }
}
