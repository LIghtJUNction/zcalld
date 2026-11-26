//! Agent discovery and reversible, field-level configuration changes.
mod editor;
use crate::{
    config,
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

struct Agent {
    id: &'static str,
    binary: &'static str,
    files: Vec<PathBuf>,
    format: &'static str,
}

fn config_home(context: &Context) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| context.home.join("AppData/Roaming"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| context.home.join(".config"))
    }
}
fn agents(context: &Context) -> Vec<Agent> {
    let h = &context.home;
    let x = config_home(context);
    let code = if cfg!(target_os = "macos") {
        h.join("Library/Application Support/Code/User/settings.json")
    } else {
        x.join("Code/User/settings.json")
    };
    let editor_user = code.parent().unwrap();
    vec![
        Agent {
            id: "claude",
            binary: "claude",
            files: vec![h.join(".claude/settings.json")],
            format: "claude",
        },
        Agent {
            id: "codex",
            binary: "codex",
            files: vec![
                env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| h.join(".codex"))
                    .join("config.toml"),
            ],
            format: "codex",
        },
        Agent {
            id: "gemini",
            binary: "gemini",
            files: vec![h.join(".gemini/settings.json"), h.join(".gemini/.env")],
            format: "gemini",
        },
        Agent {
            id: "opencode",
            binary: "opencode",
            files: vec![{
                let plain = x.join("opencode/opencode.json");
                let comments = x.join("opencode/opencode.jsonc");
                if comments.exists() { comments } else { plain }
            }],
            format: "opencode",
        },
        Agent {
            id: "pi",
            binary: "pi",
            files: vec![
                h.join(".pi/agent/models.json"),
                h.join(".pi/agent/settings.json"),
            ],
            format: "pi",
        },
        Agent {
            id: "qwen",
            binary: "qwen",
            files: vec![h.join(".qwen/settings.json"), h.join(".qwen/.env")],
            format: "qwen",
        },
        Agent {
            id: "aider",
            binary: "aider",
            files: vec![h.join(".aider.conf.yml")],
            format: "aider",
        },
        Agent {
            id: "goose",
            binary: "goose",
            files: vec![x.join("goose/config.yaml")],
            format: "goose",
        },
        Agent {
            id: "zed",
            binary: "zed",
            files: vec![if cfg!(target_os = "macos") {
                h.join(".config/zed/settings.json")
            } else if cfg!(target_os = "windows") {
                x.join("Zed/settings.json")
            } else {
                x.join("zed/settings.json")
            }],
            format: "zed",
        },
        Agent {
            id: "vscode",
            binary: "code",
            files: vec![code.clone(), code.with_file_name("chatLanguageModels.json")],
            format: "vscode",
        },
        Agent {
            id: "cursor",
            binary: "cursor",
            files: vec![if cfg!(target_os = "macos") {
                h.join("Library/Application Support/Cursor/User/settings.json")
            } else {
                x.join("Cursor/User/settings.json")
            }],
            format: "detect",
        },
        Agent {
            id: "amp",
            binary: "amp",
            files: vec![x.join("amp/settings.json")],
            format: "detect",
        },
        Agent {
            id: "continue",
            binary: "continue",
            files: vec![h.join(".continue/config.yaml")],
            format: "detect",
        },
        Agent {
            id: "cline",
            binary: "cline",
            files: vec![
                editor_user
                    .join("globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"),
            ],
            format: "detect",
        },
        Agent {
            id: "roo",
            binary: "roo",
            files: vec![
                editor_user
                    .join("globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json"),
            ],
            format: "detect",
        },
        Agent {
            id: "kilo",
            binary: "kilo",
            files: vec![
                editor_user.join("globalStorage/kilocode.kilo-code/settings/mcp_settings.json"),
            ],
            format: "detect",
        },
    ]
}

fn installation_candidates(
    platform: &str,
    name: &str,
    binary: &str,
    home: &Path,
    roaming: &Path,
    local: &Path,
    system_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if platform == "macos" {
        let bundles: &[(&str, &str)] = match name {
            "zed" => &[
                ("Zed.app", "Contents/MacOS/zed"),
                ("Zed Preview.app", "Contents/MacOS/zed"),
            ],
            "vscode" => &[("Visual Studio Code.app", "Contents/Resources/app/bin/code")],
            "cursor" => &[("Cursor.app", "Contents/Resources/app/bin/cursor")],
            _ => &[],
        };
        for base in std::iter::once(home.join("Applications")).chain(system_roots.iter().cloned()) {
            for (bundle, executable) in bundles {
                paths.push(base.join(bundle).join(executable));
            }
        }
        for base in [
            home.join(".local/bin"),
            home.join(".bun/bin"),
            home.join(".opencode/bin"),
            home.join(".npm-global/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ] {
            paths.push(base.join(binary));
        }
    } else if platform == "windows" {
        let installs: &[&str] = match name {
            "zed" => &[
                "Zed/Zed.exe",
                "Zed/bin/zed.exe",
                "Zed Preview/Zed.exe",
                "Zed Preview/bin/zed.exe",
            ],
            "vscode" => &["Microsoft VS Code/Code.exe"],
            "cursor" => &["cursor/Cursor.exe", "Cursor/Cursor.exe"],
            _ => &[],
        };
        for base in std::iter::once(local.join("Programs")).chain(system_roots.iter().cloned()) {
            for relative in installs {
                paths.push(base.join(relative));
            }
        }
        for base in [
            roaming.join("npm"),
            home.join(".local/bin"),
            home.join(".bun/bin"),
            home.join(".opencode/bin"),
            home.join("scoop/shims"),
            local.join("Microsoft/WinGet/Links"),
        ] {
            for extension in ["exe", "cmd", "bat"] {
                paths.push(base.join(format!("{binary}.{extension}")));
            }
        }
    }
    paths
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn agent_executable(context: &Context, agent: &Agent) -> Option<PathBuf> {
    executable(agent.binary).or_else(|| {
        let local = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| context.home.join("AppData/Local"));
        let roots = if cfg!(target_os = "macos") {
            vec![PathBuf::from("/Applications")]
        } else {
            ["ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(env::var_os)
                .map(PathBuf::from)
                .collect()
        };
        installation_candidates(
            env::consts::OS,
            agent.id,
            agent.binary,
            &context.home,
            &config_home(context),
            &local,
            &roots,
        )
        .into_iter()
        .find(|p| is_executable(p))
    })
}

pub fn executable(name: &str) -> Option<PathBuf> {
    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };
    for path in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        for extension in &extensions {
            let file = path.join(format!("{name}{extension}"));
            if file.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if fs::metadata(&file).ok()?.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                return Some(file);
            }
        }
    }
    None
}

pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" | "detect" | "sync" => list(context),
        "apply" => apply(context, args),
        "revert" => revert(context, args),
        "catalog" => catalog(context, args),
        "config" => editor::run(context, args),
        _ => Err("agents expects list, detect, sync, apply, revert, catalog or config".into()),
    }
}

