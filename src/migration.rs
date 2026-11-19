//! Copy-first Magpie migration. This module never stops or edits the live gateway.
use crate::{
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str).unwrap_or("status") {
        "magpie" => import(context, args),
        "status" | "verify" => verify(context),
        _ => Err(
            "migrate expects magpie [--source PATH] [--catalog FILE] [--apply], status or verify"
                .into(),
        ),
    }
}

fn read_optional(path: &Path) -> Result<Value, String> {
    if path.exists() {
        json::read(path)
    } else {
        Ok(Value::Null)
    }
}
fn safe_internal(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(72)
        .collect();
    format!(
        "p-{}-{}",
        if clean.is_empty() { "provider" } else { &clean },
        &crate::digest::sha256(id.as_bytes())[..8]
    )
}
fn model_catalog(source: &Path, args: &[String]) -> Result<Value, String> {
    if let Some(file) = control::option(args, "--catalog")? {
        return json::read(Path::new(file));
    }
    let saved = source.join("model-catalog.json");
    if saved.exists() {
        return json::read(&saved);
    }
    let port = control::option(args, "--port")?
        .unwrap_or("3425")
        .parse::<u16>()
        .map_err(|_| "invalid gateway port")?;
    let (code, body, _) = control::command(
        "curl",
        &[
            "--fail".into(),
            "--silent".into(),
            "--max-time".into(),
            "5".into(),
            format!("http://127.0.0.1:{port}/v1/models"),
        ],
        None,
        Duration::from_secs(7),
    )?;
    if code != 0 {
        return Err("cannot read the live Magpie model catalog; use --catalog with an exported /v1/models document".into());
    }
    json::parse(body.as_bytes())
}

#[derive(Clone)]
struct Route {
    provider: String,
    model: String,
}
struct Plan {
    config: String,
    credentials: String,
    routes: BTreeMap<String, Route>,
    unresolved: Vec<String>,
    groups: Vec<Value>,
    deciders: Vec<Value>,
    limitations: Vec<String>,
    provider_count: usize,
}

