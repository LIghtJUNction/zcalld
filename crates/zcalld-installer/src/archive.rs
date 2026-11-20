use crate::{
    MAX_ENTRIES, MAX_FILE, MAX_METADATA, MAX_UNPACKED,
    hash::{self, Sha256},
    json::Value,
    private, process,
    release::Plan,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}
#[derive(Clone, Debug)]
pub struct Manifest {
    pub raw: Vec<u8>,
    pub files: BTreeMap<String, Entry>,
}

pub fn safe_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 384
        || path.starts_with('/')
        || path.ends_with('/')
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+@".contains(&b))
    {
        return Err("archive contains an unsafe path".into());
    }
    let mut depth = 0;
    for part in path.split('/') {
        depth += 1;
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > 128
            || part.ends_with('.')
            || depth > 20
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err("archive path is not portable".into());
        }
    }
    if path == ".zcalld-install.json" {
        return Err("archive contains reserved installation state".into());
    }
    Ok(())
}

pub fn required(target: &str) -> Vec<String> {
    let windows = target.ends_with("windows-msvc");
    let mac = target.ends_with("apple-darwin");
    let mut files = vec![
        format!("bin/zcalld{}", if windows { ".exe" } else { "" }),
        format!("bin/zcalld-installer{}", if windows { ".exe" } else { "" }),
    ];
    for name in [
        "zcalld_aliases",
        "zcalld_policy",
        "zcalld_redact",
        "zcalld_community",
        "zcalld_plugin_routing",
        "zcalld_desktop",
    ] {
        files.push(format!(
            "lib/zcalld/plugins/{}{}{}",
            if windows { "" } else { "lib" },
            name,
            if windows {
                ".dll"
            } else if mac {
                ".dylib"
            } else {
                ".so"
            }
        ));
    }
    if windows {
        files.push("libexec/zcalld/zcalld-desktop-notify.exe".into());
    }
    if mac {
        files.extend([
            "libexec/zcalld/zcalld-desktop-notify.app/Contents/Info.plist".into(),
            "libexec/zcalld/zcalld-desktop-notify.app/Contents/MacOS/zcalld-desktop-notify".into(),
        ]);
    }
    files
}

