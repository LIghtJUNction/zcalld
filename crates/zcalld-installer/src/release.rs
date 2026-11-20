use crate::{
    MAX_ARCHIVE, MAX_METADATA, REPOSITORY,
    json::{self, Value},
    private, process,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub fn host_target() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
}
pub fn default_prefix() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let value = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("zcalld/runtime"));
    #[cfg(target_os = "macos")]
    let value = std::env::var_os("HOME")
        .map(|p| PathBuf::from(p).join("Library/Application Support/zcalld/runtime"));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let value = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .map(|p| p.join("zcalld/runtime"));
    normalize_prefix(&value.ok_or("cannot locate the private installation directory")?)
}
pub fn normalize_prefix(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("installation prefix must be an absolute normalized UTF-8 path".into());
    }
    #[cfg(windows)]
    if !matches!(path.components().next(),Some(Component::Prefix(p)) if matches!(p.kind(),std::path::Prefix::Disk(_)|std::path::Prefix::VerbatimDisk(_)))
    {
        return Err("installation requires a local Windows drive".into());
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("invalid installation prefix")?;
    crate::archive::safe_path(name)?;
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_dir() || meta.file_type().is_symlink())
    {
        return Err("installation prefix is not a regular directory".into());
    }
    let mut existing = path.parent().ok_or("installation prefix has no parent")?;
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or("invalid installation parent")?
                .to_owned(),
        );
        existing = existing
            .parent()
            .ok_or("installation parent cannot be resolved")?;
    }
    let mut parent = existing
        .canonicalize()
        .map_err(|_| "installation parent cannot be resolved")?;
    for part in suffix.into_iter().rev() {
        parent.push(part);
    }
    let result = parent.join(name);
    private::no_links(&result)?;
    if managed_path(&result) {
        return Err(
            "this prefix is managed by the operating system; use its package manager".into(),
        );
    }
    Ok(result)
}
pub fn managed_path(path: &Path) -> bool {
    #[cfg(unix)]
    {
        [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/opt/homebrew",
            "/nix/store",
            "/Applications",
        ]
        .iter()
        .any(|p| path.starts_with(p))
    }
    #[cfg(windows)]
    {
        let text = path
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .to_ascii_lowercase();
        ["ProgramFiles", "ProgramFiles(x86)", "SystemRoot", "WINDIR"]
            .iter()
            .filter_map(std::env::var_os)
            .any(|p| {
                let p = PathBuf::from(p)
                    .to_string_lossy()
                    .trim_start_matches("\\\\?\\")
                    .to_ascii_lowercase();
                text == p || text.starts_with(&(p + "\\"))
            })
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub version: String,
    pub target: String,
    pub source: String,
    pub prefix: PathBuf,
    pub archive_name: String,
    pub archive_sha256: String,
    pub archive_size: u64,
    pub archive_asset_id: u64,
    pub release_id: u64,
    pub checksums_asset_id: u64,
    pub local_archive: Option<PathBuf>,
    pub created_at_ms: u64,
}
impl Plan {
    pub fn stem(&self) -> String {
        format!("zcalld-{}-{}", self.version, self.target)
    }
    pub fn local(
        archive: &Path,
        sha256: &str,
        version: &str,
        source: &str,
        prefix: &Path,
    ) -> Result<Self, String> {
        let archive = archive
            .canonicalize()
            .map_err(|_| "local installation archive is missing")?;
        let size = private::regular(&archive, MAX_ARCHIVE)?.len();
        let name = format!(
            "zcalld-{version}-{}{}",
            host_target(),
            if cfg!(windows) { ".zip" } else { ".tar.gz" }
        );
        let plan = Self {
            version: version.into(),
            target: host_target().into(),
            source: source.into(),
            prefix: normalize_prefix(prefix)?,
            archive_name: name,
            archive_sha256: sha256.into(),
            archive_size: size,
            archive_asset_id: 0,
            release_id: 0,
            checksums_asset_id: 0,
            local_archive: Some(archive),
            created_at_ms: crate::now_ms(),
        };
        plan.validate()?;
        Ok(plan)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !crate::version(&self.version)
            || self.target != host_target()
            || !crate::hex(&self.source, 40)
            || !crate::hex(&self.archive_sha256, 64)
            || self.archive_size == 0
            || self.archive_size > MAX_ARCHIVE
        {
            return Err("invalid fixed installation release".into());
        }
        let expected = format!(
            "{}{}",
            self.stem(),
            if cfg!(windows) { ".zip" } else { ".tar.gz" }
        );
        if self.archive_name != expected || normalize_prefix(&self.prefix)? != self.prefix {
            return Err("installation plan path or asset changed".into());
        }
        match &self.local_archive {
            Some(path)
                if path.is_absolute()
                    && path.to_str().is_some()
                    && self.archive_asset_id == 0
                    && self.release_id == 0
                    && self.checksums_asset_id == 0 => {}
            None if self.archive_asset_id > 0
                && self.release_id > 0
                && self.checksums_asset_id > 0 => {}
            _ => return Err("invalid installation source provenance".into()),
        }
        Ok(())
    }
    pub fn value(&self) -> Value {
        json::object([
            ("schema", 1usize.into()),
            ("repository", REPOSITORY.into()),
            ("version", self.version.clone().into()),
            ("target", self.target.clone().into()),
            ("source", self.source.clone().into()),
            ("prefix", self.prefix.to_string_lossy().into_owned().into()),
            ("archive_name", self.archive_name.clone().into()),
            ("archive_sha256", self.archive_sha256.clone().into()),
            ("archive_size", self.archive_size.into()),
            ("archive_asset_id", self.archive_asset_id.into()),
            ("release_id", self.release_id.into()),
            ("checksums_asset_id", self.checksums_asset_id.into()),
            (
                "local_archive",
                self.local_archive
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned().into())
                    .unwrap_or(Value::Null),
            ),
            ("created_at_ms", self.created_at_ms.into()),
        ])
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let parent = path.parent().ok_or("plan path has no parent")?;
        private::parents_private(parent)?;
        private::check_dir(parent)?;
        private::atomic(path, self.value().stringify().as_bytes())
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::parse(&private::read(path, MAX_METADATA)?)
    }
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let v = crate::strict_json(bytes, MAX_METADATA)?;
        let fields = [
            "schema",
            "repository",
            "version",
            "target",
            "source",
            "prefix",
            "archive_name",
            "archive_sha256",
            "archive_size",
            "archive_asset_id",
            "release_id",
            "checksums_asset_id",
            "local_archive",
            "created_at_ms",
        ];
        if v.as_object().is_none_or(|o| {
            o.len() != fields.len() || o.keys().any(|k| !fields.contains(&k.as_str()))
        }) || v.get("schema").as_u64() != Some(1)
            || v.get("repository").as_str() != Some(REPOSITORY)
        {
            return Err("invalid installation plan fields".into());
        }
        let num = |key: &str| {
            v.get(key)
                .as_u64()
                .ok_or_else(|| format!("invalid plan field: {key}"))
        };
        let local_archive = match v.get("local_archive") {
            Value::Null => None,
            Value::String(s) => Some(PathBuf::from(s)),
            _ => return Err("invalid local archive path".into()),
        };
        let plan = Self {
            version: crate::string(&v, "version")?,
            target: crate::string(&v, "target")?,
            source: crate::string(&v, "source")?,
            prefix: crate::string(&v, "prefix")?.into(),
            archive_name: crate::string(&v, "archive_name")?,
            archive_sha256: crate::string(&v, "archive_sha256")?,
            archive_size: num("archive_size")?,
            archive_asset_id: num("archive_asset_id")?,
            release_id: num("release_id")?,
            checksums_asset_id: num("checksums_asset_id")?,
            local_archive,
            created_at_ms: num("created_at_ms")?,
        };
        plan.validate()?;
        Ok(plan)
    }
}