fn plan_from_active(
    settings: &Value,
    providers: &Value,
    plugins: &[Value],
    catalog: &Value,
    port: u16,
) -> Result<Plan, String> {
    let mut config = format!(
        "# Imported from Magpie. Private credentials live in ./credentials.\n[server]\nlisten = {}:{port}\n{}trust_loopback = true\ncatalog_file = model-catalog.json\nworkers = 4\nqueue = 32\nmax_body_bytes = 16777216\nmax_response_bytes = 16777216\n",
        if settings.get("lan").as_bool() == Some(true) {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        },
        if cfg!(unix) {
            "unix_socket = auto\n"
        } else {
            ""
        }
    );
    let mut credentials = String::new();
    if let Some(token) = settings.get("lanKey").as_str().filter(|v| !v.is_empty()) {
        config.push_str("token_env = ZCALLD_GATEWAY_TOKEN\n");
        credentials.push_str(&format!(
            "ZCALLD_GATEWAY_TOKEN={}\n",
            crate::http::quote(token)
        ));
    } else if settings.get("lan").as_bool() == Some(true) {
        return Err("Magpie LAN access is enabled without a saved LAN key; migration requires a gateway token".into());
    }
    let mut direct = BTreeMap::new();
    let mut deciders = Vec::new();
    let mut provider_count = 0;
    for provider in providers.get("providers").as_array() {
        if provider.get("off").as_bool() == Some(true) {
            continue;
        }
        let Some(id) = provider.get("id").as_str() else {
            continue;
        };
        if let Some(url) = provider.get("decide").as_str().filter(|s| !s.is_empty()) {
            deciders.push(object([
                ("id", id.into()),
                ("url", url.into()),
                ("protocol", "systemone".into()),
                ("key", provider.get("key").clone()),
            ]));
        } else if let Some(url) = provider.get("chat").as_str().filter(|s| !s.is_empty()) {
            deciders.push(object([
                ("id", id.into()),
                ("url", url.into()),
                ("protocol", "chat".into()),
                ("key", provider.get("key").clone()),
            ]));
        }
        let native: Vec<_> = ["responses", "chat", "anthropic", "gemini"]
            .into_iter()
            .filter_map(|p| {
                provider
                    .get(p)
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(|url| (p, url))
            })
            .collect();
        if native.is_empty() {
            continue;
        }
        let internal = safe_internal(id);
        config.push_str(&format!(
            "\n[provider {internal}]\nbase_url = {}\nprotocol = {}\n",
            native[0].1, native[0].0
        ));
        for (protocol, url) in &native {
            if url.contains(['\r', '\n']) {
                return Err("provider endpoint contains a newline".into());
            }
            config.push_str(&format!("{protocol}_url = {url}\n"));
        }
        if let Some(key) = provider.get("key").as_str().filter(|v| !v.is_empty()) {
            let variable = format!("ZCALLD_IMPORTED_{}_KEY", provider_count);
            credentials.push_str(&format!("{variable}={}\n", crate::http::quote(key)));
            config.push_str(&format!("api_key_env = {variable}\n"));
        } else {
            config.push_str("auth = none\n");
        }
        direct.insert(id.to_string(), internal.clone());
        for alias in provider
            .get("was")
            .as_array()
            .iter()
            .filter_map(Value::as_str)
        {
            direct.insert(alias.into(), internal.clone());
        }
        provider_count += 1;
    }
    let mut routes: BTreeMap<String, Route> = BTreeMap::new();
    let mut unresolved = Vec::new();
    let mut plugin_variants = BTreeMap::new();
    let mut groups = Vec::new();
    let mut limitations = Vec::new();
    let models = catalog.get("data").as_array();
    if models.is_empty() {
        return Err("catalog has no model objects".into());
    }
    let mut seen = BTreeSet::new();
    for model in models {
        let id = model.get("id").as_str().ok_or("catalog model has no ID")?;
        if !seen.insert(id) {
            return Err("catalog repeats a model ID".into());
        }
        if !control::valid_name(id) {
            return Err("catalog contains a model ID that cannot be stored as a route".into());
        }
        let Some((prefix, tail)) = id.split_once('/') else {
            unresolved.push(id.into());
            continue;
        };
        if let Some(provider) = direct.get(prefix) {
            routes.insert(
                id.into(),
                Route {
                    provider: provider.clone(),
                    model: tail.into(),
                },
            );
            continue;
        }
        let Some(plugin) = plugins
            .iter()
            .find(|p| p.get("sourceId").as_str().or_else(|| p.get("id").as_str()) == Some(prefix))
        else {
            if prefix != "group" && prefix != "auto" {
                unresolved.push(id.into());
            }
            continue;
        };
        let provider_model = plugin
            .get("models")
            .as_array()
            .iter()
            .find(|m| m.get("id").as_str() == Some(tail) || m.get("id").as_str() == Some(id));
        let api = provider_model
            .and_then(|m| m.get("url").as_str())
            .filter(|v| !v.is_empty())
            .or_else(|| plugin.get("api").as_str())
            .ok_or("plugin model has no native endpoint")?;
        let npm = provider_model
            .and_then(|m| m.get("npm").as_str())
            .filter(|v| !v.is_empty())
            .or_else(|| plugin.get("npm").as_str())
            .unwrap_or("@ai-sdk/openai-compatible");
        let account = plugin.get("accountId").as_str().unwrap_or("");
        let plugin_id = plugin
            .get("pluginId")
            .as_str()
            .or_else(|| plugin.get("id").as_str())
            .unwrap_or(prefix);
        let protocol = provider_model
            .and_then(|m| m.get("protocol").as_str())
            .or_else(|| plugin.get("protocol").as_str())
            .unwrap_or_else(|| {
                if npm.contains("anthropic") {
                    "anthropic"
                } else if npm.contains("google") {
                    "gemini"
                } else if npm == "@ai-sdk/openai" {
                    "responses"
                } else {
                    "chat"
                }
            });
        let native_urls = provider_model
            .map(|m| m.get("native_urls"))
            .unwrap_or(&Value::Null);
        let key = format!(
            "{prefix}\n{api}\n{npm}\n{account}\n{protocol}\n{}",
            native_urls.stringify()
        );
        let provider = if let Some(existing) = plugin_variants.get(&key) {
            String::clone(existing)
        } else {
            let name = format!("plugin-{}-{}", safe_internal(prefix), plugin_variants.len());
            if [api, npm, account, plugin_id]
                .iter()
                .any(|s| s.chars().any(char::is_control))
            {
                return Err("plugin metadata contains a control character".into());
            }
            config.push_str(&format!("\n[provider {name}]\nbase_url = {api}\nprotocol = {protocol}\nauth = none\nplugin = {plugin_id}\nplugin_npm = {npm}\n"));
            if let Some(urls) = native_urls.as_object() {
                for (kind, url) in urls {
                    if ![
                        "chat",
                        "responses",
                        "anthropic",
                        "gemini",
                        "completions",
                        "interactions",
                    ]
                    .contains(&kind.as_str())
                    {
                        return Err("builtin metadata names an unknown protocol".into());
                    }
                    let url = url
                        .as_str()
                        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                        .ok_or("invalid builtin native URL")?;
                    config.push_str(&format!("{kind}_url = {url}\n"));
                }
            }
            if !account.is_empty() {
                config.push_str(&format!("plugin_account = {account}\n"));
            }
            provider_count += 1;
            plugin_variants.insert(key, name.clone());
            name
        };
        let upstream = provider_model
            .and_then(|m| m.get("apiId").as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(tail);
        routes.insert(
            id.into(),
            Route {
                provider,
                model: upstream.into(),
            },
        );
    }
    for model in models {
        let id = model.get("id").as_str().unwrap();
        if !id.starts_with("auto/") && !id.starts_with("group/") {
            continue;
        }
        let (kind, name) = id.split_once('/').unwrap();
        let mut mode = "order".to_string();
        let mut members = Vec::new();
        let mut original = Value::Null;
        if kind == "group" {
            if let Some(group) = providers.get("groups").as_array().iter().find(|g| {
                g.get("id").as_str() == Some(name) || g.get("name").as_str() == Some(name)
            }) {
                original = group.clone();
                mode = group.get("routing").as_str().unwrap_or("smart").to_string();
                for member in group.get("members").as_array() {
                    if let Some(member) = member.as_str().or_else(|| member.get("model").as_str())
                        && routes.contains_key(member)
                    {
                        members.push(member.to_string());
                    }
                }
                if let Some(classifier) = group.get("classifier").as_str().filter(|s| !s.is_empty())
                {
                    let provider = classifier.split_once('/').map_or(classifier, |p| p.0);
                    if !deciders
                        .iter()
                        .any(|d| d.get("id").as_str() == Some(provider))
                    {
                        limitations.push(format!(
                            "{id}: the configured classifier has no decision transport"
                        ));
                    }
                }
                for rule in group.get("rules").as_array() {
                    for field in ["time", "where", "folders", "exceptFolders"] {
                        let value = rule.get(field);
                        if !matches!(value, Value::Null | Value::Array(_))
                            || !value.as_array().is_empty()
                        {
                            limitations.push(format!(
                                "{id}: {field} rule requires the rule compatibility worker"
                            ));
                        }
                    }
                }
            }
        } else {
            let suffix = canonical_model(name);
            members = routes
                .keys()
                .filter(|r| {
                    r.split_once('/')
                        .is_some_and(|(_, tail)| canonical_model(tail) == suffix)
                })
                .cloned()
                .collect();
            mode = "smart".into();
        }
        if members.is_empty() {
            unresolved.push(id.into());
            continue;
        }
        let target = routes.get(&members[0]).unwrap().clone();
        routes.insert(id.into(), target);
        groups.push(object([
            ("name", id.into()),
            ("mode", mode.into()),
            (
                "members",
                Value::Array(
                    members
                        .into_iter()
                        .map(|m| {
                            let metadata = models.iter().find(|v| v.get("id").as_str() == Some(&m));
                            let mut member =
                                object([("model", m.clone().into()), ("weight", 1u64.into())]);
                            if let Some(metadata) = metadata {
                                member.set(
                                    "reasoning_levels",
                                    metadata.get("reasoning_levels").clone(),
                                );
                                member
                                    .set("context_window", metadata.get("context_window").clone());
                            }
                            if original
                                .get("off")
                                .as_array()
                                .iter()
                                .any(|v| v.as_str() == Some(&m))
                            {
                                member.set("off", true.into());
                            }
                            if let Some(weight) = original.get("weights").get(&m).as_u64() {
                                member.set("weight", weight.into());
                            }
                            member
                        })
                        .collect(),
                ),
            ),
            ("original", original),
        ]));
    }
    for (name, route) in &routes {
        if route.model.chars().any(char::is_control) {
            return Err("upstream model contains a control character".into());
        }
        config.push_str(&format!(
            "\n[route {name}]\nprovider = {}\nmodel = {}\n",
            route.provider, route.model
        ));
    }
    Ok(Plan {
        config,
        credentials,
        routes,
        unresolved,
        groups,
        deciders,
        limitations,
        provider_count,
    })
}

fn canonical_model(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| e.to_string())
}
fn copy_checked(source: &Path, destination: &Path) -> Result<(u64, u64), String> {
    let metadata = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "migration source contains a symlink requiring review: {}",
            source.display()
        ));
    }
    if metadata.is_dir() {
        private_dir(destination)?;
        let mut count = 0;
        let mut bytes = 0;
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let (c, b) = copy_checked(&entry.path(), &destination.join(entry.file_name()))?;
            count += c;
            bytes += b;
        }
        return Ok((count, bytes));
    }
    if !metadata.is_file() {
        return Ok((0, 0));
    }
    if let Some(parent) = destination.parent() {
        private_dir(parent)?;
    }
    let mut source_file = fs::File::open(source).map_err(|e| e.to_string())?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Keep helper scripts runnable by their owner without exposing copied
        // credentials or skill content to other users.
        let executable = metadata.permissions().mode() & 0o111 != 0;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut target = options.open(destination).map_err(|e| e.to_string())?;
    // Usage logs append while the old gateway is live. Copy and verify a fixed
    // prefix; the final handover copies any appended tail after it has drained.
    let append_only = source.extension().is_some_and(|v| v == "jsonl");
    let bytes = std::io::copy(&mut source_file.by_ref().take(metadata.len()), &mut target)
        .map_err(|e| e.to_string())?;
    target.sync_all().map_err(|e| e.to_string())?;
    if bytes != metadata.len() {
        return Err("source changed while its migration snapshot was copied".into());
    }
    if !append_only && fs::metadata(source).map_err(|e| e.to_string())?.len() != metadata.len() {
        return Err("source changed while its migration snapshot was copied".into());
    }
    let mut a = fs::File::open(source)
        .map_err(|e| e.to_string())?
        .take(metadata.len());
    let mut b = fs::File::open(destination).map_err(|e| e.to_string())?;
    let mut one = [0; 65536];
    let mut two = [0; 65536];
    loop {
        let n = a.read(&mut one).map_err(|e| e.to_string())?;
        let m = b.read(&mut two).map_err(|e| e.to_string())?;
        if n != m || one[..n] != two[..m] {
            return Err(
                "migration snapshot verification failed; live data may have changed".into(),
            );
        }
        if n == 0 {
            break;
        }
    }
    Ok((1, bytes))
}