impl Manifest {
    pub fn parse(raw: Vec<u8>, plan: &Plan) -> Result<Self, String> {
        let value = crate::strict_json(&raw, MAX_METADATA)?;
        let allowed = [
            "schema", "product", "version", "target", "source", "files", "required",
        ];
        let object = value
            .as_object()
            .ok_or("installation manifest must be an object")?;
        if object.keys().any(|k| !allowed.contains(&k.as_str()))
            || value.get("schema").as_u64() != Some(1)
            || value.get("product").as_str() != Some("zcalld")
        {
            return Err("unsupported installation manifest".into());
        }
        let version = crate::string(&value, "version")?;
        let target = crate::string(&value, "target")?;
        let source = crate::string(&value, "source")?;
        if version != plan.version || target != plan.target || source != plan.source {
            return Err(
                "manifest does not match the fixed release version, target and source".into(),
            );
        }
        let list = match value.get("files") {
            Value::Array(v) if !v.is_empty() && v.len() <= MAX_ENTRIES => v,
            _ => return Err("invalid installation file count".into()),
        };
        let mut files = BTreeMap::new();
        let mut folded = BTreeMap::new();
        let mut total = 0u64;
        for item in list {
            let obj = item.as_object().ok_or("invalid installation file record")?;
            if obj.len() != 4
                || obj
                    .keys()
                    .any(|k| !["path", "size", "sha256", "mode"].contains(&k.as_str()))
            {
                return Err("invalid installation file fields".into());
            }
            let path = crate::string(item, "path")?;
            safe_path(&path)?;
            if path == "INSTALL.json" {
                return Err("manifest must not recursively list itself".into());
            }
            let size = item
                .get("size")
                .as_u64()
                .filter(|n| *n <= MAX_FILE)
                .ok_or("invalid installation file size")?;
            total = total
                .checked_add(size)
                .filter(|n| *n <= MAX_UNPACKED)
                .ok_or("unpacked archive exceeds its limit")?;
            let sha256 = crate::string(item, "sha256")?;
            if !crate::hex(&sha256, 64) {
                return Err("invalid installation file digest".into());
            }
            let mode = item
                .get("mode")
                .as_u64()
                .filter(|m| matches!(*m, 0o644 | 0o755))
                .ok_or("invalid installed file mode")? as u32;
            let mut prefix = String::new();
            for component in path.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                let key = prefix.to_ascii_lowercase();
                if let Some(previous) = folded.insert(key, prefix.clone())
                    && previous != prefix
                {
                    return Err("archive has case-insensitive path collisions".into());
                }
            }
            if files
                .insert(
                    path.clone(),
                    Entry {
                        path,
                        size,
                        sha256,
                        mode,
                    },
                )
                .is_some()
            {
                return Err("duplicate installation path".into());
            }
        }
        for path in files.keys() {
            let mut parent = Path::new(path).parent();
            while let Some(p) = parent {
                let s = p.to_str().ok_or("invalid manifest parent")?;
                if files.contains_key(s) {
                    return Err("archive file is also a parent directory".into());
                }
                parent = p.parent();
            }
        }
        let requested = match value.get("required") {
            Value::Array(v) if v.len() <= 64 => v,
            _ => return Err("invalid required component list".into()),
        };
        let mut declared = BTreeSet::new();
        for item in requested {
            let name = item.as_str().ok_or("invalid required component")?;
            safe_path(name)?;
            if !declared.insert(name) || !files.get(name).is_some_and(|f| f.size > 0) {
                return Err("missing or duplicate required component".into());
            }
        }
        for name in required(&target) {
            if !declared.contains(name.as_str()) {
                return Err(format!("required native component is missing: {name}"));
            }
        }
        if target.ends_with("windows-msvc")
            && !files.keys().any(|p| {
                p.starts_with("bin/")
                    && p.to_ascii_lowercase().contains("curl")
                    && p.ends_with(".dll")
            })
        {
            return Err("Windows archive is missing its curl runtime".into());
        }
        Ok(Self { raw, files })
    }
    pub fn digest(&self) -> String {
        hash::sha256(&self.raw)
    }
}

struct Member {
    name: String,
    directory: bool,
}
fn tar() -> Result<PathBuf, String> {
    let program = process::executable("tar")?;
    let mut command = process::command(&program);
    command.arg("--version");
    let bytes = process::capture(command, 8192, Duration::from_secs(5))?;
    let version = String::from_utf8(bytes).map_err(|_| "unrecognized system tar")?;
    if !version.starts_with("tar (GNU tar)") && !version.starts_with("bsdtar ") {
        return Err("installer requires GNU tar or bsdtar".into());
    }
    Ok(program)
}
fn enumerate(program: &Path, archive: &Path, root: &str) -> Result<Vec<Member>, String> {
    let mut names = process::command(program);
    names.arg("-tf").arg(archive);
    let names = process::capture(names, MAX_METADATA, Duration::from_secs(20))?;
    let mut details = process::command(program);
    details.arg("--numeric-owner").arg("-tvf").arg(archive);
    let details = process::capture(details, 2 * MAX_METADATA, Duration::from_secs(20))?;
    let names = String::from_utf8(names).map_err(|_| "archive names are not UTF-8")?;
    let details = String::from_utf8(details).map_err(|_| "archive listing is not UTF-8")?;
    let names = names.lines().collect::<Vec<_>>();
    let details = details.lines().collect::<Vec<_>>();
    if names.is_empty() || names.len() > MAX_ENTRIES * 2 || names.len() != details.len() {
        return Err("invalid archive enumeration".into());
    }
    let mut unique = BTreeSet::new();
    let mut result = Vec::new();
    for (name, detail) in names.into_iter().zip(details) {
        let directory = name.ends_with('/');
        let plain = name.strip_suffix('/').unwrap_or(name);
        safe_path(plain)?;
        if plain != root && !plain.starts_with(&format!("{root}/")) {
            return Err("archive has an unexpected root".into());
        }
        if !unique.insert(plain.to_ascii_lowercase()) {
            return Err("archive contains duplicate or case-colliding entries".into());
        }
        let fields = detail.split_whitespace().collect::<Vec<_>>();
        let kind = fields.first().and_then(|v| v.as_bytes().first()).copied();
        if fields.len() < 5
            || fields.last().copied() != Some(name)
            || fields[0].len() != 10
            || !fields[0].as_bytes()[1..]
                .iter()
                .all(|b| b"rwxstST-".contains(b))
            || (directory && kind != Some(b'd'))
            || (!directory && kind != Some(b'-'))
        {
            return Err("archive contains a link, special entry, or ambiguous listing".into());
        }
        result.push(Member {
            name: name.to_owned(),
            directory,
        });
    }
    Ok(result)
}