fn api(path: &str, accept: &str, max: usize) -> Result<Vec<u8>, String> {
    let mut command = curl(
        &format!("https://api.github.com/repos/{REPOSITORY}/{path}"),
        accept,
        max as u64,
    )?;
    command.arg("--output").arg("-");
    process::capture(command, max, Duration::from_secs(35))
}
fn curl(url: &str, accept: &str, max: u64) -> Result<std::process::Command, String> {
    let mut command = process::command(process::executable("curl")?);
    command
        .args([
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "5",
            "--connect-timeout",
            "15",
            "--max-time",
            "120",
            "--max-filesize",
        ])
        .arg(max.to_string())
        .args([
            "--user-agent",
            "zcalld-installer",
            "--header",
            &format!("Accept: {accept}"),
            "--url",
            url,
        ]);
    Ok(command)
}
fn api_json(path: &str) -> Result<Value, String> {
    crate::strict_json(
        &api(path, "application/vnd.github+json", MAX_METADATA)?,
        MAX_METADATA,
    )
}
fn asset<'a>(release: &'a Value, name: &str) -> Result<&'a Value, String> {
    let matches = release
        .get("assets")
        .as_array()
        .iter()
        .filter(|a| a.get("name").as_str() == Some(name))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "official release requires exactly one {name} asset"
        ));
    }
    Ok(matches[0])
}
fn asset_id(value: &Value) -> Result<u64, String> {
    value
        .get("id")
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| "invalid official asset ID".into())
}
pub fn checksums(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    if bytes.len() > MAX_METADATA {
        return Err("checksum list exceeds its limit".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "checksum list is not UTF-8")?;
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let (digest, name) = line.split_once("  ").ok_or("invalid checksum record")?;
        if !crate::hex(digest, 64)
            || name.contains('/')
            || name.len() > 384
            || crate::archive::safe_path(name).is_err()
            || map.len() >= 128
            || map.insert(name.into(), digest.into()).is_some()
        {
            return Err("invalid or duplicate checksum entry".into());
        }
    }
    if map.is_empty() {
        return Err("empty checksum list".into());
    }
    Ok(map)
}
pub fn plan_official(requested: Option<&str>, prefix: Option<&Path>) -> Result<Plan, String> {
    if requested.is_some_and(|v| !crate::version(v)) {
        return Err("release version must be a stable major.minor.patch".into());
    }
    let resource = requested
        .map(|v| format!("releases/tags/v{v}"))
        .unwrap_or_else(|| "releases/latest".into());
    let release = api_json(&resource)?;
    if release.get("draft").as_bool() != Some(false)
        || release.get("prerelease").as_bool() != Some(false)
    {
        return Err("installer only accepts published stable releases".into());
    }
    let tag = crate::string(&release, "tag_name")?;
    let version = tag
        .strip_prefix('v')
        .filter(|v| crate::version(v))
        .ok_or("invalid official release tag")?;
    if requested.is_some_and(|v| v != version) {
        return Err("official release does not match the requested version".into());
    }
    let reference = api_json(&format!("git/ref/tags/{tag}"))?;
    let mut object = reference.get("object").clone();
    if object.get("type").as_str() == Some("tag") {
        let id = crate::string(&object, "sha")?;
        if !crate::hex(&id, 40) {
            return Err("invalid official tag object".into());
        }
        object = api_json(&format!("git/tags/{id}"))?.get("object").clone();
    }
    let source = crate::string(&object, "sha")?;
    if object.get("type").as_str() != Some("commit") || !crate::hex(&source, 40) {
        return Err("official tag is not a fixed source commit".into());
    }
    let target = host_target();
    let stem = format!("zcalld-{version}-{target}");
    let name = format!("{stem}{}", if cfg!(windows) { ".zip" } else { ".tar.gz" });
    let archive = asset(&release, &name)?;
    let checks = asset(&release, "SHA256SUMS")?;
    let checksums_asset_id = asset_id(checks)?;
    let map = checksums(&api(
        &format!("releases/assets/{checksums_asset_id}"),
        "application/octet-stream",
        MAX_METADATA,
    )?)?;
    let archive_sha256 = map
        .get(&name)
        .ok_or("official archive checksum is missing")?
        .clone();
    let helper_name = format!(
        "zcalld-installer-{version}-{target}{}",
        if cfg!(windows) { ".exe" } else { "" }
    );
    asset(&release, &helper_name)?;
    if !map.contains_key(&helper_name) {
        return Err("official installer checksum is missing".into());
    }
    let plan = Plan {
        version: version.into(),
        target: target.into(),
        source,
        prefix: match prefix {
            Some(p) => normalize_prefix(p)?,
            None => default_prefix()?,
        },
        archive_name: name,
        archive_sha256,
        archive_size: archive
            .get("size")
            .as_u64()
            .ok_or("invalid official archive size")?,
        archive_asset_id: asset_id(archive)?,
        release_id: release
            .get("id")
            .as_u64()
            .ok_or("invalid official release ID")?,
        checksums_asset_id,
        local_archive: None,
        created_at_ms: crate::now_ms(),
    };
    plan.validate()?;
    Ok(plan)
}
pub fn obtain(plan: &Plan, path: &Path) -> Result<(), String> {
    let mut file = private::create(path)?;
    let mut hash = crate::hash::Sha256::new();
    let mut size = 0u64;
    let mut consume = |bytes: &[u8]| {
        size += bytes.len() as u64;
        if size > plan.archive_size {
            return Err("archive exceeds its fixed release size".into());
        }
        hash.update(bytes);
        file.write_all(bytes)
            .map_err(|_| "cannot write installation archive".into())
    };
    if let Some(source) = &plan.local_archive {
        use std::io::Read;
        private::regular(source, MAX_ARCHIVE)?;
        let mut input = fs::File::open(source).map_err(|_| "cannot read local archive")?;
        let mut buffer = [0u8; 65536];
        loop {
            let n = input
                .read(&mut buffer)
                .map_err(|_| "cannot read local archive")?;
            if n == 0 {
                break;
            }
            consume(&buffer[..n])?;
        }
    } else {
        let url = format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/assets/{}",
            plan.archive_asset_id
        );
        process::stream(
            curl(&url, "application/octet-stream", plan.archive_size)?,
            plan.archive_size,
            Duration::from_secs(125),
            &mut consume,
        )?;
    }
    if size != plan.archive_size || hash.finish() != plan.archive_sha256 {
        return Err("archive size or SHA-256 does not match the fixed release".into());
    }
    file.sync_all()
        .map_err(|_| "cannot sync downloaded archive".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksums_require_unique_bounded_unambiguous_names() {
        let d = "a".repeat(64);
        assert!(checksums(format!("{d}  file\n").as_bytes()).is_ok());
        assert!(checksums(format!("{d}  file\n{d}  file\n").as_bytes()).is_err());
        assert!(checksums(format!("{d}  ../file\n").as_bytes()).is_err());
    }
}