// Materialize internal library links so the imported skills keep working after
// Magpie is removed. Never follow a link outside the library or execute a file.
fn copy_library_tree(source: &Path, destination: &Path) -> Result<(u64, u64), String> {
    let metadata = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("the Magpie library root must be a real directory".into());
    }
    let root = source.canonicalize().map_err(|e| e.to_string())?;
    fn visit(
        path: &Path,
        destination: &Path,
        root: &Path,
        ancestors: &mut Vec<std::path::PathBuf>,
    ) -> Result<(u64, u64), String> {
        let resolved = path.canonicalize().map_err(|e| e.to_string())?;
        if !resolved.starts_with(root) {
            return Err("library contains a link outside its own directory".into());
        }
        let metadata = fs::metadata(&resolved).map_err(|e| e.to_string())?;
        if metadata.is_file() {
            return copy_checked(&resolved, destination);
        }
        if !metadata.is_dir() {
            return Err("library contains a special file requiring review".into());
        }
        if ancestors.contains(&resolved) {
            return Err("library contains a directory link cycle".into());
        }
        ancestors.push(resolved.clone());
        private_dir(destination)?;
        let mut total = (0, 0);
        for entry in fs::read_dir(&resolved).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let copied = visit(
                &entry.path(),
                &destination.join(entry.file_name()),
                root,
                ancestors,
            )?;
            total.0 += copied.0;
            total.1 += copied.1;
        }
        ancestors.pop();
        Ok(total)
    }
    visit(&root, destination, &root, &mut Vec::new())
}

