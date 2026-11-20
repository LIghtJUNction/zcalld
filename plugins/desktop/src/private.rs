use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
};
use zcalld_compat_client::json::{self, Value};

static NEXT: AtomicU64 = AtomicU64::new(1);
#[cfg(windows)]
#[path = "../../../src/privacy_windows.rs"]
mod windows_private;

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
fn safe_ancestors(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("private path must be absolute".into());
    }
    let mut part = std::path::PathBuf::new();
    for c in path.components() {
        if matches!(c, Component::ParentDir) {
            return Err("parent components are not allowed".into());
        }
        part.push(c.as_os_str());
        match fs::symlink_metadata(&part) {
            Ok(m) if is_link(&m) => return Err("private path contains a symlink".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect private path".into()),
        }
    }
    Ok(())
}
fn regular(file: &File) -> Result<(), String> {
    let m = file.metadata().map_err(|_| "cannot inspect private file")?;
    if !m.is_file() {
        return Err("private payload is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.mode() & 0o077 != 0 || m.nlink() != 1 || m.uid() != unsafe { geteuid() } {
            return Err("private file permissions are unsafe".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Err("private file is a reparse point".into());
        }
    }
    Ok(())
}
fn options(read: bool, write: bool) -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(read).write(write);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(0x20000);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(0x100);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(0x00200000);
        if write {
            o.access_mode(0x40000000 | 0x80000000 | 0x00040000);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}
fn protect(file: &File) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows_private::protect(file)?;
    }
    #[cfg(not(windows))]
    {
        let _ = file;
    }
    Ok(())
}
pub fn directory(path: &Path) -> Result<(), String> {
    safe_ancestors(path)?;
    if !path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut b = fs::DirBuilder::new();
            b.recursive(true).mode(0o700);
            b.create(path)
                .map_err(|_| "cannot create private directory")?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(path).map_err(|_| "cannot create private directory")?;
    }
    let m = fs::symlink_metadata(path).map_err(|_| "cannot inspect private directory")?;
    if !m.is_dir() || is_link(&m) {
        return Err("private directory is not a directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.mode() & 0o077 != 0 || m.uid() != unsafe { geteuid() } {
            return Err("private directory permissions are unsafe".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let f = OpenOptions::new()
            .access_mode(0x80000000 | 0x00040000)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)
            .map_err(|_| "cannot protect private directory")?;
        protect(&f)?;
    }
    Ok(())
}
pub fn read(path: &Path, maximum: usize) -> Result<Vec<u8>, String> {
    safe_ancestors(path)?;
    let f = options(true, false)
        .open(path)
        .map_err(|_| "cannot open private file")?;
    regular(&f)?;
    if f.metadata()
        .map_err(|_| "cannot inspect private file")?
        .len()
        > maximum as u64
    {
        return Err("private file exceeds limit".into());
    }
    let mut bytes = Vec::new();
    f.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read private file")?;
    if bytes.len() > maximum {
        return Err("private file exceeds limit".into());
    }
    Ok(bytes)
}
pub fn read_config(path: &Path) -> Result<Vec<u8>, String> {
    read(path, 16_384)
}
pub fn lock(dir: &Path) -> Result<File, String> {
    directory(dir)?;
    let p = dir.join(".desktop.lock");
    safe_ancestors(&p)?;
    let f = options(true, true)
        .create(true)
        .truncate(false)
        .open(p)
        .map_err(|_| "cannot open desktop instance lock")?;
    protect(&f)?;
    regular(&f)?;
    f.try_lock()
        .map_err(|_| "desktop instance is already active")?;
    Ok(f)
}
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("desktop state exceeds limit".into());
    }
    safe_ancestors(path)?;
    let parent = path.parent().ok_or("invalid desktop state path")?;
    directory(parent)?;
    if path.exists() {
        let _ = read(path, 2 * 1024 * 1024)?;
    }
    #[cfg(windows)]
    let nonce = {
        let mut bytes = [0u8; 32];
        windows_private::random(&mut bytes)?;
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    };
    #[cfg(not(windows))]
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".desktop-{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut f = options(false, true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "cannot create private state temporary file")?;
        protect(&f)?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|_| "cannot write private desktop state")?;
        fs::rename(&temp, path).map_err(|_| "cannot commit private desktop state")?;
        // Rename is the commit boundary. A later directory-sync error must not
        // replay a committed mark in memory; notification durability is best effort.
        #[cfg(unix)]
        let _ = File::open(parent).and_then(|f| f.sync_all());
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn load_ledger(dir: &Path) -> Result<Option<Value>, String> {
    let p = dir.join("ledger.json");
    if !p.exists() {
        return Ok(None);
    }
    json::parse(&read(&p, 2 * 1024 * 1024)?)
        .map(Some)
        .map_err(|_| "invalid desktop ledger JSON".into())
}
pub type InboxEntry = (std::path::PathBuf, Result<Vec<u8>, String>);
pub fn inbox(dir: &Path) -> Result<Vec<InboxEntry>, String> {
    // Claim renames share the producer lock. Otherwise a publisher could list an
    // old name, miss it after rename, and undercount the still-pending claim.
    let path = dir.join(".publish.lock");
    safe_ancestors(&path)?;
    let lock = options(true, true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| "cannot open inbox publication lock")?;
    protect(&lock)?;
    regular(&lock)?;
    if lock
        .metadata()
        .map_err(|_| "cannot inspect inbox lock")?
        .len()
        != 0
    {
        return Err("invalid inbox publication lock".into());
    }
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Vec::new()),
        Err(_) => return Err("cannot lock inbox publication".into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let disk =
            fs::symlink_metadata(&path).map_err(|_| "cannot inspect inbox publication lock")?;
        let held = lock
            .metadata()
            .map_err(|_| "cannot inspect inbox publication lock")?;
        if disk.dev() != held.dev() || disk.ino() != held.ino() {
            return Err("inbox publication lock changed".into());
        }
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)
        .map_err(|_| "cannot read desktop inbox")?
        .take(256)
    {
        let entry = entry.map_err(|_| "cannot inspect desktop inbox")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() > 160
            || !name.ends_with(".json")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            continue;
        }
        if name.starts_with('.') && !name.starts_with(".claim-") {
            continue;
        }
        let claimed = if name.starts_with(".claim-") {
            entry.path()
        } else {
            let p = dir.join(format!(
                ".claim-{}-{}.json",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            if fs::rename(entry.path(), &p).is_err() {
                continue;
            }
            p
        };
        let bytes = read(&claimed, 16_384);
        files.push((claimed, bytes));
        if files.len() == 32 {
            break;
        }
    }
    Ok(files)
}
pub fn record(dir: &Path, value: &Value) -> Result<(), String> {
    let p = dir.join("recorded.jsonl");
    if p.exists()
        && fs::symlink_metadata(&p)
            .map_err(|_| "cannot inspect recorded notifications")?
            .len()
            >= 1024 * 1024
    {
        let old = dir.join("recorded.previous.jsonl");
        if old.exists() {
            let _ = read(&old, 2 * 1024 * 1024)?;
            fs::remove_file(&old).map_err(|_| "cannot rotate recorded notifications")?;
        }
        fs::rename(&p, &old).map_err(|_| "cannot rotate recorded notifications")?;
    }
    safe_ancestors(&p)?;
    let mut f = options(false, true)
        .create(true)
        .append(true)
        .open(p)
        .map_err(|_| "cannot record desktop event")?;
    protect(&f)?;
    regular(&f)?;
    f.seek(SeekFrom::End(0))
        .map_err(|_| "cannot append recorded event")?;
    let mut data = value.stringify().into_bytes();
    data.push(b'\n');
    f.write_all(&data)
        .map_err(|_| "cannot record desktop event".into())
}
