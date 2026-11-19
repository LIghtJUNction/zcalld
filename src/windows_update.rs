use crate::{
    control::{self, Context},
    state_json::{self as json, object},
};
use std::os::windows::process::CommandExt;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// An on-demand copy of the CLI waits for the old executable's Windows handles.
/// It never terminates a gateway or an account refresh to force an update.
pub fn queue(
    context: &Context,
    download: &Path,
    destination: &Path,
    expected: &str,
    version: &str,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or("update executable has no directory")?;
    let helper = parent.join(".zcalld-update-helper.exe");
    fs::copy(destination, &helper).map_err(|e| format!("cannot stage update helper: {e}"))?;
    let manifest = context.directory.join("pending-update.json");
    json::atomic_write(
        &manifest,
        object([
            ("download", download.to_string_lossy().into_owned().into()),
            (
                "destination",
                destination.to_string_lossy().into_owned().into(),
            ),
            ("sha256", expected.into()),
            ("version", version.into()),
            ("state", "waiting".into()),
        ])
        .stringify()
        .as_bytes(),
    )?;
    Command::new(&helper)
        .arg("__finish-update")
        .arg(&manifest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| format!("cannot start update helper: {e}"))?;
    control::emit(object([
        ("ok", true.into()), ("changed", false.into()), ("staged", true.into()),
        ("latest", version.into()), ("restart_required", true.into()),
        ("message", "Verified update queued. It will replace the executable when the gateway stops; running requests are not terminated.".into()),
    ]))
}

pub fn finish(path: &Path) -> Result<(), String> {
    let mut state = json::read(path)?;
    let download = std::path::PathBuf::from(
        state
            .get("download")
            .as_str()
            .ok_or("missing update download")?,
    );
    let destination = std::path::PathBuf::from(
        state
            .get("destination")
            .as_str()
            .ok_or("missing update destination")?,
    );
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if download.parent() != destination.parent()
        || exe.parent() != destination.parent()
        || download == destination
        || destination == exe
    {
        return Err("update staging paths do not match the installed directory".into());
    }
    let metadata = fs::symlink_metadata(&download).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > 50 * 1024 * 1024 {
        return Err("invalid staged update".into());
    }
    if Some(crate::digest::sha256(&fs::read(&download).map_err(|e| e.to_string())?).as_str())
        != state.get("sha256").as_str()
    {
        state.set("state", "checksum-failed".into());
        json::atomic_write(path, state.stringify().as_bytes())?;
        return Err("staged update checksum mismatch; installed binary was not changed".into());
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match fs::rename(&download, &destination) {
            Ok(()) => {
                state.set("state", "applied".into());
                json::atomic_write(path, state.stringify().as_bytes())?;
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Err(_) => {
                state.set("state", "waiting-for-gateway-stop".into());
                json::atomic_write(path, state.stringify().as_bytes())?;
                return Err(
                    "gateway executable remains in use; verified download was retained".into(),
                );
            }
        }
    }
}