fn import(context: &Context, args: &[String]) -> Result<(), String> {
    let source = control::option(args, "--source")?
        .map(PathBuf::from)
        .unwrap_or_else(|| context.home.join(".config/magpie"));
    let source = fs::canonicalize(source).map_err(|e| e.to_string())?;
    let settings = json::read(&source.join("settings.json"))?;
    let providers = json::read(&source.join("providers.json"))?;
    let plugin_catalog = read_optional(&source.join("plugin-providers.json"))?;
    let mut plugins = plugin_catalog.as_array().to_vec();
    if let Some(path) = control::option(args, "--builtin-catalog")? {
        plugins.extend_from_slice(json::read(Path::new(path))?.as_array());
    } else {
        plugins.extend_from_slice(
            json::parse(include_str!("../extensions/builtin-providers.json").as_bytes())?
                .as_array(),
        );
    }
    let catalog = model_catalog(&source, args)?;
    let port = control::option(args, "--port")?
        .unwrap_or("3425")
        .parse::<u16>()
        .map_err(|_| "invalid port")?;
    let mut plan = plan_from_active(&settings, &providers, &plugins, &catalog, port)?;
    let summary_of = |plan: &Plan| {
        object([
            ("providers", plan.provider_count.into()),
            (
                "catalog_models",
                catalog.get("data").as_array().len().into(),
            ),
            ("mapped_models", plan.routes.len().into()),
            ("unresolved_models", plan.unresolved.len().into()),
            ("routing_groups", plan.groups.len().into()),
            ("compatibility_gaps", plan.limitations.len().into()),
            ("source", source.to_string_lossy().into_owned().into()),
            (
                "destination",
                context.directory.to_string_lossy().into_owned().into(),
            ),
        ])
    };
    let summary = summary_of(&plan);
    if !control::flag(args, "--apply") {
        return control::emit(object([("dry_run",true.into()),("ready",(plan.unresolved.is_empty()&&plan.limitations.is_empty()).into()),("summary",summary), ("message","No configuration or running process was changed. Use --apply to create a private copy for validation.".into())]));
    }
    if context.path.exists() {
        return Err("destination config already exists; use a fresh --config directory to avoid overwriting managed credentials".into());
    }
    private_dir(&context.directory)?;
    let snapshot = context
        .directory
        .join("backups")
        .join(format!("magpie-{}", control::now()));
    // Package dependencies/model caches are reproducible and handled separately by
    // the extension importer. Unique configuration and history are copied exactly.
    private_dir(&snapshot)?;
    let mut files = 0;
    let mut bytes = 0;
    let mut skipped = Vec::new();
    for entry in fs::read_dir(&source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if matches!(
            name_text.as_ref(),
            "plugins" | "backups" | "cache" | "locks"
        ) {
            skipped.push(name_text.into_owned().into());
            continue;
        }
        if entry.file_type().map_err(|e| e.to_string())?.is_symlink() {
            skipped.push(name_text.into_owned().into());
            continue;
        }
        let (count, size) = if name_text == "library" {
            copy_library_tree(&entry.path(), &snapshot.join(&name))?
        } else {
            copy_checked(&entry.path(), &snapshot.join(&name))?
        };
        files += count;
        bytes += size;
    }
    let helper = crate::maintenance::plugin_helper()?;
    let mut command = vec![
        "--root".into(),
        context
            .directory
            .join("extensions")
            .to_string_lossy()
            .into_owned(),
        "import-magpie".into(),
        "--source".into(),
        source.to_string_lossy().into_owned(),
        "--privacy-root".into(),
        context
            .directory
            .join("privacy")
            .to_string_lossy()
            .into_owned(),
        "--model-root".into(),
        control::option(args, "--model-root")?
            .map(str::to_string)
            .unwrap_or_else(|| {
                context
                    .home
                    .join(".cache/magpie")
                    .to_string_lossy()
                    .into_owned()
            }),
        "--json".into(),
    ];
    if control::flag(args, "--skip-extensions") {
        command.clear();
    }
    let extensions = if command.is_empty() {
        Value::Null
    } else {
        let (code, out, _) = control::command(&helper, &command, None, Duration::from_secs(600))?;
        if code != 0 {
            return Err("extension migration failed; live Magpie is unchanged and its private snapshot was retained".into());
        }
        json::parse(out.as_bytes())?
    };
    if !command.is_empty() {
        if let Some(path) = control::option(args, "--oauth-clients")? {
            let clients = json::read(Path::new(path))?;
            json::atomic_write(
                &context
                    .directory
                    .join("extensions/builtin-oauth-clients.json"),
                clients.stringify().as_bytes(),
            )?;
        }
        let (code, _, _) = control::command(
            &helper,
            &[
                "--root".into(),
                context
                    .directory
                    .join("extensions")
                    .to_string_lossy()
                    .into_owned(),
                "import-builtin".into(),
                "--source".into(),
                source.to_string_lossy().into_owned(),
                "--json".into(),
            ],
            None,
            Duration::from_secs(30),
        )?;
        if code != 0 {
            return Err(
                "builtin account import failed; the running Magpie process is unchanged".into(),
            );
        }
        let builtin = json::read(&context.directory.join("extensions/builtin-providers.json"))?;
        let mut active_plugins = plugin_catalog.as_array().to_vec();
        active_plugins.extend_from_slice(builtin.as_array());
        plan = plan_from_active(&settings, &providers, &active_plugins, &catalog, port)?;
    }
    if source.join("caller-keys.json").exists() {
        plan.config = control::ini_set(
            &plan.config,
            "server",
            "caller_keys_file",
            "caller-keys.json",
        )?;
    }
    let summary = summary_of(&plan);
    json::atomic_write(
        &context.directory.join("credentials"),
        plan.credentials.as_bytes(),
    )?;
    json::atomic_write(
        &context.directory.join("model-catalog.json"),
        catalog.stringify().as_bytes(),
    )?;
    json::atomic_write(
        &context.directory.join("provider-source.json"),
        providers.stringify().as_bytes(),
    )?;
    json::atomic_write(
        &context.directory.join("routing.json"),
        object([
            ("groups", Value::Array(plan.groups.clone())),
            ("deciders", Value::Array(plan.deciders.clone())),
            (
                "usage_file",
                context
                    .directory
                    .join("usage.jsonl")
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            ),
        ])
        .stringify()
        .as_bytes(),
    )?;
    for name in [
        "library.json",
        "usage.jsonl",
        "caller-keys.json",
        "logins.json",
        "quota-history.json",
        "quotas.json",
        "applied.json",
        "served.json",
    ] {
        let from = source.join(name);
        if from.is_file() {
            copy_checked(&from, &context.directory.join(name))?;
        }
    }
    if source.join("library").exists() {
        copy_library_tree(&source.join("library"), &context.directory.join("library"))?;
    }
    if source.join("mcp-signins.json").is_file() {
        // Preserve original OAuth state privately without claiming it is an
        // active zcalld MCP authorization or executing an imported login flow.
        copy_checked(
            &source.join("mcp-signins.json"),
            &context.directory.join("migration-archive/mcp-signins.json"),
        )?;
    }
    json::atomic_write(&context.path, plan.config.as_bytes())?;
    let record = object([
        ("format", 1u64.into()),
        ("source", source.to_string_lossy().into_owned().into()),
        ("snapshot", snapshot.to_string_lossy().into_owned().into()),
        ("created", control::now().into()),
        ("summary", summary.clone()),
        ("backup_files", files.into()),
        ("backup_bytes", bytes.into()),
        ("separately_handled", Value::Array(skipped)),
        ("extensions", extensions),
        (
            "unresolved",
            Value::Array(plan.unresolved.iter().cloned().map(Value::from).collect()),
        ),
        (
            "limitations",
            Value::Array(plan.limitations.iter().cloned().map(Value::from).collect()),
        ),
        ("cutover_complete", false.into()),
    ]);
    json::atomic_write(
        &context.directory.join("migration.json"),
        record.stringify().as_bytes(),
    )?;
    control::emit(object([
        ("ok", true.into()),
        ("live_gateway_unchanged", true.into()),
        (
            "ready",
            (plan.unresolved.is_empty() && plan.limitations.is_empty()).into(),
        ),
        ("summary", summary),
        ("snapshot", snapshot.to_string_lossy().into_owned().into()),
    ]))
}

