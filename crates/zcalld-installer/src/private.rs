use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

#[cfg(windows)]
#[path = "../../../src/privacy_windows.rs"]
mod privacy_windows;

pub fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    #[cfg(unix)]
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "system random source is unavailable")?;
    #[cfg(windows)]
    privacy_windows::random(&mut bytes)?;
    Ok(bytes[..16].iter().map(|b| format!("{b:02x}")).collect())
}
pub fn no_links(path: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err("noncanonical installer path".into());
        }
        prefix.push(component.as_os_str());
        // A Windows drive prefix alone is not its root, especially for the
        // extended paths returned by canonicalize. Inspect the complete root
        // on the next iteration; drive-relative inputs keep their old checks.
        if matches!(component, Component::Prefix(_))
            && matches!(components.peek(), Some(Component::RootDir))
        {
            continue;
        }
        match fs::symlink_metadata(&prefix) {
            Ok(meta) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err("reparse point in installer path".into());
                    }
                }
                if meta.file_type().is_symlink() {
                    return Err("symlink in installer path".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect installer path".into()),
        }
    }
    Ok(())
}
pub fn regular(path: &Path, limit: u64) -> Result<fs::Metadata, String> {
    no_links(path)?;
    let m = fs::symlink_metadata(path).map_err(|_| "installation file is missing")?;
    if !m.is_file() || m.len() > limit {
        return Err("invalid installation file type or size".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.nlink() != 1 {
            return Err("hard-linked installation file".into());
        }
    }
    #[cfg(windows)]
    {
        let file = File::open(path).map_err(|_| "cannot inspect Windows installation file")?;
        windows_regular(&file)?;
    }
    Ok(m)
}
#[cfg(windows)]
fn windows_regular(file: &File) -> Result<(), String> {
    windows_regular_links(file, false)
}
#[cfg(windows)]
fn windows_regular_links(file: &File, allow_unlinked: bool) -> Result<(), String> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut c_void, information: *mut u32) -> i32;
    }
    let mut info = [0u32; 13];
    // SAFETY: BY_HANDLE_FILE_INFORMATION is thirteen DWORDs, including its
    // FILETIME pairs. The handle stays live for this synchronous query.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0
        || info[0] & (0x10 | 0x400) != 0
        || (info[10] != 1 && !(allow_unlinked && info[10] == 0))
    {
        return Err("installation files must be regular and have exactly one link".into());
    }
    Ok(())
}
#[cfg(windows)]
pub fn check_file_private(path: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|_| "cannot inspect private installed file")?;
    windows_regular(&file)?;
    privacy_windows::check_private(&file)
}
#[cfg(unix)]
fn own(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments or external memory access.
    meta.uid() == unsafe { geteuid() } && (meta.mode() & 0o077) == 0
}
pub fn check_dir(path: &Path) -> Result<(), String> {
    no_links(path)?;
    let meta =
        fs::symlink_metadata(path).map_err(|_| "private installation directory is missing")?;
    if !meta.is_dir() {
        return Err("installer requires a private directory".into());
    }
    #[cfg(unix)]
    if !own(&meta) {
        return Err("installer directory is not private to this user".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .read(true)
            .access_mode(0x00020000)
            .custom_flags(0x02000000)
            .open(path)
            .map_err(|_| "cannot open private installation directory")?;
        privacy_windows::check_private(&file)?;
    }
    Ok(())
}
pub fn mkdir(path: &Path) -> Result<(), String> {
    no_links(path)?;
    if path.exists() {
        return check_dir(path);
    }
    let parent = path.parent().ok_or("installation path has no parent")?;
    if !parent.is_dir() {
        return Err("installation parent directory is missing".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| "cannot create private installation directory")?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        fs::create_dir(path).map_err(|_| "cannot create private installation directory")?;
        let file = OpenOptions::new()
            .read(true)
            .access_mode(0x00020000 | 0x00040000 | 0x00080000)
            .custom_flags(0x02000000)
            .open(path)
            .map_err(|_| "cannot protect new installation directory")?;
        privacy_windows::protect_created(&file)?;
    }
    check_dir(path)
}
pub fn parents_private(path: &Path) -> Result<(), String> {
    if path.exists() {
        no_links(path)?;
        return Ok(());
    }
    let parent = path.parent().ok_or("installation path has no parent")?;
    parents_private(parent)?;
    mkdir(path)
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "macos")]
        const NOFOLLOW: i32 = 0x100;
        #[cfg(not(target_os = "macos"))]
        const NOFOLLOW: i32 = 0x20000;
        options.mode(0o600).custom_flags(NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .access_mode(0x80000000 | 0x40000000 | 0x00040000 | 0x00080000)
            .share_mode(0x1 | 0x2);
    }
    options
}
pub fn create(path: &Path) -> Result<File, String> {
    no_links(path)?;
    let file = options()
        .create_new(true)
        .open(path)
        .map_err(|_| "cannot exclusively create installation file")?;
    #[cfg(windows)]
    privacy_windows::protect_created(&file)?;
    Ok(file)
}
pub fn lock_file(path: &Path) -> Result<File, String> {
    no_links(path)?;
    let file = match create(path) {
        Ok(file) => file,
        Err(_) if path.exists() => options()
            .open(path)
            .map_err(|_| "cannot open installer lock")?,
        Err(error) => return Err(error),
    };
    let meta = file
        .metadata()
        .map_err(|_| "cannot inspect installer lock")?;
    if !meta.is_file() || meta.len() != 0 {
        return Err("invalid installer lock".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !own(&meta) || meta.nlink() != 1 {
            return Err("installer lock is not private".into());
        }
        let path_meta =
            fs::symlink_metadata(path).map_err(|_| "installer lock path disappeared")?;
        if meta.dev() != path_meta.dev() || meta.ino() != path_meta.ino() {
            return Err("installer lock identity changed".into());
        }
    }
    #[cfg(windows)]
    privacy_windows::check_private(&file)?;
    Ok(file)
}
pub fn same_file(file: &File, path: &Path) -> Result<(), String> {
    no_links(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let held = file
            .metadata()
            .map_err(|_| "cannot inspect installer lock")?;
        let current = fs::symlink_metadata(path).map_err(|_| "installer lock disappeared")?;
        if held.dev() != current.dev() || held.ino() != current.ino() || held.nlink() != 1 {
            return Err("installer lock identity changed".into());
        }
    }
    // Windows lock handles intentionally omit FILE_SHARE_DELETE, so replacement
    // is denied while this handle exists. Check the ACL without mutating it.
    #[cfg(windows)]
    privacy_windows::check_private(file)?;
    Ok(())
}
pub fn read(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    read_handle(read_open(path)?, limit)
}
fn read_open(path: &Path) -> Result<File, String> {
    no_links(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "macos")]
        const NOFOLLOW: i32 = 0x100;
        #[cfg(not(target_os = "macos"))]
        const NOFOLLOW: i32 = 0x20000;
        #[cfg(target_os = "macos")]
        const NONBLOCK: i32 = 0x4;
        #[cfg(not(target_os = "macos"))]
        const NONBLOCK: i32 = 0x800;
        // A swapped FIFO must not block before the held-file type check.
        options.custom_flags(NOFOLLOW | NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Inspect a reparse point itself, never its target. Unlike lock files,
        // state readers allow atomic replacement while this snapshot is open.
        options.custom_flags(0x00200000).share_mode(0x1 | 0x2 | 0x4);
    }
    options
        .open(path)
        .map_err(|_| "cannot read installation state".into())
}
fn read_handle(file: File, limit: usize) -> Result<Vec<u8>, String> {
    let meta = file
        .metadata()
        .map_err(|_| "cannot inspect installation state".to_owned())?;
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err("invalid installation file type or size".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // atomic() may have replaced the name after open. That private old
        // snapshot remains readable with zero links; extra aliases never do.
        if meta.nlink() > 1 {
            return Err("hard-linked installation file".into());
        }
        if !own(&meta) {
            return Err("installation state is not private to this user".into());
        }
    }
    #[cfg(windows)]
    {
        windows_regular_links(&file, true)?;
        privacy_windows::check_private(&file)?;
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read installation state")?;
    if bytes.len() > limit {
        return Err("installation state exceeds its limit".into());
    }
    Ok(bytes)
}

pub fn atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    no_links(path)?;
    let parent = path.parent().ok_or("installation state has no parent")?;
    check_dir(parent)?;
    let temp = parent.join(format!(".pending-{}", random_id()?));
    let result = (|| {
        let mut f = create(&temp)?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|_| "cannot persist installation state")?;
        drop(f);
        fs::rename(&temp, path).map_err(|_| "cannot commit installation state")?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn sync_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|_| "cannot sync installation directory")?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
pub fn mode(file: &File, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|_| "cannot set installed file permissions")?;
    }
    #[cfg(windows)]
    {
        let _ = mode;
        privacy_windows::protect(file)?;
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    struct TempDirectory(PathBuf);
    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn canonical_windows_root_and_private_child_are_inspected_as_complete_paths() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        assert!(matches!(
            parent.components().next(),
            Some(Component::Prefix(prefix)) if prefix.kind().is_verbatim()
        ));
        no_links(&parent).expect("canonical Windows temp ancestors must remain inspectable");
        let root =
            TempDirectory(parent.join(format!("zcalld-private-test-{}", random_id().unwrap())));
        mkdir(&root.0).unwrap();
        check_dir(&root.0).unwrap();
        let path = root.0.join("private-state");
        let mut file = create(&path).unwrap();
        file.write_all(b"fixture").unwrap();
        drop(file);
        assert_eq!(read(&path, 32).unwrap(), b"fixture");
        let mut noncanonical = root.0.as_os_str().to_os_string();
        noncanonical.push(r"\..\outside");
        assert!(no_links(Path::new(&noncanonical)).is_err());
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "zcalld-installer-snapshot-{}",
                random_id().unwrap()
            ));
            mkdir(&root).unwrap();
            Self(root)
        }
        fn state(&self) -> PathBuf {
            self.0.join("state.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_replacement_keeps_the_open_private_snapshot_readable() {
        let fixture = Fixture::new();
        let path = fixture.state();
        let old = br#"{"phase":"pending_exit"}"#;
        let new = br#"{"phase":"committed"}"#;
        atomic(&path, old).unwrap();
        let held = read_open(&path).unwrap();
        atomic(&path, new).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(held.metadata().unwrap().nlink(), 0);
        }
        assert_eq!(read_handle(held, 1024).unwrap(), old);
        assert_eq!(read(&path, 1024).unwrap(), new);
        assert!(read(&path, new.len() - 1).is_err());
    }

    #[test]
    fn snapshots_reject_real_hardlinks_and_directories() {
        let fixture = Fixture::new();
        let path = fixture.state();
        let alias = fixture.0.join("alias.json");
        atomic(&path, b"private state").unwrap();
        fs::hard_link(&path, &alias).unwrap();
        assert!(regular(&path, 1024).is_err());
        assert!(read(&path, 1024).is_err());
        assert!(read_handle(read_open(&path).unwrap(), 1024).is_err());
        fs::remove_file(alias).unwrap();
        assert_eq!(read(&path, 1024).unwrap(), b"private state");
        assert!(read(&fixture.0, 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_reject_nonprivate_files_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let path = fixture.state();
        atomic(&path, b"private state").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path, 1024).is_err());
        assert!(read_handle(read_open(&path).unwrap(), 1024).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = fixture.0.join("symlink.json");
        symlink(&path, &alias).unwrap();
        assert!(read_open(&alias).is_err());
        assert!(read(&alias, 1024).is_err());
        assert_eq!(read(&path, 1024).unwrap(), b"private state");
    }
}