// Read the same fields the built-in adapters write. An inactive provider
// or a URL in a comment must not count as a gateway connection.
fn observed_connection(agent: &Agent, url: &str) -> Option<(bool, Option<String>)> {
    let text = control::read_text(&agent.files[0]).ok()?;
    let v1 = format!("{url}/v1");
    let json_value = |text: &str| json::parse_jsonc(text).ok();
    let secondary = || control::read_text(&agent.files[1]).ok();
    let (connected, route) = match agent.format {
        "codex" => (
            scalar(&text, "", "model_provider", '=').as_deref() == Some("zcalld")
                && scalar(&text, "model_providers.zcalld", "base_url", '=').as_deref() == Some(&v1),
            scalar(&text, "", "model", '='),
        ),
        "aider" => (
            scalar(&text, "", "openai-api-base", ':').as_deref() == Some(&v1),
            scalar(&text, "", "model", ':')
                .and_then(|model| model.strip_prefix("openai/").map(str::to_owned)),
        ),
        "goose" => (
            scalar(&text, "", "GOOSE_PROVIDER", ':').as_deref() == Some("openai")
                && scalar(&text, "", "OPENAI_HOST", ':').as_deref() == Some(url),
            scalar(&text, "", "GOOSE_MODEL", ':'),
        ),
        "detect" => return None,
        format => {
            let data = json_value(&text)?;
            let model = data.get("model");
            let (connected, route) = match format {
                "claude" => {
                    let env = data.get("env");
                    (
                        env.get("ANTHROPIC_BASE_URL").as_str() == Some(url),
                        env.get("ANTHROPIC_MODEL")
                            .as_str()
                            .or_else(|| model.as_str()),
                    )
                }
                "gemini" | "qwen" => {
                    let auth = data
                        .get("security")
                        .get("auth")
                        .get("selectedType")
                        .as_str();
                    let env = secondary().unwrap_or_default();
                    let connected = if format == "gemini" {
                        auth == Some("gemini-api-key")
                            && scalar(&env, "", "GOOGLE_GEMINI_BASE_URL", '=').as_deref()
                                == Some(url)
                    } else {
                        auth == Some("openai")
                            && model.get("baseUrl").as_str() == Some(&v1)
                            && scalar(&env, "", "OPENAI_BASE_URL", '=').as_deref() == Some(&v1)
                    };
                    (connected, model.get("name").as_str())
                }
                "opencode" => {
                    let route = model
                        .as_str()
                        .and_then(|model| model.strip_prefix("zcalld/"));
                    (
                        route.is_some()
                            && data
                                .get("provider")
                                .get("zcalld")
                                .get("options")
                                .get("baseURL")
                                .as_str()
                                == Some(&v1),
                        route,
                    )
                }
                "zed" => {
                    let default = data.get("agent").get("default_model");
                    (
                        default.get("provider").as_str() == Some("zcalld")
                            && data
                                .get("language_models")
                                .get("openai_compatible")
                                .get("zcalld")
                                .get("api_url")
                                .as_str()
                                == Some(&v1),
                        default.get("model").as_str(),
                    )
                }
                "pi" => {
                    let settings = json_value(&secondary()?)?;
                    return Some((
                        settings.get("defaultProvider").as_str() == Some("zcalld")
                            && data.get("providers").get("zcalld").get("baseUrl").as_str()
                                == Some(&v1),
                        settings.get("defaultModel").as_str().map(str::to_owned),
                    ));
                }
                "vscode" => {
                    let models = json_value(&secondary()?)?;
                    let route = data.get("chat.defaultModel").as_str();
                    let endpoint = format!("{v1}/chat/completions");
                    (
                        models.as_array().iter().any(|group| {
                            group.get("name").as_str() == Some("zcalld")
                                && group.get("vendor").as_str() == Some("customendpoint")
                                && group.get("models").as_array().iter().any(|model| {
                                    route.is_some()
                                        && model.get("id").as_str() == route
                                        && model.get("url").as_str() == Some(&endpoint)
                                })
                        }),
                        route,
                    )
                }
                _ => return None,
            };
            (connected, route.map(str::to_owned))
        }
    };
    Some((connected, route))
}

fn scalar(text: &str, table: &str, key: &str, separator: char) -> Option<String> {
    let mut current = "";
    let mut found = None;
    for line in text.lines() {
        if separator == ':' && line.starts_with([' ', '\t']) {
            continue;
        }
        let line = line.trim();
        if separator == '=' && line.starts_with('[') {
            let header = &line[..inline_comment(line).unwrap_or(line.len())];
            current = header.trim().trim_matches(['[', ']']).trim();
            continue;
        }
        let Some((name, value)) = line.trim_start_matches("export ").split_once(separator) else {
            continue;
        };
        if current != table || name.trim() != key {
            continue;
        }
        if found.is_some() {
            return None;
        }
        let value = value[..inline_comment(value).unwrap_or(value.len())].trim();
        found = Some(if value.starts_with('"') {
            json::parse(value.as_bytes()).ok()?.as_str()?.to_owned()
        } else if let Some(value) = value.strip_prefix('\'') {
            value.strip_suffix('\'')?.to_owned()
        } else {
            value.to_owned()
        });
    }
    found.filter(|value| !value.is_empty())
}

fn restore_state(applied: &Value, name: &str) -> (bool, bool) {
    let Some(backup) = applied.get(name).get("backup").as_str() else {
        return (false, false);
    };
    let backup = Path::new(backup);
    let restorable = json::read(&backup.join("manifest.json"))
        .and_then(|manifest| check_applied_files(backup, &manifest))
        .is_ok();
    (restorable, !restorable)
}

fn list(context: &Context) -> Result<(), String> {
    let url = control::gateway_url(context).unwrap_or_default();
    let applied = json::read(&context.directory.join("agents/applied.json")).unwrap_or_default();
    let mut list = Vec::new();
    for agent in agents(context) {
        let binary = agent_executable(context, &agent);
        let observed = agent.files.iter().any(|p| p.is_file());
        let (configured, route) = if url.is_empty() {
            (false, None)
        } else {
            observed_connection(&agent, &url).unwrap_or_default()
        };
        let (restorable, changed) = restore_state(&applied, agent.id);
        list.push(object([
            ("name", agent.id.into()),
            ("installed", binary.is_some().into()),
            ("detected_config", observed.into()),
            ("configured", configured.into()),
            (
                "route",
                route
                    .filter(|_| configured)
                    .map_or(Value::Null, Value::from),
            ),
            ("restorable", restorable.into()),
            ("changed", changed.into()),
            ("path", agent.files[0].to_string_lossy().into_owned().into()),
            (
                "executable",
                binary
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
            ),
            ("writable", (agent.format != "detect").into()),
            ("source", "builtin".into()),
        ]));
    }
    for (name, definition) in custom_definitions(context)? {
        let binary = definition.get("binary").as_str().and_then(executable);
        let files = definition.get("files").as_array();
        let files: Vec<_> = files
            .iter()
            .filter_map(|file| {
                let path = under_home(context, file.get("path").as_str()?).ok()?;
                Some((path, file.get("bindings").as_array()))
            })
            .collect();
        let configured = !url.is_empty()
            && files.iter().any(|(path, bindings)| {
                let Some(data) = control::read_text(path)
                    .ok()
                    .and_then(|text| json::parse_jsonc(&text).ok())
                else {
                    return false;
                };
                bindings.iter().any(|binding| {
                    let Some(template) = binding.get("value").as_str() else {
                        return false;
                    };
                    if !template.contains("${gateway}")
                        && !template.contains("${gateway_v1}")
                        && template != url
                        && template != format!("{url}/v1")
                    {
                        return false;
                    }
                    let actual = binding
                        .get("path")
                        .as_array()
                        .iter()
                        .try_fold(&data, |data, key| Some(data.get(key.as_str()?)));
                    let expected = template
                        .replace("${gateway}", &url)
                        .replace("${gateway_v1}", &format!("{url}/v1"));
                    actual.and_then(Value::as_str) == Some(expected.as_str())
                })
            });
        let (restorable, changed) = restore_state(&applied, &name);
        let route = if configured && restorable {
            applied.get(&name).get("route").clone()
        } else {
            Value::Null
        };
        list.push(object([
            ("name", name.into()),
            ("installed", binary.is_some().into()),
            (
                "detected_config",
                files.iter().any(|(path, _)| path.is_file()).into(),
            ),
            ("configured", configured.into()),
            ("route", route),
            ("restorable", restorable.into()),
            ("changed", changed.into()),
            (
                "path",
                files
                    .first()
                    .map(|(path, _)| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
            ),
            (
                "executable",
                binary
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
            ),
            ("writable", true.into()),
            ("source", "community".into()),
        ]));
    }
    control::emit(object([
        ("agents", Value::Array(list)),
        ("platform", env::consts::OS.into()),
    ]))
}

fn read_or(path: &Path, fallback: &str) -> Result<String, String> {
    if path.exists() {
        control::read_text(path)
    } else {
        Ok(fallback.into())
    }
}
fn patch(text: &str, path: &[&str], value: impl Into<Value>) -> Result<String, String> {
    json::patch_json(text, path, &value.into())
}

/// TOML edits deliberately touch only a single-line scalar. Ambiguous or multiline
/// existing values fail before any file is changed; users' tables stay byte-for-byte.
pub fn toml_set(text: &str, table: &str, key: &str, value: &str) -> Result<String, String> {
    toml_set_literal(text, table, key, &crate::http::quote(value))
}
fn toml_set_literal(text: &str, table: &str, key: &str, value: &str) -> Result<String, String> {
    if key.is_empty() || key.chars().any(|c| c.is_control() || "=\"'[]".contains(c)) {
        return Err("invalid TOML key".into());
    }
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut current = String::new();
    let mut found = None;
    let mut insertion = if table.is_empty() { Some(0) } else { None };
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if current == table && insertion.is_some() {
                insertion = Some(i);
            }
            current = trimmed.trim_matches(['[', ']']).trim().into();
            if current == table {
                insertion = Some(i + 1);
            }
            continue;
        }
        if current == table {
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == key
            {
                if found.is_some()
                    || (v.trim_start().starts_with("\"\"\"") || v.trim_start().starts_with("'''"))
                {
                    return Err("ambiguous or multiline TOML value; no files changed".into());
                }
                found = Some(i);
            }
            if insertion.is_some() {
                insertion = Some(i + 1);
            }
        }
    }
    let new = format!("{key} = {value}");
    if let Some(i) = found {
        let previous = lines[i].split_once('=').unwrap().1;
        lines[i] = match inline_comment(previous) {
            Some(offset) => format!("{new} {}", &previous[offset..]),
            None => new,
        };
    } else if let Some(i) = insertion {
        lines.insert(i, new);
    } else {
        lines.push(String::new());
        lines.push(format!("[{table}]"));
        lines.push(new);
    }
    Ok(lines.join("\n") + "\n")
}
fn inline_comment(value: &str) -> Option<usize> {
    let mut quote = None;
    let mut escape = false;
    for (i, c) in value.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if quote == Some('"') && c == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '#' {
            return Some(i);
        }
    }
    None
}
fn env_set(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<_> = text
        .lines()
        .filter(|l| {
            !l.trim_start()
                .trim_start_matches("export ")
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == key)
        })
        .map(String::from)
        .collect();
    lines.push(format!("{key}={}", crate::http::quote(value)));
    lines.join("\n") + "\n"
}
fn yaml_scalar(text: &str, key: &str, value: &str) -> Result<String, String> {
    let mut lines: Vec<_> = text.lines().map(String::from).collect();
    let matches: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.split_once(':').is_some_and(|(k, _)| k == key))
        .map(|(i, _)| i)
        .collect();
    if matches.len() > 1 {
        return Err("duplicate YAML key; no files changed".into());
    }
    let entry = format!("{key}: {}", crate::http::quote(value));
    if let Some(i) = matches.first() {
        if lines.get(i + 1).is_some_and(|l| {
            l.starts_with([' ', '\t']) && !l.trim_start().starts_with('#') && !l.trim().is_empty()
        }) {
            return Err("cannot replace a nested YAML mapping with a scalar".into());
        }
        lines[*i] = entry;
    } else {
        lines.push(entry);
    }
    Ok(lines.join("\n") + "\n")
}