fn verify(context: &Context) -> Result<(), String> {
    let record = json::read(&context.directory.join("migration.json"))?;
    let config = crate::config::Config::load(&context.path);
    let error = config.as_ref().err().cloned();
    let unresolved = record.get("unresolved").as_array().len();
    let limitations = record.get("limitations").as_array().len();
    control::emit(object([
        ("valid", config.is_ok().into()),
        (
            "ready",
            (config.is_ok() && unresolved == 0 && limitations == 0).into(),
        ),
        ("unresolved_models", unresolved.into()),
        ("compatibility_gaps", limitations.into()),
        ("cutover_complete", record.get("cutover_complete").clone()),
        ("summary", record.get("summary").clone()),
        ("error", error.map(Value::from).unwrap_or(Value::Null)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct LibraryFixture(std::path::PathBuf);
    impl LibraryFixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "zcalld-library-migration-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("source/skills/example/scripts")).unwrap();
            Self(path)
        }
    }
    impl Drop for LibraryFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn library_migration_copies_bodies_without_executing_helpers() {
        let fixture = LibraryFixture::new();
        let source = fixture.0.join("source");
        let skill = source.join("skills/example");
        fs::write(
            skill.join("SKILL.md"),
            b"Run the helper only when requested.",
        )
        .unwrap();
        let helper = skill.join("scripts/run.sh");
        fs::write(&helper, b"#!/bin/sh\nexit 73\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let destination = fixture.0.join("destination");
        let (files, _) = copy_library_tree(&source, &destination).unwrap();
        assert_eq!(files, 2);
        let copied = destination.join("skills/example/scripts/run.sh");
        assert_eq!(fs::read(&copied).unwrap(), fs::read(&helper).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(copied).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn library_migration_materializes_internal_links_and_rejects_escape_or_cycle() {
        use std::os::unix::fs::symlink;
        let fixture = LibraryFixture::new();
        let source = fixture.0.join("source");
        fs::write(source.join("shared.md"), b"shared instructions").unwrap();
        symlink("../../shared.md", source.join("skills/example/SKILL.md")).unwrap();
        let destination = fixture.0.join("destination");
        copy_library_tree(&source, &destination).unwrap();
        let body = destination.join("skills/example/SKILL.md");
        assert_eq!(fs::read(&body).unwrap(), b"shared instructions");
        assert!(
            !fs::symlink_metadata(&body)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(fixture.0.join("outside"), b"must not copy").unwrap();
        symlink("../outside", source.join("escape")).unwrap();
        let error = copy_library_tree(&source, &fixture.0.join("escape-result")).unwrap_err();
        assert!(error.contains("outside"));
        fs::remove_file(source.join("escape")).unwrap();
        symlink(".", source.join("loop")).unwrap();
        let error = copy_library_tree(&source, &fixture.0.join("cycle-result")).unwrap_err();
        assert!(error.contains("cycle"));
    }
    #[test]
    fn private_ids_are_path_safe_and_collision_resistant() {
        assert_ne!(safe_internal("a@b"), safe_internal("a#b"));
        assert!(control::safe_id(&safe_internal("../../secret")));
    }
    #[test]
    fn importing_unknown_models_does_not_claim_readiness() {
        let providers = object([("providers", Value::Array(Vec::new()))]);
        let catalog = object([(
            "data",
            Value::Array(vec![object([("id", "unknown/model".into())])]),
        )]);
        let p = plan_from_active(&Value::Null, &providers, &[], &catalog, 3425).unwrap();
        assert_eq!(p.unresolved, vec!["unknown/model"]);
        assert!(p.routes.is_empty());
    }
}
