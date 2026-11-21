//! Fetch skill files as Git objects; never check out or execute repository code.
use super::{FileData, MAX_FILES, MAX_TREE, relative, resource_path};
use crate::{
    control,
    state_json::{Value, object},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stop_git(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // Every Git invocation has its own group, including HTTPS helpers.
        // Terminate that group before joining pipe readers on a timeout.
        if let Ok(pid) = i32::try_from(child.id()) {
            unsafe {
                kill(-pid, 9);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn source_location(source: &str) -> Result<String, String> {
    if source.is_empty() || source.chars().any(char::is_control) {
        return Err("Git source must be an HTTPS URL or an existing local repository".into());
    }
    if let Some(rest) = source.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or_default();
        if host.is_empty() || host.contains('@') || source.contains(['?', '#', '\\']) {
            return Err(
                "Git HTTPS sources must not contain credentials, queries or fragments".into(),
            );
        }
        return Ok(source.to_owned());
    }
    let path = Path::new(source);
    if path.is_dir() {
        return path
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| e.to_string());
    }
    Err("Git source must be an HTTPS URL or an existing local repository".into())
}

fn git(dir: &Path, args: &[&str], input: Option<Vec<u8>>, limit: usize) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-C",
        ])
        .arg(dir)
        .args([
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "credential.interactive=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start git: {e}"))?;
    let output = child.stdout.take().unwrap();
    let errors = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let error_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = errors.take(64 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let writer = input.map(|bytes| {
        let mut pipe = child.stdin.take().unwrap();
        std::thread::spawn(move || pipe.write_all(&bytes))
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < Duration::from_secs(120) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Ok(None) => {
                stop_git(&mut child);
                break Err("Git skill operation timed out after 120 seconds".to_owned());
            }
            Err(e) => {
                stop_git(&mut child);
                break Err(e.to_string());
            }
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let output = reader
        .join()
        .map_err(|_| "Git output reader failed")?
        .map_err(|e| e.to_string())?;
    let errors = error_reader.join().unwrap_or_default();
    let status = status?;
    if output.len() > limit {
        return Err("Git skill output exceeds the library size limit".into());
    }
    if !status.success() {
        return Err(format!(
            "Git skill operation failed: {}",
            String::from_utf8_lossy(&errors).trim()
        ));
    }
    Ok(output)
}

struct Blob {
    path: PathBuf,
    oid: String,
    size: usize,
    executable: bool,
}

fn blob_sizes(blobs: &mut [Blob], output: &[u8]) -> Result<usize, String> {
    let output = std::str::from_utf8(output).map_err(|_| "invalid Git blob size response")?;
    let mut lines = output.lines();
    let mut total = 0usize;
    for blob in blobs {
        let columns: Vec<_> = lines
            .next()
            .ok_or("incomplete Git blob sizes")?
            .split_whitespace()
            .collect();
        if columns.len() != 3 || columns[0] != blob.oid || columns[1] != "blob" {
            return Err("Git skill blob is missing or changed identity".into());
        }
        blob.size = columns[2].parse().map_err(|_| "invalid Git blob size")?;
        total = total.checked_add(blob.size).ok_or("skill size overflow")?;
        if blob.size > 8 * 1024 * 1024 || total > MAX_TREE {
            return Err("Git skill exceeds 8 MiB per file or 32 MiB total".into());
        }
    }
    if lines.next().is_some() {
        return Err("unexpected data after Git blob sizes".into());
    }
    Ok(total)
}

pub(super) fn fetch(
    directory: &Path,
    source: &str,
    revision: &str,
    subdirectory: &str,
) -> Result<(BTreeMap<PathBuf, FileData>, Value), String> {
    let source = source_location(source)?;
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.len() > 256
        || revision.contains([':', '?', '*', '[', '\\', '^', '~'])
        || revision.contains("..")
        || revision.contains("@{")
        || revision
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("Git revision must be a branch, tag or commit".into());
    }
    let subdirectory = if subdirectory == "." {
        String::new()
    } else {
        resource_path(&relative(subdirectory)?)
    };
    let staging = directory.join(format!(
        ".skill-fetch-{}-{}",
        std::process::id(),
        control::now()
    ));
    fs::create_dir(&staging).map_err(|e| e.to_string())?;
    let staging = Staging(staging);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging.0, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    git(&staging.0, &["init", "--quiet", "--bare"], None, 4096)?;
    git(
        &staging.0,
        &["remote", "add", "origin", &source],
        None,
        4096,
    )?;
    git(
        &staging.0,
        &[
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "--no-recurse-submodules",
            "--filter=blob:none",
            "--",
            "origin",
            revision,
        ],
        None,
        4096,
    )?;
    let commit = String::from_utf8(git(
        &staging.0,
        &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
        None,
        128,
    )?)
    .map_err(|_| "Git returned an invalid commit")?
    .trim()
    .to_owned();
    let tree_path = if subdirectory.is_empty() {
        "."
    } else {
        &subdirectory
    };
    // Listing sizes here would lazily download one blob per Git invocation.
    // Validate the selected paths first, then fetch their objects in one batch.
    let listing = git(
        &staging.0,
        &["ls-tree", "-rz", "--full-tree", &commit, "--", tree_path],
        None,
        1024 * 1024,
    )?;
    let mut blobs = Vec::new();
    for record in listing
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).map_err(|_| "skill paths must be valid UTF-8")?;
        let (header, filename) = record.split_once('\t').ok_or("invalid Git tree entry")?;
        let columns: Vec<_> = header.split_whitespace().collect();
        if columns.len() != 3 || columns[1] != "blob" || !matches!(columns[0], "100644" | "100755")
        {
            return Err(
                "Git skills must contain regular files, without symlinks or submodules".into(),
            );
        }
        let filename = if subdirectory.is_empty() {
            filename
        } else {
            filename
                .strip_prefix(&format!("{subdirectory}/"))
                .ok_or("Git skill path leaves its selected directory")?
        };
        let path = relative(filename)?;
        if path.components().count() > 32 {
            return Err("skill directory depth exceeds 32 levels".into());
        }
        if blobs.len() >= MAX_FILES {
            return Err("Git skill exceeds 2048 files".into());
        }
        blobs.push(Blob {
            path,
            oid: columns[2].to_owned(),
            size: 0,
            executable: cfg!(unix) && columns[0] == "100755",
        });
    }
    if !blobs.iter().any(|blob| blob.path == Path::new("SKILL.md")) {
        return Err("selected Git directory must contain SKILL.md".into());
    }
    let unique = blobs
        .iter()
        .map(|blob| blob.oid.as_str())
        .collect::<BTreeSet<_>>();
    let wanted = unique
        .iter()
        .flat_map(|oid| oid.bytes().chain(std::iter::once(b'\n')))
        .collect();
    git(
        &staging.0,
        &[
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--filter=blob:none",
            "--stdin",
            "--",
            "origin",
        ],
        Some(wanted),
        4096,
    )?;
    let input: Vec<u8> = blobs
        .iter()
        .flat_map(|blob| blob.oid.bytes().chain(std::iter::once(b'\n')))
        .collect();
    let sizes = git(
        &staging.0,
        &["cat-file", "--batch-check"],
        Some(input.clone()),
        blobs.len() * 128,
    )?;
    let total = blob_sizes(&mut blobs, &sizes)?;
    let output = git(
        &staging.0,
        &["cat-file", "--batch"],
        Some(input),
        total + blobs.len() * 128,
    )?;
    let mut remaining = output.as_slice();
    let mut tree = BTreeMap::new();
    for blob in blobs {
        let end = remaining
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("incomplete Git blob header")?;
        let header =
            std::str::from_utf8(&remaining[..end]).map_err(|_| "invalid Git blob header")?;
        let expected = format!("{} blob {}", blob.oid, blob.size);
        if header != expected {
            return Err("Git blob identity or size changed".into());
        }
        remaining = &remaining[end + 1..];
        if remaining.len() <= blob.size || remaining[blob.size] != b'\n' {
            return Err("incomplete Git skill file".into());
        }
        tree.insert(
            blob.path,
            FileData {
                bytes: remaining[..blob.size].to_vec(),
                executable: blob.executable,
            },
        );
        remaining = &remaining[blob.size + 1..];
    }
    if !remaining.is_empty() {
        return Err("unexpected data after Git skill files".into());
    }
    let metadata = object([
        ("kind", "git".into()),
        ("url", source.into()),
        ("ref", revision.into()),
        (
            "path",
            if subdirectory.is_empty() {
                ".".into()
            } else {
                subdirectory.into()
            },
        ),
        ("commit", commit.into()),
        ("files", super::tree_record(&tree)),
    ]);
    Ok((tree, metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn blob(oid: &str) -> Blob {
        Blob {
            path: PathBuf::from("SKILL.md"),
            oid: oid.into(),
            size: 0,
            executable: false,
        }
    }

    #[test]
    fn batch_sizes_validate_identity_and_bound_files_before_reading_contents() {
        let mut blobs = [blob("a"), blob("b")];
        assert_eq!(
            blob_sizes(&mut blobs, b"a blob 0\nb blob 256\n").unwrap(),
            256
        );
        assert_eq!(blobs[1].size, 256);
        for output in [
            b"a missing\n".as_slice(),
            b"b blob 1\n",
            b"a tree 1\n",
            b"a blob 1\nb blob 2\n",
        ] {
            assert!(blob_sizes(&mut [blob("a")], output).is_err());
        }
        let too_large = format!("a blob {}\n", 8 * 1024 * 1024 + 1);
        assert!(blob_sizes(&mut [blob("a")], too_large.as_bytes()).is_err());
        let mut blobs = (0..5).map(|_| blob("a")).collect::<Vec<_>>();
        let too_large = format!("a blob {}\n", 8 * 1024 * 1024).repeat(5);
        assert!(blob_sizes(&mut blobs, too_large.as_bytes()).is_err());
    }

    #[test]
    fn git_source_rejects_credentials_and_remote_helpers() {
        for source in [
            "https://user:secret@example.com/repo",
            "https:///repo",
            "ext::sh -c arbitrary",
            "https://host/repo?secret=yes",
            "https://host/repo#fragment",
            "https://host/repo\n",
        ] {
            assert!(source_location(source).is_err(), "{source}");
        }
        assert_eq!(
            source_location("https://github.com/owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }
}