/// Zed keys use the complete API URL and the "Bearer" account. These spellings
/// match upstream Zed d5615304 (gpui_apple/keychain, gpui_linux/platform and
/// gpui_windows/util); no credential is placed in command arguments or logs.
trait CredentialStore {
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn write(&self, key: &str, secret: &[u8]) -> Result<(), String>;
    fn remove(&self, key: &str) -> Result<(), String>;
}

struct SystemCredentialStore {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    command: PathBuf,
    #[cfg(target_os = "macos")]
    app: PathBuf,
}
impl SystemCredentialStore {
    fn new(context: &Context) -> Result<Self, String> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            let _ = context;
            let command = executable("secret-tool").ok_or(
                "Zed requires secret-tool (libsecret) and an unlocked Secret Service; no agent configuration was changed",
            )?;
            Ok(Self { command })
        }
        #[cfg(target_os = "macos")]
        {
            let agent = agents(context).into_iter().find(|a| a.id == "zed").unwrap();
            let binary = agent_executable(context, &agent)
                .and_then(|p| fs::canonicalize(p).ok())
                .ok_or("cannot locate the installed Zed app to authorize its Keychain access")?;
            let app = binary
                .ancestors()
                .find(|p| p.extension().is_some_and(|v| v == "app"))
                .ok_or("Zed executable is not inside an installed app bundle")?
                .to_path_buf();
            Ok(Self { app })
        }
        #[cfg(target_os = "windows")]
        {
            let _ = context;
            Ok(Self {})
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "windows"
        )))]
        {
            let _ = context;
            Err("Zed system credential storage is unsupported on this platform".into())
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
fn credential_command(
    program: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<std::process::Output, String> {
    use std::{
        io::{Read, Write},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "cannot start the system credential utility".to_string())?;
    fn collect(mut stream: impl Read) -> Vec<u8> {
        let mut bytes = Vec::new();
        let _ = stream.by_ref().take(65537).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
        bytes
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || collect(stdout));
    let err = std::thread::spawn(move || collect(stderr));
    if let Some(input) = input {
        let written = child.stdin.take().unwrap().write_all(input);
        if written.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            return Err("system credential utility rejected its private input".into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out.join();
                let _ = err.join();
                return Err("system credential store did not respond within 30 seconds; unlock the store and retry".into());
            }
        }
    };
    let stdout = out.join().map_err(|_| "credential output reader failed")?;
    let stderr = err.join().map_err(|_| "credential error reader failed")?;
    if stdout.len() > 65536 || stderr.len() > 65536 {
        return Err("system credential utility returned excessive output".into());
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
fn credential_bytes(mut bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.len() > 2560 || bytes.contains(&0) || bytes.contains(&b'\n') || bytes.contains(&b'\r')
    {
        return Err("Zed credential is not a bounded single-line API key".into());
    }
    Ok(bytes)
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl CredentialStore for SystemCredentialStore {
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let out = credential_command(
            &self.command,
            &["lookup", "url", key, "username", "Bearer"],
            None,
        )?;
        if out.status.success() {
            return credential_bytes(out.stdout).map(Some);
        }
        if out.status.code() == Some(1) && out.stderr.is_empty() {
            return Ok(None);
        }
        Err("Zed Secret Service lookup failed; unlock the system credential store and retry".into())
    }
    fn write(&self, key: &str, secret: &[u8]) -> Result<(), String> {
        let out = credential_command(
            &self.command,
            &[
                "store",
                "--label=zed-github-account",
                "url",
                key,
                "username",
                "Bearer",
            ],
            Some(secret),
        )?;
        if out.status.success() {
            Ok(())
        } else {
            Err("Zed Secret Service write failed; no successful apply was recorded".into())
        }
    }
    fn remove(&self, key: &str) -> Result<(), String> {
        let out = credential_command(
            &self.command,
            &["clear", "url", key, "username", "Bearer"],
            None,
        )?;
        if out.status.success() || (out.status.code() == Some(1) && out.stderr.is_empty()) {
            Ok(())
        } else {
            Err("Zed Secret Service removal failed".into())
        }
    }
}

#[cfg(target_os = "macos")]
fn security_word(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("Keychain command value contains a control character".into());
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
#[cfg(target_os = "macos")]
impl CredentialStore for SystemCredentialStore {
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let out = credential_command(
            Path::new("/usr/bin/security"),
            &["find-internet-password", "-s", key, "-a", "Bearer", "-w"],
            None,
        )?;
        if out.status.success() {
            return credential_bytes(out.stdout).map(Some);
        }
        if out.status.code() == Some(44) {
            return Ok(None);
        } // errSecItemNotFound
        Err("Zed Keychain lookup failed; unlock or authorize the login Keychain and retry".into())
    }
    fn write(&self, key: &str, secret: &[u8]) -> Result<(), String> {
        let secret = std::str::from_utf8(secret).map_err(|_| "Zed API key is not UTF-8")?;
        // Interactive security input keeps the secret out of the process command line.
        // Read-back verification below catches an interactive subcommand failure.
        let script = format!(
            "add-internet-password -U -T {} -T \"/usr/bin/security\" -s {} -a \"Bearer\" -w {}\n",
            security_word(&self.app.to_string_lossy())?,
            security_word(key)?,
            security_word(secret)?,
        );
        if script.len() >= 4096 {
            return Err("Zed Keychain command exceeds the platform input limit".into());
        }
        let out = credential_command(
            Path::new("/usr/bin/security"),
            &["-i"],
            Some(script.as_bytes()),
        )?;
        if !out.status.success() || self.read(key)?.as_deref() != Some(secret.as_bytes()) {
            return Err(
                "Zed Keychain write could not be verified; authorize Keychain access and retry"
                    .into(),
            );
        }
        Ok(())
    }
    fn remove(&self, key: &str) -> Result<(), String> {
        let out = credential_command(
            Path::new("/usr/bin/security"),
            &["delete-internet-password", "-s", key, "-a", "Bearer"],
            None,
        )?;
        if out.status.success() || out.status.code() == Some(44) {
            Ok(())
        } else {
            Err("Zed Keychain removal failed".into())
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_credentials {
    use std::{ffi::c_void, ptr};
    #[repr(C)]
    struct Credential {
        flags: u32,
        kind: u32,
        target: *mut u16,
        comment: *mut u16,
        written: [u32; 2],
        size: u32,
        blob: *mut u8,
        persist: u32,
        count: u32,
        attributes: *mut c_void,
        alias: *mut u16,
        username: *mut u16,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredReadW(
            target: *const u16,
            kind: u32,
            flags: u32,
            result: *mut *mut Credential,
        ) -> i32;
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *mut c_void);
    }
    fn wide(text: &str) -> Result<Vec<u16>, String> {
        if text.contains('\0') {
            return Err("invalid credential target".into());
        }
        Ok(text.encode_utf16().chain([0]).collect())
    }
    fn error(action: &str) -> String {
        format!(
            "Windows Credential Manager {action} failed (OS error {})",
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default()
        )
    }
    pub fn read(key: &str) -> Result<Option<Vec<u8>>, String> {
        let target = wide(&format!("zed:url={key}"))?;
        let mut raw = ptr::null_mut();
        // SAFETY: target is NUL terminated and the output pointer is valid.
        if unsafe { CredReadW(target.as_ptr(), 1, 0, &mut raw) } == 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(1168) {
                return Ok(None);
            }
            return Err(error("lookup"));
        }
        if raw.is_null() {
            return Err("Windows Credential Manager returned an empty result".into());
        }
        // SAFETY: successful CredReadW owns this allocation until CredFree.
        let result = unsafe {
            let value = &*raw;
            let username = wide("Bearer")?;
            let expected = username.as_slice();
            let valid_user = !value.username.is_null()
                && expected
                    .iter()
                    .enumerate()
                    .all(|(i, unit)| *value.username.add(i) == *unit);
            if !valid_user {
                Err("existing Zed credential uses another account; it was preserved".into())
            } else if value.size > 2560 || (value.size > 0 && value.blob.is_null()) {
                Err("Windows Credential Manager returned an invalid API key".into())
            } else if value.size == 0 {
                Ok(Some(Vec::new()))
            } else {
                Ok(Some(
                    std::slice::from_raw_parts(value.blob, value.size as usize).to_vec(),
                ))
            }
        };
        // SAFETY: raw came from CredReadW and is released exactly once.
        unsafe { CredFree(raw.cast()) };
        result
    }
    pub fn write(key: &str, secret: &[u8]) -> Result<(), String> {
        if secret.len() > 2560 {
            return Err("API key exceeds Windows Credential Manager's limit".into());
        }
        let mut target = wide(&format!("zed:url={key}"))?;
        let mut user = wide("Bearer")?;
        let value = Credential {
            flags: 0,
            kind: 1,
            target: target.as_mut_ptr(),
            comment: ptr::null_mut(),
            written: [0; 2],
            size: secret.len() as u32,
            blob: secret.as_ptr().cast_mut(),
            persist: 2,
            count: 0,
            attributes: ptr::null_mut(),
            alias: ptr::null_mut(),
            username: user.as_mut_ptr(),
        };
        // SAFETY: all referenced buffers remain valid during the synchronous OS call.
        if unsafe { CredWriteW(&value, 0) } == 0 {
            Err(error("write"))
        } else {
            Ok(())
        }
    }
    pub fn remove(key: &str) -> Result<(), String> {
        let target = wide(&format!("zed:url={key}"))?;
        // SAFETY: target is NUL terminated for the duration of the call.
        if unsafe { CredDeleteW(target.as_ptr(), 1, 0) } != 0
            || std::io::Error::last_os_error().raw_os_error() == Some(1168)
        {
            Ok(())
        } else {
            Err(error("removal"))
        }
    }
}
#[cfg(target_os = "windows")]
impl CredentialStore for SystemCredentialStore {
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        windows_credentials::read(key)
    }
    fn write(&self, key: &str, secret: &[u8]) -> Result<(), String> {
        windows_credentials::write(key, secret)
    }
    fn remove(&self, key: &str) -> Result<(), String> {
        windows_credentials::remove(key)
    }
}
#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos",
    target_os = "windows"
)))]
impl CredentialStore for SystemCredentialStore {
    fn read(&self, _: &str) -> Result<Option<Vec<u8>>, String> {
        Err("unsupported system credential store".into())
    }
    fn write(&self, _: &str, _: &[u8]) -> Result<(), String> {
        Err("unsupported system credential store".into())
    }
    fn remove(&self, _: &str) -> Result<(), String> {
        Err("unsupported system credential store".into())
    }
}