pub fn unpack(archive: &Path, stage: &Path, plan: &Plan) -> Result<Manifest, String> {
    let program = tar()?;
    let root = plan.stem();
    let members = enumerate(&program, archive, &root)?;
    let manifest_name = format!("{root}/INSTALL.json");
    if !members
        .iter()
        .any(|m| m.name == manifest_name && !m.directory)
    {
        return Err("native archive has no INSTALL.json".into());
    }
    let mut command = process::command(&program);
    command
        .arg("-xOf")
        .arg(archive)
        .arg("--")
        .arg(&manifest_name);
    let raw = process::capture(command, MAX_METADATA, Duration::from_secs(20))?;
    let manifest = Manifest::parse(raw, plan)?;
    let mut directories = BTreeSet::new();
    directories.insert(root.clone());
    for path in manifest
        .files
        .keys()
        .chain(std::iter::once(&"INSTALL.json".to_owned()))
    {
        let full = format!("{root}/{path}");
        let mut parent = Path::new(&full).parent();
        while let Some(p) = parent {
            let name = p.to_str().ok_or("invalid archive parent")?;
            if !name.is_empty() {
                directories.insert(name.into());
            }
            parent = p.parent();
        }
    }
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for member in &members {
        if member.directory {
            if !directories.contains(member.name.trim_end_matches('/')) {
                return Err("archive contains an unlisted directory".into());
            }
            continue;
        }
        let path = member
            .name
            .strip_prefix(&format!("{root}/"))
            .ok_or("invalid archive root file")?;
        let entry = if path == "INSTALL.json" {
            Entry {
                path: path.into(),
                size: manifest.raw.len() as u64,
                sha256: manifest.digest(),
                mode: 0o644,
            }
        } else {
            manifest
                .files
                .get(path)
                .cloned()
                .ok_or("archive contains an unlisted file")?
        };
        total = total
            .checked_add(entry.size)
            .filter(|n| *n <= MAX_UNPACKED)
            .ok_or("unpacked archive exceeds its limit")?;
        seen.insert(path.to_owned());
        order.push(entry);
    }
    if seen.len() != manifest.files.len() + 1 {
        return Err("archive omits manifest files".into());
    }
    private::mkdir(stage)?;
    let mut index = 0usize;
    let mut written = 0u64;
    let mut current: Option<File> = None;
    let mut hash = Sha256::new();
    let mut command = process::command(program);
    command.arg("-xOf").arg(archive);
    process::stream(command, total, Duration::from_secs(90), |mut chunk| {
        while !chunk.is_empty() || index < order.len() && order[index].size == 0 {
            let entry = order
                .get(index)
                .ok_or("archive stdout contains extra bytes")?;
            if current.is_none() {
                let path = stage.join(&entry.path);
                private::parents_private(path.parent().ok_or("file has no parent")?)?;
                current = Some(private::create(&path)?);
            }
            let count = (entry.size - written).min(chunk.len() as u64) as usize;
            current
                .as_mut()
                .unwrap()
                .write_all(&chunk[..count])
                .map_err(|_| "cannot write staged file")?;
            hash.update(&chunk[..count]);
            written += count as u64;
            chunk = &chunk[count..];
            if written == entry.size {
                if std::mem::replace(&mut hash, Sha256::new()).finish() != entry.sha256 {
                    return Err("staged file checksum mismatch".into());
                }
                let file = current.take().unwrap();
                private::mode(&file, entry.mode)?;
                file.sync_all().map_err(|_| "cannot sync staged file")?;
                written = 0;
                index += 1;
            }
        }
        Ok(())
    })?;
    if index != order.len() {
        return Err("archive stdout ended before all manifest files".into());
    }
    verify_tree(stage, &manifest, false)?;
    Ok(manifest)
}

