//! Agent discovery and reversible, field-level configuration changes.
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
            files: vec![x.join("zed/settings.json")],
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
            files: vec![x.join(
                "Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json",
            )],
            format: "detect",
        },
        Agent {
            id: "roo",
            binary: "roo",
            files: vec![x.join(
                "Code/User/globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json",
            )],
            format: "detect",
        },
        Agent {
            id: "kilo",
            binary: "kilo",
            files: vec![
                x.join("Code/User/globalStorage/kilocode.kilo-code/settings/mcp_settings.json"),
            ],
            format: "detect",
        },
    ]
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
        "list" | "detect" => list(context),
        "apply" => apply(context, args),
        "revert" => revert(context, args),
        "catalog" => catalog(context, args),
        _ => Err("agents expects list, detect, apply, revert or catalog".into()),
    }
}

fn list(context: &Context) -> Result<(), String> {
    let url = control::gateway_url(context).unwrap_or_default();
    let applied = json::read(&context.directory.join("agents/applied.json")).unwrap_or_default();
    let mut list = Vec::new();
    for agent in agents(context) {
        let binary = executable(agent.binary);
        let observed = agent.files.iter().any(|p| p.is_file());
        let configured = agent
            .files
            .iter()
            .any(|p| !url.is_empty() && control::read_text(p).is_ok_and(|s| s.contains(&url)));
        list.push(object([
            ("name", agent.id.into()),
            ("installed", binary.is_some().into()),
            ("detected_config", observed.into()),
            ("configured", configured.into()),
            ("route", applied.get(agent.id).get("route").clone()),
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
        let paths: Vec<_> = files
            .iter()
            .filter_map(|f| f.get("path").as_str())
            .filter_map(|p| under_home(context, p).ok())
            .collect();
        let configured = paths
            .iter()
            .any(|p| !url.is_empty() && control::read_text(p).is_ok_and(|s| s.contains(&url)));
        list.push(object([
            ("name", name.into()),
            ("installed", binary.is_some().into()),
            ("detected_config", paths.iter().any(|p| p.is_file()).into()),
            ("configured", configured.into()),
            ("route", Value::Null),
            (
                "path",
                paths
                    .first()
                    .map(|p| p.to_string_lossy().into_owned())
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
                && k.trim() == key {
                    if found.is_some()
                        || (v.trim_start().starts_with("\"\"\"")
                            || v.trim_start().starts_with("'''"))
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
    let new = format!("{key} = {}", crate::http::quote(value));
    if let Some(i) = found {
        let comment = toml_comment(lines[i].split_once('=').unwrap().1);
        lines[i] = format!("{new}{comment}");
    } else if let Some(i) = insertion {
        lines.insert(i, new);
    } else {
        lines.push(String::new());
        lines.push(format!("[{table}]"));
        lines.push(new);
    }
    Ok(lines.join("\n") + "\n")
}
fn toml_comment(value: &str) -> String {
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
            return format!(" {}", &value[i..]);
        }
    }
    String::new()
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

fn apply(context: &Context, args: &[String]) -> Result<(), String> {
    let name = args.get(1).ok_or("agent name required")?;
    let config = config::Config::load(&context.path)?;
    let route = control::option(args, "--route")?
        .map(str::to_string)
        .or_else(|| config.routes.first().map(|r| r.name.clone()))
        .ok_or("add a provider/model route before applying an agent")?;
    if !config.routes.iter().any(|r| r.name == route) {
        return Err("route is not in the configured model catalog".into());
    }
    let url = control::gateway_url(context)?;
    let v1 = format!("{url}/v1");
    let token = control::gateway_token(context)?;
    let mut changes = Vec::new();
    if let Some(agent) = agents(context).into_iter().find(|a| a.id == name) {
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
                toml_set(
                    &s,
                    "model_providers.zcalld",
                    "experimental_bearer_token",
                    &token,
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
                        object([
                            ("id", r.name.clone().into()),
                            ("name", r.name.clone().into()),
                            ("reasoning", true.into()),
                            ("input", Value::Array(vec!["text".into(), "image".into()])),
                            ("contextWindow", 200000u64.into()),
                            ("maxTokens", 32768u64.into()),
                        ])
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
    let paths: Vec<_> = changes.iter().map(|(p, _)| p.clone()).collect();
    let saved = control::backup(context, &paths, &format!("agent-{name}"))?;
    for (path, text) in &changes {
        if let Err(error) = json::atomic_write(path, text.as_bytes()) {
            restore_manifest(&saved, false)?;
            return Err(error);
        }
    }
    let mut record = json::read(&saved.join("manifest.json"))?;
    for (i, (_, text)) in changes.iter().enumerate() {
        json::atomic_write(&saved.join(format!("{i}.applied")), text.as_bytes())?;
    }
    record.set("route", route.clone().into());
    json::atomic_write(&saved.join("manifest.json"), record.stringify().as_bytes())?;
    let applied = context.directory.join("agents/applied.json");
    let mut state = json::read(&applied).unwrap_or_default();
    state.set(
        name,
        object([
            ("route", route.clone().into()),
            ("backup", saved.to_string_lossy().into_owned().into()),
        ]),
    );
    json::atomic_write(&applied, state.stringify().as_bytes())?;
    control::emit(object([
        ("ok", true.into()),
        ("agent", name.clone().into()),
        ("route", route.into()),
        ("files", paths.len().into()),
        ("backup", saved.to_string_lossy().into_owned().into()),
    ]))
}

fn restore_manifest(directory: &Path, check: bool) -> Result<(), String> {
    let manifest = json::read(&directory.join("manifest.json"))?;
    if check {
        for (i, record) in manifest.get("files").as_array().iter().enumerate() {
            let path = Path::new(record.get("path").as_str().ok_or("invalid backup path")?);
            let expected =
                fs::read(directory.join(format!("{i}.applied"))).map_err(|e| e.to_string())?;
            if fs::read(path).map_err(|e| e.to_string())? != expected {
                return Err("agent file changed after apply; review it before reverting, or use --force to restore exact backup".into());
            }
        }
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
    Ok(())
}
fn revert(context: &Context, args: &[String]) -> Result<(), String> {
    let name = args
        .get(1)
        .filter(|s| control::safe_id(s))
        .ok_or("safe agent name required")?;
    let path = context.directory.join("agents/applied.json");
    let mut state = json::read(&path)?;
    let backup = state
        .get(name)
        .get("backup")
        .as_str()
        .ok_or("agent has no managed change to revert")?
        .to_string();
    restore_manifest(Path::new(&backup), !control::flag(args, "--force"))?;
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
}