fn verify_credential(store: &dyn CredentialStore, key: &str, secret: &[u8]) -> Result<(), String> {
    store.write(key, secret)?;
    if store.read(key)?.as_deref() != Some(secret) {
        return Err("system credential write did not read back correctly".into());
    }
    Ok(())
}
fn credential_record(
    store: &dyn CredentialStore,
    saved: &Path,
    key: &str,
    secret: &[u8],
) -> Result<Value, String> {
    if secret.is_empty() || secret.len() > 2560 || secret.iter().any(|b| b.is_ascii_control()) {
        return Err("Zed requires a nonempty, bounded API key".into());
    }
    let previous = store.read(key)?;
    let id = saved
        .file_name()
        .ok_or("invalid agent backup directory")?
        .to_string_lossy();
    let old = format!("zcalld:agent-backup:{id}:previous");
    let expected = format!("zcalld:agent-backup:{id}:applied");
    let prepare = (|| {
        verify_credential(store, &expected, secret)?;
        if let Some(previous) = &previous {
            verify_credential(store, &old, previous)?;
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = prepare {
        let _ = store.remove(&old);
        let _ = store.remove(&expected);
        return Err(error);
    }
    Ok(object([
        ("url", key.into()),
        ("previous", old.into()),
        ("applied", expected.into()),
        ("had_previous", previous.is_some().into()),
    ]))
}
fn restore_credential(
    store: &dyn CredentialStore,
    record: &Value,
    check: bool,
) -> Result<(), String> {
    let key = record
        .get("url")
        .as_str()
        .ok_or("invalid credential backup")?;
    let old = record
        .get("previous")
        .as_str()
        .ok_or("invalid credential backup")?;
    let applied = record
        .get("applied")
        .as_str()
        .ok_or("invalid credential backup")?;
    if check {
        let expected = store
            .read(applied)?
            .ok_or("applied credential checkpoint is missing; backup retained")?;
        if store.read(key)?.as_deref() != Some(expected.as_slice()) {
            return Err(
                "Zed credential changed after apply; review it before reverting, or use --force"
                    .into(),
            );
        }
    }
    if record.get("had_previous").as_bool() == Some(true) {
        let previous = store
            .read(old)?
            .ok_or("previous credential checkpoint is missing; backup retained")?;
        verify_credential(store, key, &previous)?;
    } else {
        store.remove(key)?;
        if store.read(key)?.is_some() {
            return Err("Zed credential removal could not be verified".into());
        }
    }
    Ok(())
}
fn clear_credential_checkpoint(store: &dyn CredentialStore, record: &Value) -> Result<(), String> {
    for key in ["previous", "applied"] {
        if let Some(value) = record.get(key).as_str() {
            store.remove(value)?;
        }
    }
    Ok(())
}

fn commit_agent_change(
    context: &Context,
    name: &str,
    route: &str,
    changes: &[(PathBuf, String)],
    credential: Option<(&dyn CredentialStore, &str, &[u8])>,
) -> Result<PathBuf, String> {
    let paths: Vec<_> = changes.iter().map(|(p, _)| p.clone()).collect();
    let saved = control::backup(context, &paths, &format!("agent-{name}"))?;
    let applied = context.directory.join("agents/applied.json");
    let previous_state = if applied.exists() {
        Some(fs::read(&applied).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let mut state = previous_state
        .as_deref()
        .map(json::parse)
        .transpose()?
        .unwrap_or_default();
    state.set(
        name,
        object([
            ("route", route.into()),
            ("backup", saved.to_string_lossy().into_owned().into()),
        ]),
    );
    let mut record = json::read(&saved.join("manifest.json"))?;
    for (i, (_, text)) in changes.iter().enumerate() {
        json::atomic_write(&saved.join(format!("{i}.applied")), text.as_bytes())?;
    }
    record.set("route", route.into());
    if let Some((store, key, secret)) = credential {
        record.set("credential", credential_record(store, &saved, key, secret)?);
    }
    if let Err(error) =
        json::atomic_write(&saved.join("manifest.json"), record.stringify().as_bytes())
    {
        if let Some((store, _, _)) = credential {
            let _ = clear_credential_checkpoint(store, record.get("credential"));
        }
        return Err(error);
    }
    let change = (|| {
        if let Some((store, key, secret)) = credential {
            verify_credential(store, key, secret)?;
        }
        for (path, text) in changes {
            json::atomic_write(path, text.as_bytes())?;
        }
        json::atomic_write(&applied, state.stringify().as_bytes())
    })();
    if let Err(error) = change {
        let store = credential.map(|(store, _, _)| store);
        let restored = restore_manifest(&saved, false, store);
        let state_restored = if let Some(previous) = &previous_state {
            json::atomic_write(&applied, previous)
        } else if applied.exists() {
            fs::remove_file(&applied).map_err(|e| e.to_string())
        } else {
            Ok(())
        };
        if let Err(state_error) = state_restored {
            return Err(format!(
                "{error}; applied-state rollback requires recovery: {state_error}; backup {}",
                saved.display()
            ));
        }
        return Err(match restored {
            Ok(()) => format!("{error}; previous agent configuration and credential restored"),
            Err(restore) => format!(
                "{error}; rollback requires recovery: {restore}; backup {}",
                saved.display()
            ),
        });
    }
    Ok(saved)
}

fn automatic_route<'a>(
    config: &'a config::Config,
    agent: Option<&Agent>,
    url: &str,
) -> Option<&'a str> {
    use crate::protocol::Protocol;
    let current = agent.and_then(|agent| observed_connection(agent, url)?.1);
    let automatic = current.as_ref().map(|model| format!("auto/{model}"));
    let source = match agent.map(|agent| agent.format) {
        Some("codex") => Protocol::OpenAiResponses,
        Some("claude") => Protocol::AnthropicMessages,
        Some("gemini") => Protocol::GeminiGenerateContent,
        _ => Protocol::OpenAiChat,
    };
    config
        .routes
        .iter()
        .filter_map(|route| {
            let (protocol, _, _) = config.providers[route.provider].for_protocol(source);
            if protocol.is_decision() {
                return None;
            }
            let score = if current.as_deref() == Some(route.name.as_str()) {
                0
            } else if automatic.as_deref() == Some(route.name.as_str()) {
                1
            } else if current.as_deref() == Some(route.model.as_str()) {
                2
            } else {
                3
            };
            Some(((score, protocol != source), route.name.as_str()))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, route)| route)
}

fn apply(context: &Context, args: &[String]) -> Result<(), String> {
    let name = args.get(1).ok_or("agent name required")?;
    let config = config::Config::load(&context.path)?;
    let agent = agents(context).into_iter().find(|agent| agent.id == name);
    let url = control::gateway_url(context)?;
    let route = control::option(args, "--route")?
        .or_else(|| automatic_route(&config, agent.as_ref(), &url))
        .ok_or("add a generation model route before connecting this tool")?
        .to_owned();
    if !config.routes.iter().any(|r| r.name == route) {
        return Err("route is not in the configured model catalog".into());
    }
    let v1 = format!("{url}/v1");
    let token = control::gateway_token(context)?;
    let mut changes = Vec::new();
    let mut credential_key = None;
    if let Some(agent) = agent {
        let first = &agent.files[0];
        let text = read_or(
            first,
            if matches!(agent.format, "codex" | "aider" | "goose") {
                ""
            } else {
                "{}\n"
            },
        )?;
        let next = match agent.format {
            "claude" => {
                let s = patch(&text, &["env", "ANTHROPIC_BASE_URL"], url.clone())?;
                let s = patch(&s, &["env", "ANTHROPIC_AUTH_TOKEN"], token.clone())?;
                let s = patch(&s, &["env", "ANTHROPIC_MODEL"], route.clone())?;
                patch(&s, &["model"], route.clone())?
            }
            "codex" => {
                let s = toml_set(&text, "", "model", &route)?;
                let s = toml_set(&s, "", "model_provider", "zcalld")?;
                let s = toml_set(&s, "model_providers.zcalld", "name", "zcalld")?;
                let s = toml_set(&s, "model_providers.zcalld", "base_url", &v1)?;
                let s = toml_set(&s, "model_providers.zcalld", "wire_api", "responses")?;
                let s = toml_set(
                    &s,
                    "model_providers.zcalld",
                    "experimental_bearer_token",
                    &token,
                )?;
                toml_set_literal(
                    &s,
                    "model_providers.zcalld",
                    "requires_openai_auth",
                    "false",
                )?
            }
            "gemini" | "qwen" => {
                let is_qwen = agent.format == "qwen";
                let s = patch(
                    &text,
                    &["security", "auth", "selectedType"],
                    if is_qwen { "openai" } else { "gemini-api-key" },
                )?;
                let s = patch(&s, &["model", "name"], route.clone())?;
                let envtext = read_or(&agent.files[1], "")?;
                let envtext = env_set(
                    &envtext,
                    if is_qwen {
                        "OPENAI_BASE_URL"
                    } else {
                        "GOOGLE_GEMINI_BASE_URL"
                    },
                    if is_qwen { &v1 } else { &url },
                );
                let envtext = env_set(
                    &envtext,
                    if is_qwen {
                        "ZCALLD_GATEWAY_KEY"
                    } else {
                        "GEMINI_API_KEY"
                    },
                    &token,
                );
                changes.push((agent.files[1].clone(), envtext));
                if is_qwen {
                    let s = patch(&s, &["model", "baseUrl"], v1.clone())?;
                    let entry = object([
                        ("id", route.clone().into()),
                        ("name", route.clone().into()),
                        ("baseUrl", v1.clone().into()),
                        ("envKey", "ZCALLD_GATEWAY_KEY".into()),
                    ]);
                    json::upsert_array(
                        &s,
                        &["modelProviders", "openai"],
                        "envKey",
                        "ZCALLD_GATEWAY_KEY",
                        &entry,
                    )?
                } else {
                    s
                }
            }
            "opencode" => {
                let provider = object([
                    ("npm", "@ai-sdk/openai-compatible".into()),
                    ("name", "zcalld".into()),
                    (
                        "options",
                        object([
                            ("baseURL", v1.clone().into()),
                            ("apiKey", token.clone().into()),
                        ]),
                    ),
                    (
                        "models",
                        Value::Object(
                            config
                                .routes
                                .iter()
                                .map(|r| {
                                    (r.name.clone(), object([("name", r.name.clone().into())]))
                                })
                                .collect(),
                        ),
                    ),
                ]);
                let s = patch(&text, &["provider", "zcalld"], provider)?;
                patch(&s, &["model"], format!("zcalld/{route}"))?
            }
            "pi" => {
                let models = config
                    .routes
                    .iter()
                    .map(|r| {
                        let mut model = object([
                            ("id", r.name.clone().into()),
                            ("name", r.name.clone().into()),
                            ("reasoning", true.into()),
                            ("input", Value::Array(vec!["text".into(), "image".into()])),
                            ("contextWindow", 200000u64.into()),
                            ("maxTokens", 32768u64.into()),
                        ]);
                        let (native, _, _) = config.providers[r.provider]
                            .for_protocol(crate::protocol::Protocol::AnthropicMessages);
                        if native == crate::protocol::Protocol::AnthropicMessages {
                            model.set("api", "anthropic-messages".into());
                            model.set("baseUrl", url.clone().into());
                            let upstream = r.model.replace('.', "-");
                            if upstream == "claude-opus-4-7"
                                || upstream.starts_with("claude-opus-4-7-")
                            {
                                model.set(
                                    "compat",
                                    object([("forceAdaptiveThinking", true.into())]),
                                );
                            }
                        }
                        model
                    })
                    .collect();
                let s = patch(
                    &text,
                    &["providers", "zcalld"],
                    object([
                        ("baseUrl", v1.clone().into()),
                        ("apiKey", token.clone().into()),
                        ("api", "openai-completions".into()),
                        ("models", Value::Array(models)),
                    ]),
                )?;
                let settings = read_or(&agent.files[1], "{}\n")?;
                let settings = patch(&settings, &["defaultProvider"], "zcalld")?;
                let mut settings = patch(&settings, &["defaultModel"], route.clone())?;
                let previous = json::parse_jsonc(&settings)?;
                let mut scope = previous.get("enabledModels").as_array().to_vec();
                let chosen = format!("zcalld/{route}");
                if !scope.is_empty() && !scope.iter().any(|v| v.as_str() == Some(&chosen)) {
                    scope.push(chosen.into());
                    settings = patch(&settings, &["enabledModels"], Value::Array(scope))?;
                }
                changes.push((agent.files[1].clone(), settings));
                s
            }
            "aider" => {
                let s = yaml_scalar(&text, "openai-api-base", &v1)?;
                let s = yaml_scalar(&s, "openai-api-key", &token)?;
                yaml_scalar(&s, "model", &format!("openai/{route}"))?
            }
            "goose" => {
                let s = yaml_scalar(&text, "GOOSE_PROVIDER", "openai")?;
                let s = yaml_scalar(&s, "GOOSE_MODEL", &route)?;
                let s = yaml_scalar(&s, "OPENAI_HOST", &url)?;
                yaml_scalar(&s, "OPENAI_API_KEY", &token)?
            }
            "zed" => {
                credential_key = Some(v1.clone());
                let available = Value::Array(
                    config
                        .routes
                        .iter()
                        .map(|r| {
                            object([
                                ("name", r.name.clone().into()),
                                ("display_name", r.name.clone().into()),
                                ("max_tokens", 200000u64.into()),
                            ])
                        })
                        .collect(),
                );
                let s = patch(
                    &text,
                    &["language_models", "openai_compatible", "zcalld"],
                    object([
                        ("api_url", v1.clone().into()),
                        ("available_models", available),
                    ]),
                )?;
                patch(
                    &s,
                    &["agent", "default_model"],
                    object([
                        ("provider", "zcalld".into()),
                        ("model", route.clone().into()),
                    ]),
                )?
            }
            "vscode" => {
                let models = Value::Array(
                    config
                        .routes
                        .iter()
                        .map(|r| {
                            object([
                                ("id", r.name.clone().into()),
                                ("name", r.name.clone().into()),
                                ("url", format!("{v1}/chat/completions").into()),
                                ("toolCalling", true.into()),
                                ("vision", false.into()),
                                ("maxInputTokens", 32768u64.into()),
                                ("maxOutputTokens", 4096u64.into()),
                                (
                                    "requestHeaders",
                                    object([("Authorization", format!("Bearer {token}").into())]),
                                ),
                            ])
                        })
                        .collect(),
                );
                let group = object([
                    ("vendor", "customendpoint".into()),
                    ("name", "zcalld".into()),
                    ("models", models),
                ]);
                let previous = read_or(&agent.files[1], "[]\n")?;
                let models = json::upsert_array(&previous, &[], "name", "zcalld", &group)?;
                changes.push((agent.files[1].clone(), models));
                patch(&text, &["chat.defaultModel"], route.clone())?
            }
            _ => {
                return Err(format!(
                    "{name} was detected, but its credentials require its native settings UI or a community agent adapter; existing routing is preserved"
                ));
            }
        };
        changes.push((first.clone(), next));
    } else {
        let definitions = custom_definitions(context)?;
        let definition = definitions.get(name).ok_or("unknown agent")?;
        for file in definition.get("files").as_array() {
            let path = under_home(
                context,
                file.get("path").as_str().ok_or("adapter file needs path")?,
            )?;
            let mut text = read_or(&path, "{}\n")?;
            for binding in file.get("bindings").as_array() {
                let keys: Vec<_> = binding
                    .get("path")
                    .as_array()
                    .iter()
                    .map(|v| v.as_str().ok_or("binding path components must be strings"))
                    .collect::<Result<_, _>>()?;
                let value = binding
                    .get("value")
                    .as_str()
                    .ok_or("binding value must be a string")?
                    .replace("${gateway}", &url)
                    .replace("${gateway_v1}", &v1)
                    .replace("${token}", &token)
                    .replace("${route}", &route);
                text = patch(&text, &keys, value)?;
            }
            changes.push((path, text));
        }
        if changes.is_empty() {
            return Err("community agent has no configuration bindings".into());
        }
    }
    let store = if credential_key.is_some() {
        Some(SystemCredentialStore::new(context)?)
    } else {
        None
    };
    let credential = store
        .as_ref()
        .zip(credential_key.as_deref())
        .map(|(store, key)| (store as &dyn CredentialStore, key, token.as_bytes()));
    let saved = commit_agent_change(context, name, &route, &changes, credential)?;
    control::emit(object([
        ("ok", true.into()),
        ("agent", name.clone().into()),
        ("route", route.into()),
        ("files", changes.len().into()),
        ("backup", saved.to_string_lossy().into_owned().into()),
    ]))
}

fn check_applied_files(directory: &Path, manifest: &Value) -> Result<(), String> {
    for (i, record) in manifest.get("files").as_array().iter().enumerate() {
        let path = Path::new(record.get("path").as_str().ok_or("invalid backup path")?);
        let expected =
            fs::read(directory.join(format!("{i}.applied"))).map_err(|e| e.to_string())?;
        if fs::read(path).map_err(|e| e.to_string())? != expected {
            return Err("agent file changed after apply; review it before reverting, or use --force to restore exact backup".into());
        }
    }
    Ok(())
}

fn restore_manifest(
    directory: &Path,
    check: bool,
    store: Option<&dyn CredentialStore>,
) -> Result<(), String> {
    let manifest = json::read(&directory.join("manifest.json"))?;
    if check {
        check_applied_files(directory, &manifest)?;
    }
    let credential = manifest.get("credential");
    if credential.get("url").as_str().is_some() {
        restore_credential(
            store.ok_or("Zed credential store is required to restore this change")?,
            credential,
            check,
        )?;
    }
    for record in manifest.get("files").as_array() {
        let path = Path::new(record.get("path").as_str().ok_or("invalid backup path")?);
        if record.get("existed").as_bool() == Some(true) {
            let original = fs::read(record.get("backup").as_str().ok_or("missing backup")?)
                .map_err(|e| e.to_string())?;
            json::atomic_write(path, &original)?;
        } else if path.exists() {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    if credential.get("url").as_str().is_some() {
        clear_credential_checkpoint(store.unwrap(), credential)?;
    }
    Ok(())
}
fn revert(context: &Context, args: &[String]) -> Result<(), String> {
    let name = args
        .get(1)
        .filter(|s| control::safe_id(s))
        .ok_or("safe agent name required")?;
    let path = context.directory.join("agents/applied.json");
    let mut state = match fs::read(&path) {
        Ok(bytes) => json::parse(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("agent has no managed change to revert".into());
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let backup = state
        .get(name)
        .get("backup")
        .as_str()
        .ok_or("agent has no managed change to revert")?
        .to_string();
    let manifest = json::read(&Path::new(&backup).join("manifest.json"))?;
    let store = if manifest.get("credential").get("url").as_str().is_some() {
        Some(SystemCredentialStore::new(context)?)
    } else {
        None
    };
    restore_manifest(
        Path::new(&backup),
        !control::flag(args, "--force"),
        store.as_ref().map(|s| s as &dyn CredentialStore),
    )?;
    state.remove(name);
    json::atomic_write(&path, state.stringify().as_bytes())?;
    control::emit(object([
        ("ok", true.into()),
        ("agent", name.clone().into()),
    ]))
}

fn under_home(context: &Context, path: &str) -> Result<PathBuf, String> {
    let relative = path.strip_prefix("~/").unwrap_or(path);
    let p = Path::new(relative);
    if p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err("community adapter paths must stay under the user's home directory".into());
    }
    Ok(context.home.join(p))
}
fn custom_definitions(context: &Context) -> Result<BTreeMap<String, Value>, String> {
    let mut out = BTreeMap::new();
    let base = context.directory.join("agents.d");
    if !base.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(base).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|v| v == "json") {
            let definition = json::read(&path)?;
            let name = definition
                .get("name")
                .as_str()
                .filter(|v| control::safe_id(v))
                .ok_or("community agent needs a safe name")?;
            if agents(context).iter().any(|a| a.id == name) {
                return Err("community agent must not silently replace a built-in adapter".into());
            }
            out.insert(name.into(), definition);
        }
    }
    Ok(out)
}
fn catalog(context: &Context, args: &[String]) -> Result<(), String> {
    let path = control::option(args, "--file")?
        .ok_or("agents catalog requires --file for a community JSON adapter")?;
    let definition = json::read(Path::new(path))?;
    let name = definition
        .get("name")
        .as_str()
        .filter(|v| control::safe_id(v))
        .ok_or("adapter needs safe name")?;
    for file in definition.get("files").as_array() {
        under_home(
            context,
            file.get("path")
                .as_str()
                .ok_or("adapter file path missing")?,
        )?;
    }
    if definition.get("files").as_array().is_empty() {
        return Err("adapter must define at least one configuration file".into());
    }
    json::atomic_write(
        &context
            .directory
            .join("agents.d")
            .join(format!("{name}.json")),
        definition.stringify().as_bytes(),
    )?;
    control::emit(object([("ok", true.into()), ("name", name.into())]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toml_keeps_tables_and_comments() {
        let src = "# hello\nmodel = \"old#value\" # keep\n[project.\"a\"]\ntrust = \"yes\"\n";
        let out = toml_set(src, "", "model", "new").unwrap();
        assert!(out.contains("model = \"new\" # keep"));
        assert!(out.contains("[project.\"a\"]\ntrust = \"yes\""));
        let out = toml_set(
            &out,
            "model_providers.zcalld",
            "base_url",
            "http://localhost/v1",
        )
        .unwrap();
        assert!(out.ends_with("base_url = \"http://localhost/v1\"\n"));
    }
    #[test]
    fn toml_rejects_duplicate_scalar() {
        assert!(toml_set("model=\"a\"\nmodel=\"b\"\n", "", "model", "c").is_err());
    }
    #[test]
    fn dotenv_keeps_other_secrets() {
        assert_eq!(
            env_set("# keep\nOTHER='secret'\nexport KEY=old\n", "KEY", "new"),
            "# keep\nOTHER='secret'\nKEY=\"new\"\n"
        );
    }
    #[test]
    fn yaml_does_not_flatten_mappings() {
        assert!(yaml_scalar("models:\n  a: b\n", "models", "x").is_err());
    }

    fn fixture() -> (Context, PathBuf) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let path = env::temp_dir().join(format!(
            "zcalld-adapter-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        let context = Context {
            home: path.join("home"),
            directory: path.join("state"),
            path: path.join("state/zcalld.conf"),
        };
        (context, path)
    }
    #[test]
    fn automatic_codex_connect_prefers_matching_route_and_skips_decision_models() {
        let (context, root) = fixture();
        let file = root.join("codex.toml");
        fs::create_dir_all(&context.directory).unwrap();
        fs::write(
            &context.path,
            "[server]\nlisten = 127.0.0.1:9415\nunix_socket = none\n\
             [provider decisions]\nbase_url = https://example.test/v1\nprotocol = decisions\n\
             [provider text]\nbase_url = https://example.test/v1\nprotocol = responses\n\
             [route 00-decision]\nprovider = decisions\nmodel = old\n\
             [route auto/old]\nprovider = text\nmodel = old\n\
             [route other]\nprovider = text\nmodel = other\n",
        )
        .unwrap();
        fs::write(&file, "model = \"old\"\nmodel_provider = \"openai\"\n").unwrap();
        let agent = Agent {
            id: "codex",
            binary: "codex",
            files: vec![file],
            format: "codex",
        };
        let config = config::Config::load(&context.path).unwrap();
        let url = "http://127.0.0.1:9415";
        assert_eq!(
            automatic_route(&config, Some(&agent), url),
            Some("auto/old")
        );
        assert_eq!(automatic_route(&config, None, url), Some("auto/old"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_uses_live_codex_provider_and_route_and_preserves_external_edits() {
        let (context, root) = fixture();
        let file = root.join("tool/config.toml");
        let url = "http://127.0.0.1:9415";
        let config = |provider, route| {
            format!(
                "model = '{route}' # current route\nmodel_provider = '{provider}'\n\n[model_providers.zcalld] # gateway settings\nbase_url = '{url}/v1'\n"
            )
        };
        let agent = Agent {
            id: "codex",
            binary: "codex",
            files: vec![file.clone()],
            format: "codex",
        };

        json::atomic_write(&file, config("zcalld", "manual").as_bytes()).unwrap();
        assert_eq!(
            observed_connection(&agent, url),
            Some((true, Some("manual".into())))
        );
        assert_eq!(restore_state(&Value::Null, "codex"), (false, false));

        let backup = commit_agent_change(
            &context,
            "codex",
            "original-route",
            &[(file.clone(), config("zcalld", "original-route"))],
            None,
        )
        .unwrap();
        let applied = json::read(&context.directory.join("agents/applied.json")).unwrap();
        assert_eq!(restore_state(&applied, "codex"), (true, false));

        json::atomic_write(&file, config("zcalld", "edited-route").as_bytes()).unwrap();
        assert_eq!(
            observed_connection(&agent, url),
            Some((true, Some("edited-route".into())))
        );
        assert_eq!(restore_state(&applied, "codex"), (false, true));
        assert!(restore_manifest(&backup, true, None).is_err());
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            config("zcalld", "edited-route")
        );

        json::atomic_write(&file, config("openai", "edited-route").as_bytes()).unwrap();
        assert_eq!(
            observed_connection(&agent, url),
            Some((false, Some("edited-route".into())))
        );
        fs::remove_file(&file).unwrap();
        assert_eq!(observed_connection(&agent, url), None);
        assert_eq!(restore_state(&applied, "codex"), (false, true));
        fs::remove_dir_all(root).unwrap();
    }

    struct MemoryCredentials {
        values: std::cell::RefCell<BTreeMap<String, Vec<u8>>>,
        unavailable: bool,
        fail_live_once: std::cell::Cell<bool>,
    }
    impl MemoryCredentials {
        fn ready() -> Self {
            Self {
                values: Default::default(),
                unavailable: false,
                fail_live_once: std::cell::Cell::new(false),
            }
        }
    }
    impl CredentialStore for MemoryCredentials {
        fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            if self.unavailable {
                return Err("test store unavailable".into());
            }
            Ok(self.values.borrow().get(key).cloned())
        }
        fn write(&self, key: &str, secret: &[u8]) -> Result<(), String> {
            if self.unavailable {
                return Err("test store unavailable".into());
            }
            self.values.borrow_mut().insert(key.into(), secret.to_vec());
            if key.starts_with("http:") && self.fail_live_once.replace(false) {
                return Err("test write failed after committing".into());
            }
            Ok(())
        }
        fn remove(&self, key: &str) -> Result<(), String> {
            self.values.borrow_mut().remove(key);
            Ok(())
        }
    }
    #[test]
    fn mac_bundles_and_windows_installations_are_independent_of_path() {
        let (_, root) = fixture();
        let home = root.join("home");
        let apps = root.join("Applications");
        let roaming = home.join("AppData/Roaming");
        let local = home.join("AppData/Local");
        let system = root.join("Program Files");
        let mac = installation_candidates(
            "macos",
            "zed",
            "zed",
            &home,
            &roaming,
            &local,
            std::slice::from_ref(&apps),
        );
        let actual = apps.join("Zed Preview.app/Contents/MacOS/zed");
        assert!(mac.contains(&actual));
        assert!(
            !mac.iter()
                .any(|p| p.extension().is_some_and(|v| v == "app"))
        );
        let windows = installation_candidates(
            "windows",
            "vscode",
            "code",
            &home,
            &roaming,
            &local,
            std::slice::from_ref(&system),
        );
        assert!(windows.contains(&local.join("Programs/Microsoft VS Code/Code.exe")));
        assert!(windows.contains(&system.join("Microsoft VS Code/Code.exe")));
        let cli =
            installation_candidates("windows", "claude", "claude", &home, &roaming, &local, &[]);
        assert!(cli.contains(&home.join(".local/bin/claude.exe")));
        assert!(cli.contains(&roaming.join("npm/claude.cmd")));
        // A leftover app directory is insufficient evidence of an installed executable.
        fs::create_dir_all(actual.parent().unwrap()).unwrap();
        assert!(!is_executable(&actual));
        fs::write(&actual, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&actual, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(is_executable(&actual));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn codex_explicit_gateway_disables_openai_login_with_boolean() {
        let text =
            "[model_providers.zcalld]\nrequires_openai_auth = true # preserve\n[other]\nkeep=1\n";
        let updated = toml_set_literal(
            text,
            "model_providers.zcalld",
            "requires_openai_auth",
            "false",
        )
        .unwrap();
        assert!(updated.contains("requires_openai_auth = false # preserve"));
        assert!(!updated.contains("\"false\""));
        assert!(updated.contains("[other]\nkeep=1"));
    }
    #[test]
    fn zed_credentials_are_verified_and_reverted_without_plaintext_backups() {
        let (context, root) = fixture();
        let settings = root.join("settings.json");
        fs::write(&settings, b"{\"original\":true}\n").unwrap();
        let store = MemoryCredentials::ready();
        let key = "http://localhost:1234/v1";
        store.write(key, b"old-fixture-key").unwrap();
        let changes = vec![(settings.clone(), "{\"managed\":true}\n".into())];
        let saved = commit_agent_change(
            &context,
            "zed",
            "test-route",
            &changes,
            Some((&store, key, b"new-fixture-key")),
        )
        .unwrap();
        assert_eq!(
            store.read(key).unwrap().as_deref(),
            Some(b"new-fixture-key".as_slice())
        );
        let manifest = fs::read_to_string(saved.join("manifest.json")).unwrap();
        assert!(!manifest.contains("fixture-key"));
        assert!(manifest.contains("credential"));
        restore_manifest(&saved, true, Some(&store)).unwrap();
        assert_eq!(
            fs::read_to_string(settings).unwrap(),
            "{\"original\":true}\n"
        );
        assert_eq!(
            store.read(key).unwrap().as_deref(),
            Some(b"old-fixture-key".as_slice())
        );
        assert_eq!(store.values.borrow().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zed_unavailable_keychain_does_not_change_settings() {
        let (context, root) = fixture();
        let settings = root.join("settings.json");
        fs::write(&settings, b"original").unwrap();
        let store = MemoryCredentials {
            unavailable: true,
            ..MemoryCredentials::ready()
        };
        let result = commit_agent_change(
            &context,
            "zed",
            "route",
            &[(settings.clone(), "updated".into())],
            Some((&store, "http://localhost/v1", b"fixture")),
        );
        assert!(result.unwrap_err().contains("unavailable"));
        assert_eq!(fs::read_to_string(settings).unwrap(), "original");
        assert!(!context.directory.join("agents/applied.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zed_failed_store_write_restores_previous_credential_and_files() {
        let (context, root) = fixture();
        let settings = root.join("settings.json");
        fs::write(&settings, b"original").unwrap();
        let store = MemoryCredentials::ready();
        let key = "http://localhost/v1";
        store.write(key, b"previous-fixture").unwrap();
        store.fail_live_once.set(true);
        let result = commit_agent_change(
            &context,
            "zed",
            "route",
            &[(settings.clone(), "updated".into())],
            Some((&store, key, b"new-fixture")),
        );
        assert!(result.unwrap_err().contains("restored"));
        assert_eq!(fs::read_to_string(settings).unwrap(), "original");
        assert_eq!(
            store.read(key).unwrap().as_deref(),
            Some(b"previous-fixture".as_slice())
        );
        assert!(!context.directory.join("agents/applied.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zed_revert_refuses_a_credential_changed_after_apply() {
        let (context, root) = fixture();
        let settings = root.join("settings.json");
        fs::write(&settings, b"original").unwrap();
        let store = MemoryCredentials::ready();
        let key = "http://localhost/v1";
        let saved = commit_agent_change(
            &context,
            "zed",
            "route",
            &[(settings.clone(), "updated".into())],
            Some((&store, key, b"fixture")),
        )
        .unwrap();
        store.write(key, b"user-changed-fixture").unwrap();
        assert!(
            restore_manifest(&saved, true, Some(&store))
                .unwrap_err()
                .contains("changed after apply")
        );
        assert_eq!(fs::read_to_string(settings).unwrap(), "updated");
        assert_eq!(
            store.read(key).unwrap().as_deref(),
            Some(b"user-changed-fixture".as_slice())
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zed_configuration_write_failure_restores_key_and_previous_file() {
        let (context, root) = fixture();
        let settings = root.join("settings.json");
        fs::write(&settings, b"original").unwrap();
        let blocked = root.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let store = MemoryCredentials::ready();
        let key = "http://localhost/v1";
        store.write(key, b"previous-fixture").unwrap();
        let changes = [
            (settings.clone(), "updated".into()),
            (blocked.join("settings.json"), "updated".into()),
        ];
        let error = commit_agent_change(
            &context,
            "zed",
            "route",
            &changes,
            Some((&store, key, b"new-fixture")),
        )
        .unwrap_err();
        assert!(error.contains("restored"), "{error}");
        assert_eq!(fs::read_to_string(settings).unwrap(), "original");
        assert_eq!(
            store.read(key).unwrap().as_deref(),
            Some(b"previous-fixture".as_slice())
        );
        assert!(!context.directory.join("agents/applied.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn secret_service_utility_receives_correct_attributes_and_private_stdin() {
        use std::os::unix::fs::PermissionsExt;
        let (_, root) = fixture();
        let command = root.join("secret-tool-fixture");
        let script = r#"#!/bin/sh
printf '%s\n' "$@" >> "$0.args"
case "$1" in
store)
    test "$2" = "--label=zed-github-account" || exit 2
    test "$3" = "url" || exit 2
    test "$5" = "username" || exit 2
    test "$6" = "Bearer" || exit 2
    IFS= read -r value || :
    printf '%s\n' "$value" > "$0.value"
    ;;
lookup)
    test -f "$0.value" || exit 1
    cat "$0.value"
    ;;
clear) rm -f "$0.value" ;;
*) exit 2 ;;
esac
"#;
        fs::write(&command, script).unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let store = SystemCredentialStore {
            command: command.clone(),
        };
        let key = "http://localhost:9876/v1";
        assert!(store.read(key).unwrap().is_none());
        verify_credential(&store, key, b"private-fixture-key").unwrap();
        let argv = fs::read_to_string(command.with_extension("args")).unwrap();
        assert!(argv.contains("zed-github-account"));
        assert!(!argv.contains("private-fixture-key"));
        store.remove(key).unwrap();
        assert!(store.read(key).unwrap().is_none());
        fs::write(
            &command,
            "#!/bin/sh\nprintf 'private-fixture-key' >&2\nexit 2\n",
        )
        .unwrap();
        assert!(!store.read(key).unwrap_err().contains("private-fixture-key"));
        fs::remove_dir_all(root).unwrap();
    }
}