pub fn read_manifest(root: &Path, plan: &Plan) -> Result<Manifest, String> {
    let path = root.join("INSTALL.json");
    private::regular(&path, MAX_METADATA as u64)?;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(MAX_METADATA as u64 + 1).read_to_end(&mut bytes))
        .map_err(|_| "cannot read native manifest")?;
    Manifest::parse(bytes, plan)
}

pub fn verify_tree(root: &Path, manifest: &Manifest, receipt: bool) -> Result<(), String> {
    private::check_dir(root)?;
    let mut queue = vec![root.to_path_buf()];
    let mut seen = BTreeSet::new();
    let mut count = 0;
    let mut allowed = manifest.files.clone();
    allowed.insert(
        "INSTALL.json".into(),
        Entry {
            path: "INSTALL.json".into(),
            size: manifest.raw.len() as u64,
            sha256: manifest.digest(),
            mode: 0o644,
        },
    );
    let mut directories = BTreeSet::new();
    for path in allowed.keys() {
        let mut p = Path::new(path).parent();
        while let Some(parent) = p {
            if let Some(name) = parent.to_str()
                && !name.is_empty()
            {
                directories.insert(name.to_owned());
            }
            p = parent.parent();
        }
    }
    while let Some(dir) = queue.pop() {
        for item in fs::read_dir(&dir).map_err(|_| "cannot inspect installed directory")? {
            count += 1;
            if count > MAX_ENTRIES * 2 + 1 {
                return Err("installation tree exceeds its limit".into());
            }
            let path = item.map_err(|_| "cannot inspect installed entry")?.path();
            private::no_links(&path)?;
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "installed path escaped prefix")?
                .to_str()
                .ok_or("non-UTF8 installed path")?
                .replace('\\', "/");
            let meta = fs::symlink_metadata(&path).map_err(|_| "cannot inspect installed entry")?;
            if meta.is_dir() {
                private::check_dir(&path)?;
                if !directories.contains(&relative) {
                    return Err("installation contains an unowned directory".into());
                }
                queue.push(path);
                continue;
            }
            if receipt && relative == ".zcalld-install.json" {
                private::read(&path, MAX_METADATA)?;
                continue;
            }
            let expected = allowed
                .get(&relative)
                .ok_or("installation contains an unowned file")?;
            private::regular(&path, expected.size)?;
            #[cfg(windows)]
            private::check_file_private(&path)?;
            if meta.len() != expected.size
                || hash::sha256_file(&path, expected.size)? != expected.sha256
            {
                return Err("installed file changed outside the installer".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o777 != expected.mode {
                    return Err("installed file permissions changed".into());
                }
            }
            seen.insert(relative);
        }
    }
    if seen.len() != allowed.len() {
        return Err("installed component is missing".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_names_reject_escape_aliases_and_reserved_devices() {
        for name in [
            "../escape",
            "/escape",
            "C:/x",
            "bin\\x",
            "a//b",
            "a/../b",
            "a/CON.txt",
            "a/lpt1",
            "a/trailing.",
            "a/x:y",
            "a/control\n",
        ] {
            assert!(safe_path(name).is_err(), "{name:?}");
        }
        for name in [
            "bin/zcalld",
            "lib/zcalld/plugins/libzcalld_desktop.so",
            "share/doc/NOTICE.txt",
            "share/licenses/zcalld-compat/third-party/golang.org__x__text@v0.39.0.txt",
        ] {
            assert!(safe_path(name).is_ok());
        }
    }
}
