//! Installed optional helpers and the exact Linux package-layout transition.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum Helper {
    Pluginctl,
    Privacy,
}

impl Helper {
    pub fn file_name(self) -> String {
        format!("{}{}", self.name(), std::env::consts::EXE_SUFFIX)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pluginctl => "zcalld-pluginctl",
            Self::Privacy => "zcalld-privacy",
        }
    }
}

pub fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !path.is_absolute() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn candidates_near(helper: Helper, executable: Option<&Path>) -> Vec<PathBuf> {
    let file = helper.file_name();
    let mut paths = Vec::new();
    if let Some(dir) = executable.and_then(Path::parent) {
        if let Some(prefix) = dir.parent() {
            #[cfg(target_os = "linux")]
            paths.push(prefix.join("lib/zcalld-compat").join(&file));
            paths.push(prefix.join("libexec/zcalld").join(&file));
        }
        #[cfg(target_os = "linux")]
        paths.push(dir.join("lib/zcalld-compat").join(&file));
        paths.push(dir.join("libexec/zcalld").join(&file));
    }
    #[cfg(target_os = "linux")]
    paths.push(Path::new("/usr/lib/zcalld-compat").join(&file));
    paths.push(Path::new("/usr/libexec/zcalld").join(&file));
    paths
}

pub fn candidates(helper: Helper) -> Vec<PathBuf> {
    candidates_near(helper, std::env::current_exe().ok().as_deref())
}

pub fn find(helper: Helper) -> Option<PathBuf> {
    candidates(helper).into_iter().find(|path| executable(path))
}

#[cfg(any(target_os = "linux", test))]
fn renamed_default(path: &Path, legacy: &Path, canonical: &Path) -> Option<PathBuf> {
    // Path equality normalizes separators and "." components. The migration is
    // deliberately limited to the literal old default written by this project.
    if path.as_os_str() != legacy.as_os_str()
        || !matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        || !executable(canonical)
    {
        return None;
    }
    Some(canonical.to_path_buf())
}

/// Resolve a missing, exact old Linux default without rewriting configuration.
/// Existing files, directories, inaccessible nodes and dangling symlinks retain
/// their configured identity. Custom paths never fall back to a different helper.
pub fn resolve_legacy(path: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    for helper in [Helper::Pluginctl, Helper::Privacy] {
        let legacy = Path::new("/usr/libexec/zcalld").join(helper.name());
        let canonical = Path::new("/usr/lib/zcalld-compat").join(helper.name());
        if let Some(found) = renamed_default(path, &legacy, &canonical) {
            return found;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zcalld-helper-paths-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn program(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"fixture").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_missing_exact_default_uses_the_new_executable() {
        let fixture = Fixture::new();
        let legacy = fixture.0.join("legacy");
        let canonical = fixture.program("canonical");
        assert_eq!(
            renamed_default(&legacy, &legacy, &canonical),
            Some(canonical.clone())
        );
        assert!(renamed_default(&fixture.0.join("custom"), &legacy, &canonical).is_none());
        assert!(renamed_default(&fixture.0.join("./legacy"), &legacy, &canonical).is_none());
        assert!(renamed_default(&legacy, &legacy, &fixture.0.join("missing")).is_none());
        std::fs::write(&legacy, b"configured helper").unwrap();
        assert!(renamed_default(&legacy, &legacy, &canonical).is_none());
        std::fs::remove_file(&legacy).unwrap();
        std::fs::create_dir(&legacy).unwrap();
        assert!(renamed_default(&legacy, &legacy, &canonical).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_link_and_nonexecutable_replacement_are_not_redirected() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let fixture = Fixture::new();
        let legacy = fixture.0.join("legacy");
        let canonical = fixture.program("canonical");
        symlink(fixture.0.join("absent-target"), &legacy).unwrap();
        assert!(renamed_default(&legacy, &legacy, &canonical).is_none());
        std::fs::remove_file(&legacy).unwrap();
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(renamed_default(&legacy, &legacy, &canonical).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_and_portable_layouts_prefer_arch_library_directory() {
        let paths = candidates_near(
            Helper::Pluginctl,
            Some(Path::new("/opt/fixture/usr/bin/zcalld")),
        );
        assert_eq!(
            paths[0],
            Path::new("/opt/fixture/usr/lib/zcalld-compat/zcalld-pluginctl")
        );
        assert_eq!(
            paths[1],
            Path::new("/opt/fixture/usr/libexec/zcalld/zcalld-pluginctl")
        );
        assert!(paths.contains(&PathBuf::from("/usr/lib/zcalld-compat/zcalld-pluginctl")));
        assert!(paths.contains(&PathBuf::from("/usr/libexec/zcalld/zcalld-pluginctl")));
    }
}
