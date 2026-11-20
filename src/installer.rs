//! On-demand full native bundle installation; the daemon only holds a lease.
use std::path::Path;

pub fn run(args: &[String]) -> Result<(), String> {
    let value = zcalld_installer::cli::run(args)?;
    println!("{}", value.stringify());
    Ok(())
}

pub fn lease() -> Result<Option<zcalld_installer::RuntimeLease>, String> {
    let executable = std::env::current_exe().map_err(|_| "cannot locate the running executable")?;
    let lease = zcalld_installer::runtime_lease(&executable)?;
    if lease.as_ref().is_some_and(|lease| lease.version() != env!("CARGO_PKG_VERSION")) {
        return Err("this process loaded a different bundle version; start the installed executable again".into());
    }
    Ok(lease)
}

pub fn managed(executable: &Path) -> bool {
    zcalld_installer::managed_path(executable)
}

pub fn update(version: &str, executable: &Path) -> Result<(), String> {
    let prefix = zcalld_installer::installed_prefix(executable)?
        .map(Ok)
        .unwrap_or_else(zcalld_installer::default_prefix)?;
    let plan = zcalld_installer::plan_official(Some(version), Some(&prefix))?;
    let mut result = zcalld_installer::apply(&plan)?;
    if result.get("pending_exit").as_bool() == Some(true) {
        result = zcalld_installer::spawn_resume(Path::new(
            result.get("transaction").as_str().ok_or("missing installer transaction")?,
        ))?;
    }
    result.set("method", "verified-native-archive".into());
    println!("{}", result.stringify());
    Ok(())
}
