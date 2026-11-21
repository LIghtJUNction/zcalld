//! On-demand, file-based agent libraries. The gateway never loads this state.
use crate::{
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

const BEGIN: &str = "<!-- zcalld:library:begin -->";
const END: &str = "<!-- zcalld:library:end -->";
const MAX_TREE: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 2048;

#[path = "library_git.rs"]
mod git_source;

fn empty() -> Value {
    object([
        ("mcp", Value::Array(Vec::new())),
        ("skills", Value::Array(Vec::new())),
        (
            "instructions",
            object([("agents", Value::Array(Vec::new()))]),
        ),
    ])
}
fn read_state(path: &Path, fallback: Value) -> Result<Value, String> {
    match fs::metadata(path) {
        Ok(_) => json::read(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(fallback),
        Err(e) => Err(e.to_string()),
    }
}
fn name(value: &str) -> Result<&str, String> {
    if control::safe_id(value) {
        Ok(value)
    } else {
        Err("name must contain letters, digits, '.', '_' or '-' and not start with '.'".into())
    }
}
fn str_list(value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(values) => values
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "expected an array of strings".into())
            })
            .collect(),
        _ => Err("expected an array of strings".into()),
    }
}
fn agents_arg(args: &[String]) -> Result<Option<Value>, String> {
    control::option(args, "--agents")?
        .map(|v| {
            let mut names = BTreeSet::new();
            for item in v.split(',').filter(|s| !s.is_empty()) {
                name(item)?;
                names.insert(item.to_owned());
            }
            Ok(Value::Array(names.into_iter().map(Value::from).collect()))
        })
        .transpose()
}
fn assigned(entry: &Value, agent: &str, legacy_all: bool) -> bool {
    if entry.get("disabled").as_bool() == Some(true)
        || entry.get("hidden").as_bool() == Some(true)
        || entry.get("offHere").as_bool() == Some(true)
    {
        return false;
    }
    match entry.get("agents") {
        Value::Null => legacy_all,
        Value::Array(items) => items.iter().filter_map(Value::as_str).any(|a| {
            a == agent || matches!((a, agent), ("agy", "antigravity") | ("antigravity", "agy"))
        }),
        _ => false,
    }
}
fn validate(state: &Value) -> Result<(), String> {
    if !matches!(state, Value::Object(_)) {
        return Err("library must be a JSON object".into());
    }
    for kind in ["mcp", "skills"] {
        if !matches!(state.get(kind), Value::Null | Value::Array(_)) {
            return Err(format!("{kind} must be an array"));
        }
        let mut seen = BTreeSet::new();
        for entry in state.get(kind).as_array() {
            let id = name(
                entry
                    .get("name")
                    .as_str()
                    .ok_or("library entry has no name")?,
            )?;
            if !seen.insert(id) {
                return Err(format!("duplicate {kind} entry: {id}"));
            }
            str_list(entry.get("agents"))?;
            if kind == "mcp" {
                validate_mcp(entry)?;
            }
        }
    }
    if !matches!(state.get("instructions"), Value::Null | Value::Object(_)) {
        return Err("instructions must be an object".into());
    }
    str_list(state.get("instructions").get("agents"))?;
    let mut seen = BTreeSet::new();
    for set in state.get("instructions").get("sets").as_array() {
        let id = name(set.get("id").as_str().ok_or("instruction set has no id")?)?;
        if !seen.insert(id) {
            return Err(format!("duplicate instruction set: {id}"));
        }
    }
    Ok(())
}

pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    fs::create_dir_all(&context.directory).map_err(|e| e.to_string())?;
    let lock_path = context.directory.join(".library.lock");
    reject_links(&lock_path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&lock_path).map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|_| "another library operation is running; retry after it finishes")?;
    let path = context.directory.join("library.json");
    let mut state = read_state(&path, empty())?;
    validate(&state)?;
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => control::emit(summary(&state)),
        "targets" => target_list(context),
        "import" => {
            let input = control::option(args, "--file")?.ok_or("library import requires --file")?;
            let imported = json::read(Path::new(input))?;
            validate(&imported)?;
            let mut plan = Plan::default();
            if let Some(dir) = control::option(args, "--assets")? {
                let tree = read_tree(Path::new(dir), false)?;
                for (relative, file) in tree { plan.set(context.directory.join("library").join(relative), Some(file))?; }
            }
            plan.json(path, &imported)?;
            plan.commit(context, "library-import")?;
            control::emit(object([("ok", true.into()), ("applied", false.into())]))
        }
        "export" => {
            let target = control::option(args, "--file")?.ok_or("library export requires a private --file")?;
            let mut plan = Plan::default();
            plan.json(PathBuf::from(target), &state)?;
            if let Some(dir) = control::option(args, "--assets")? {
                let source = context.directory.join("library");
                if source.exists() { for (relative, file) in read_tree(&source, false)? { plan.set(Path::new(dir).join(relative), Some(file))?; } }
            }
            plan.commit(context, "library-export")?;
            control::emit(object([("ok", true.into())]))
        }
        "add-mcp" => mcp(context, &mut state, &[vec!["set".to_owned()], args[1..].to_vec()].concat()),
        "mcp" => mcp(context, &mut state, &args[1..]),
        "instructions" => instructions(context, &mut state, &args[1..]),
        "skills" => skills(context, &mut state, &args[1..]),
        "apply" | "revert" => apply(context, &state, args),
        _ => Err("library expects list, targets, import, export, mcp, instructions, skills, apply or revert".into()),
    }
}
fn summary(state: &Value) -> Value {
    object([
        (
            "mcp",
            Value::Array(
                state
                    .get("mcp")
                    .as_array()
                    .iter()
                    .map(mcp_summary)
                    .collect(),
            ),
        ),
        ("skills", state.get("skills").as_array().len().into()),
        ("instructions", sets(state).len().into()),
        (
            "instruction_agents",
            state.get("instructions").get("agents").clone(),
        ),
        ("active_set", active_set(state).into()),
        ("automatic_apply", false.into()),
    ])
}
fn save(context: &Context, state: &Value, mut plan: Plan) -> Result<(), String> {
    validate(state)?;
    plan.json(context.directory.join("library.json"), state)?;
    let changed = plan.commit(context, "library-edit")?;
    control::emit(object([
        ("ok", true.into()),
        ("changed_files", changed.into()),
        ("applied", false.into()),
        (
            "message",
            "Library saved. Apply it explicitly to the selected agent.".into(),
        ),
    ]))
}
fn mcp_summary(entry: &Value) -> Value {
    object([
        ("name", entry.get("name").clone()),
        ("transport", transport(entry).into()),
        ("enabled", (!off(entry)).into()),
        ("agents", entry.get("agents").clone()),
        (
            "has_env",
            entry
                .get("env")
                .as_object()
                .is_some_and(|v| !v.is_empty())
                .into(),
        ),
        (
            "has_headers",
            entry
                .get("headers")
                .as_object()
                .is_some_and(|v| !v.is_empty())
                .into(),
        ),
    ])
}
fn off(entry: &Value) -> bool {
    ["disabled", "hidden", "offHere"]
        .iter()
        .any(|k| entry.get(k).as_bool() == Some(true))
}
fn transport(entry: &Value) -> &str {
    entry
        .get("transport")
        .as_str()
        .unwrap_or(if entry.get("url").as_str().is_some() {
            "http"
        } else {
            "stdio"
        })
}
fn validate_mcp(entry: &Value) -> Result<(), String> {
    match transport(entry) {
        "stdio" => {
            if entry.get("command").as_str().is_none_or(str::is_empty) {
                return Err("stdio MCP needs a command".into());
            }
            str_list(entry.get("args"))?;
        }
        "http" | "sse" => {
            let url = entry.get("url").as_str().ok_or("remote MCP needs a URL")?;
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || url.chars().any(char::is_control)
            {
                return Err("MCP URL must use HTTP or HTTPS".into());
            }
        }
        _ => return Err("MCP transport must be stdio, http or sse".into()),
    }
    for key in ["env", "headers"] {
        match entry.get(key) {
            Value::Null => {}
            Value::Object(map) if map.values().all(|v| v.as_str().is_some()) => {}
            _ => return Err(format!("MCP {key} must map names to strings")),
        }
    }
    Ok(())
}
fn entry_index(entries: &[Value], id: &str) -> Result<usize, String> {
    entries
        .iter()
        .position(|v| v.get("name").as_str() == Some(id))
        .ok_or_else(|| format!("unknown library entry: {id}"))
}
fn mcp(context: &Context, state: &mut Value, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let mut entries = state.get("mcp").as_array().to_vec();
    if action == "list" {
        return control::emit(object([(
            "mcp",
            Value::Array(entries.iter().map(mcp_summary).collect()),
        )]));
    }
    if matches!(action, "apply" | "revert") {
        return apply_kind(context, state, args, "mcp");
    }
    if matches!(action, "set" | "add" | "update") {
        let input = control::option(args, "--file")?.ok_or("mcp set requires --file")?;
        let mut entry = json::read(Path::new(input))?;
        let id = args
            .get(1)
            .filter(|s| !s.starts_with('-'))
            .map(String::as_str)
            .or_else(|| entry.get("name").as_str())
            .ok_or("MCP entry needs a name")?
            .to_owned();
        name(&id)?;
        entry.set("name", id.as_str().into());
        validate_mcp(&entry)?;
        if let Some(agents) = agents_arg(args)? {
            entry.set("agents", agents);
        }
        let index = entries
            .iter()
            .position(|v| v.get("name").as_str() == Some(&id));
        if action == "add" && index.is_some() {
            return Err("MCP entry already exists; use set".into());
        }
        if action == "update" && index.is_none() {
            return Err("unknown MCP entry".into());
        }
        if let Some(i) = index {
            entries[i] = entry;
        } else {
            entries.push(entry);
        }
    } else {
        let id = name(args.get(1).ok_or("MCP name required")?)?;
        let i = entry_index(&entries, id)?;
        match action {
            "show" => { if let Some(file) = control::option(args, "--file")? { json::atomic_write(Path::new(file), entries[i].stringify().as_bytes())?; return control::emit(object([("ok", true.into())])); } return control::emit(mcp_summary(&entries[i])); }
            "remove" => { entries.remove(i); }
            "enable" | "disable" => { entries[i].set("disabled", (action == "disable").into()); if action == "enable" { entries[i].set("hidden", false.into()); if entries[i].get("agents").as_array().is_empty() && !entries[i].get("paused").as_array().is_empty() { let agents = entries[i].get("paused").clone(); entries[i].set("agents", agents); } } }
            "assign" => entries[i].set("agents", agents_arg(args)?.ok_or("assign requires --agents (comma-separated, empty to unassign)")?),
            _ => return Err("mcp expects list, show, set, add, update, remove, enable, disable, assign, apply or revert".into()),
        }
    }
    state.set("mcp", Value::Array(entries));
    save(context, state, Plan::default())
}

fn sets(state: &Value) -> Vec<Value> {
    let mut sets = state.get("instructions").get("sets").as_array().to_vec();
    if !sets.iter().any(|v| v.get("id").as_str() == Some("default")) {
        sets.insert(
            0,
            object([("id", "default".into()), ("name", "Default".into())]),
        );
    }
    sets
}
fn active_set(state: &Value) -> &str {
    state
        .get("instructions")
        .get("active")
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("default")
}
fn set_path(context: &Context, state: &Value, id: &str) -> Result<PathBuf, String> {
    name(id)?;
    let set = sets(state)
        .into_iter()
        .find(|v| v.get("id").as_str() == Some(id))
        .ok_or("unknown instruction set")?;
    if let Some(path) = set.get("file").as_str().filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("linked instruction file must be absolute".into());
        }
        return Ok(path);
    }
    Ok(if id == "default" {
        context.directory.join("library/instructions.md")
    } else {
        context
            .directory
            .join("library/instruction-sets")
            .join(format!("{id}.md"))
    })
}
fn extra_path(context: &Context, agent: &str) -> Result<PathBuf, String> {
    name(agent)?;
    Ok(context
        .directory
        .join("library/instructions")
        .join(format!("{agent}.md")))
}
fn instructions(context: &Context, state: &mut Value, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let mut config = state.get("instructions").clone();
    let mut all = sets(state);
    let mut plan = Plan::default();
    if action == "list" {
        return control::emit(object([
            ("sets", Value::Array(all)),
            ("active", active_set(state).into()),
            ("agents", config.get("agents").clone()),
        ]));
    }
    if matches!(action, "apply" | "revert") {
        return apply_kind(context, state, args, "instructions");
    }
    if action == "assign" {
        config.set(
            "agents",
            agents_arg(args)?.ok_or("assign requires --agents")?,
        );
    } else if matches!(action, "enable" | "disable") {
        let agent = name(control::option(args, "--agent")?.ok_or("--agent required")?)?;
        let mut agents = str_list(config.get("agents"))?;
        agents.retain(|v| v != agent);
        if action == "enable" {
            agents.push(agent.to_owned());
        }
        config.set(
            "agents",
            Value::Array(agents.into_iter().map(Value::from).collect()),
        );
    } else if action == "extra" {
        let agent = control::option(args, "--agent")?.ok_or("extra requires --agent")?;
        let input = control::option(args, "--file")?
            .ok_or("extra requires --file (an empty file clears it)")?;
        plan.text(
            extra_path(context, agent)?,
            &control::read_text(Path::new(input))?,
        )?;
    } else {
        let id = name(
            args.get(1)
                .filter(|v| !v.starts_with('-'))
                .map(String::as_str)
                .unwrap_or("default"),
        )?;
        let index = all.iter().position(|v| v.get("id").as_str() == Some(id));
        match action {
            "show" => { let path = if let Some(agent) = control::option(args, "--agent")? { extra_path(context, agent)? } else { set_path(context, state, id)? }; return control::emit(object([("id", id.into()), ("text", optional_text(&path)?.into())])); }
            "set" | "create" | "link" => {
                if action == "create" && index.is_some() && id != "default" { return Err("instruction set already exists".into()); }
                let mut set = index.map(|i| all[i].clone()).unwrap_or_else(|| object([("id", id.into()), ("name", id.into())]));
                if let Some(label) = control::option(args, "--name")? { set.set("name", label.into()); }
                let input = control::option(args, "--file")?.ok_or("set/link requires --file")?;
                let source = Path::new(input); let text = control::read_text(source)?;
                if action == "link" { let canonical = source.canonicalize().map_err(|e| e.to_string())?; set.set("file", canonical.to_string_lossy().into_owned().into()); }
                else { set.remove("file"); let target = if id == "default" { context.directory.join("library/instructions.md") } else { context.directory.join("library/instruction-sets").join(format!("{id}.md")) }; plan.text(target, &text)?; }
                if let Some(i) = index { all[i] = set; } else { all.push(set); }
            }
            "use" => { index.ok_or("unknown instruction set")?; config.set("active", id.into()); }
            "rename" => { let i = index.ok_or("unknown instruction set")?; all[i].set("name", control::option(args, "--name")?.ok_or("rename requires --name")?.into()); }
            "remove" => { if id == "default" { return Err("the default set cannot be removed; set it from an empty file".into()); } let i = index.ok_or("unknown instruction set")?; if all[i].get("file").as_str().is_none_or(str::is_empty) { plan.set(set_path(context, state, id)?, None)?; } all.remove(i); if active_set(state) == id { config.set("active", "default".into()); } }
            _ => return Err("instructions expects list, show, create, set, link, rename, use, remove, extra, assign, enable, disable, apply or revert".into()),
        }
    }
    if let Some(agents) = agents_arg(args)? {
        config.set("agents", agents);
    }
    config.set("sets", Value::Array(all));
    state.set("instructions", config);
    save(context, state, plan)
}

#[derive(Clone, Debug, PartialEq)]
struct FileData {
    bytes: Vec<u8>,
    executable: bool,
}
fn relative(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("path must stay below its skill directory".into());
    }
    Ok(path)
}
// Public skill resource names use '/' on every platform. The tree reader has
// already checked that these paths are relative UTF-8; filesystem paths and
// private ownership records retain their native representation.
fn resource_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
fn skills(context: &Context, state: &mut Value, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let mut entries = state.get("skills").as_array().to_vec();
    let root = context.directory.join("library/skills");
    if action == "list" {
        return control::emit(object([(
            "skills",
            Value::Array(
                entries
                    .iter()
                    .map(|v| {
                        object([
                            ("name", v.get("name").clone()),
                            ("enabled", (!off(v)).into()),
                            ("agents", v.get("agents").clone()),
                            ("source_kind", v.get("source").get("kind").clone()),
                            ("source_commit", v.get("source").get("commit").clone()),
                            (
                                "available",
                                v.get("name")
                                    .as_str()
                                    .is_some_and(|n| root.join(n).join("SKILL.md").is_file())
                                    .into(),
                            ),
                        ])
                    })
                    .collect(),
            ),
        )]));
    }
    if matches!(action, "apply" | "revert") {
        return apply_kind(context, state, args, "skills");
    }
    let id = name(args.get(1).ok_or("skill name required")?)?;
    let index = entries
        .iter()
        .position(|v| v.get("name").as_str() == Some(id));
    let mut plan = Plan::default();
    match action {
        "install" | "fetch" | "update" => {
            if action != "update" && index.is_some() { return Err("skill already exists; use update".into()); }
            if action == "update" && index.is_none() { return Err("unknown skill".into()); }
            let directory = control::option(args, "--dir")?;
            let explicit_git = control::option(args, "--git")?;
            if directory.is_some() && explicit_git.is_some() { return Err("select one skill source: --dir or --git".into()); }
            let previous = index.map(|i| entries[i].get("source")).unwrap_or(&Value::Null);
            let git = explicit_git.or_else(|| if directory.is_none() && previous.get("kind").as_str() == Some("git") { previous.get("url").as_str() } else { None });
            if action == "fetch" && git.is_none() { return Err("skills fetch requires --git URL".into()); }
            let dest = root.join(id);
            let current = if dest.exists() { read_tree(&dest, false)? } else { BTreeMap::new() };
            if action == "update" && previous.get("kind").as_str() == Some("git")
                && tree_record(&current) != *previous.get("files") && !args.iter().any(|arg| arg == "--force") {
                return Err("stored Git skill has local changes; review them or use --force to replace them".into());
            }
            // Force accepts this snapshot, never edits made while loading the source.
            for (path, file) in &current {
                plan.original.insert(dest.join(path), Some(file.clone()));
            }
            let registry = context.directory.join("library.json");
            plan.original.insert(registry.clone(), read_file(&registry)?);
            let (tree, source) = if let Some(url) = git {
                let reuse = explicit_git.is_none() || previous.get("url").as_str() == Some(url);
                let revision = control::option(args, "--ref")?.or_else(|| if reuse { previous.get("ref").as_str() } else { None }).unwrap_or("HEAD");
                let path = control::option(args, "--path")?.or_else(|| if reuse { previous.get("path").as_str() } else { None }).unwrap_or(".");
                git_source::fetch(&context.directory, url, revision, path)?
            } else {
                if control::option(args, "--ref")?.is_some() || control::option(args, "--path")?.is_some() { return Err("--ref and --path require a Git skill source".into()); }
                let dir = directory.map(PathBuf::from).or_else(|| previous.get("dir").as_str().map(PathBuf::from)).ok_or("install/update requires --dir PATH or --git URL")?;
                let tree = read_tree(&dir, true)?;
                let source = object([("kind", "folder".into()), ("dir", dir.canonicalize().map_err(|e| e.to_string())?.to_string_lossy().into_owned().into())]);
                (tree, source)
            };
            let latest = if dest.exists() { read_tree(&dest, false)? } else { BTreeMap::new() };
            if latest != current {
                return Err("stored skill changed while loading its source; review the changes and retry".into());
            }
            drop(latest);
            for path in tree.keys() {
                plan.original.entry(dest.join(path)).or_insert(None);
            }
            for path in current.keys() { if !tree.contains_key(path) { plan.set(dest.join(path), None)?; } }
            for (path, data) in tree { plan.set(dest.join(path), Some(data))?; }
            let mut entry = index.map(|i| entries[i].clone()).unwrap_or_else(|| object([("name", id.into()), ("agents", Value::Array(Vec::new()))]));
            entry.set("source", source);
            entry.set("offHere", false.into()); entry.remove("pending"); entry.remove("missing");
            if let Some(agents) = agents_arg(args)? { entry.set("agents", agents); }
            if let Some(i) = index { entries[i] = entry; } else { entries.push(entry); }
        }
        "create" => {
            if index.is_some() { return Err("skill already exists".into()); }
            let input = control::option(args, "--file")?.ok_or("create requires --file containing SKILL.md")?;
            plan.text(root.join(id).join("SKILL.md"), &control::read_text(Path::new(input))?)?;
            entries.push(object([("name", id.into()), ("agents", agents_arg(args)?.unwrap_or(Value::Array(Vec::new())))]));
        }
        "show" => { let i = index.ok_or("unknown skill")?; let files = read_tree(&root.join(id), true)?; let mut source = entries[i].get("source").clone(); source.remove("files"); return control::emit(object([("name", id.into()), ("agents", entries[i].get("agents").clone()), ("enabled", (!off(&entries[i])).into()), ("source", source), ("files", Value::Array(files.keys().map(|p| resource_path(p).into()).collect()))])); }
        "read" | "write" => {
            index.ok_or("unknown skill")?; let file = relative(control::option(args, "--file")?.unwrap_or("SKILL.md"))?; let path = root.join(id).join(file);
            if action == "read" { return control::emit(object([("text", control::read_text(&path)?.into())])); }
            let source = control::option(args, "--input")?.ok_or("write requires --input FILE")?;
            plan.text(path, &control::read_text(Path::new(source))?)?;
        }
        "remove" => { let i = index.ok_or("unknown skill")?; entries.remove(i); if root.join(id).exists() { for path in read_tree(&root.join(id), false)?.keys() { plan.set(root.join(id).join(path), None)?; } } }
        "enable" | "disable" => { let i = index.ok_or("unknown skill")?; entries[i].set("offHere", (action == "disable").into()); }
        "assign" => { let i = index.ok_or("unknown skill")?; entries[i].set("agents", agents_arg(args)?.ok_or("assign requires --agents")?); }
        _ => return Err("skills expects list, show, create, install, fetch, update, read, write, remove, enable, disable, assign, apply or revert".into()),
    }
    state.set("skills", Value::Array(entries));
    save(context, state, plan)
}

fn env_dir(key: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}
fn config_dir(context: &Context) -> PathBuf {
    if cfg!(windows) {
        env_dir("APPDATA", context.home.join("AppData/Roaming"))
    } else {
        env_dir("XDG_CONFIG_HOME", context.home.join(".config"))
    }
}
fn omp_dir(context: &Context) -> Result<PathBuf, String> {
    let root = std::env::var_os("PI_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(|p| context.home.join(p))
        .unwrap_or_else(|| context.home.join(".omp"));
    let profile = std::env::var("OMP_PROFILE")
        .or_else(|_| std::env::var("PI_PROFILE"))
        .unwrap_or_default();
    let profile = profile.trim();
    if !profile.is_empty() && profile != "default" {
        let stem = profile.split('.').next().unwrap_or("");
        let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
            || ((stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if profile.len() > 64
            || !profile.as_bytes()[0].is_ascii_alphanumeric()
            || profile.ends_with('.')
            || profile
                .bytes()
                .any(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)))
            || reserved
        {
            return Err("invalid omp profile; no library files were changed".into());
        }
        return Ok(root.join("profiles").join(profile).join("agent"));
    }
    Ok(env_dir("PI_CODING_AGENT_DIR", root.join("agent")))
}
struct Target {
    instructions: Option<PathBuf>,
    override_file: Option<PathBuf>,
    skills: Option<PathBuf>,
    mcp: Option<(PathBuf, &'static str, &'static str)>,
}
fn target(context: &Context, agent: &str) -> Result<Target, String> {
    let h = &context.home;
    let x = config_dir(context);
    let (dir, instruction, mcp, key, format, skill) = match agent {
        "claude" => (
            env_dir("CLAUDE_CONFIG_DIR", h.join(".claude")),
            Some("CLAUDE.md"),
            Some(".claude.json"),
            "mcpServers",
            "claude",
            true,
        ),
        "codex" => (
            env_dir("CODEX_HOME", h.join(".codex")),
            Some("AGENTS.md"),
            Some("config.toml"),
            "mcp_servers",
            "codex",
            true,
        ),
        "gemini" => (
            h.join(".gemini"),
            Some("GEMINI.md"),
            Some("settings.json"),
            "mcpServers",
            "gemini",
            true,
        ),
        "qwen" => (
            env_dir("QWEN_HOME", h.join(".qwen")),
            Some("QWEN.md"),
            Some("settings.json"),
            "mcpServers",
            "gemini",
            true,
        ),
        "agy" | "antigravity" => (
            h.join(".gemini/config"),
            Some("GEMINI.md"),
            Some("mcp_config.json"),
            "mcpServers",
            "antigravity",
            true,
        ),
        "opencode" => (
            env_dir("OPENCODE_CONFIG_DIR", x.join("opencode")),
            Some("AGENTS.md"),
            Some("opencode.json"),
            "mcp",
            "opencode",
            true,
        ),
        "copilot" => (
            env_dir("COPILOT_HOME", h.join(".copilot")),
            Some("copilot-instructions.md"),
            Some("mcp-config.json"),
            "mcpServers",
            "copilot",
            true,
        ),
        "cursor" => (
            h.join(".cursor"),
            None,
            Some("mcp.json"),
            "mcpServers",
            "cursor",
            true,
        ),
        "crush" => (
            if cfg!(windows) {
                env_dir("LOCALAPPDATA", h.join("AppData/Local")).join("crush")
            } else {
                x.join("crush")
            },
            Some("CRUSH.md"),
            Some("crush.json"),
            "mcp",
            "claude",
            true,
        ),
        "grok" => (
            env_dir("GROK_HOME", h.join(".grok")),
            Some("AGENTS.md"),
            Some("config.toml"),
            "mcp_servers",
            "grok",
            true,
        ),
        "pi" => (
            env_dir("PI_CODING_AGENT_DIR", h.join(".pi/agent")),
            Some("AGENTS.md"),
            Some("mcp.json"),
            "mcpServers",
            "pi",
            true,
        ),
        "omp" => (
            omp_dir(context)?,
            Some("AGENTS.md"),
            Some("mcp.json"),
            "mcpServers",
            "omp",
            true,
        ),
        "omo" => (
            env_dir(
                "OMO_CODING_AGENT_DIR",
                env_dir("SENPI_CODING_AGENT_DIR", h.join(".omo/agent")),
            ),
            Some("AGENTS.md"),
            Some("mcp.json"),
            "mcpServers",
            "pi",
            true,
        ),
        "zed" => (
            if cfg!(target_os = "macos") {
                h.join(".config/zed")
            } else {
                x.join(if cfg!(windows) { "Zed" } else { "zed" })
            },
            None,
            Some("settings.json"),
            "context_servers",
            "zed",
            false,
        ),
        "vscode" => (
            if cfg!(target_os = "macos") {
                h.join("Library/Application Support/Code/User")
            } else {
                x.join("Code/User")
            },
            None,
            Some("mcp.json"),
            "servers",
            "vscode",
            false,
        ),
        "claude-desktop" => (
            if cfg!(target_os = "macos") {
                h.join("Library/Application Support/Claude")
            } else {
                x.join("Claude")
            },
            None,
            Some("claude_desktop_config.json"),
            "mcpServers",
            "desktop",
            false,
        ),
        "kiro-cli" => (
            h.join(".kiro"),
            Some("steering/AGENTS.md"),
            Some("settings/mcp.json"),
            "mcpServers",
            "kiro",
            true,
        ),
        "dsh" => (
            env_dir("DSH_HOME", h.join(".dsh")),
            Some("AGENTS.md"),
            None,
            "",
            "",
            true,
        ),
        "shared" => (h.join(".agents"), Some("AGENTS.md"), None, "", "", true),
        _ => {
            return Err(format!(
                "no native library adapter for {agent}; existing files and library metadata are preserved"
            ));
        }
    };
    let override_file = if matches!(agent, "codex" | "pi" | "omo") {
        Some(dir.join("AGENTS.override.md"))
    } else {
        None
    };
    let mut instructions = instruction.map(|v| dir.join(v));
    if agent == "crush" {
        instructions = Some(env_dir("XDG_CONFIG_HOME", h.join(".config")).join("crush/CRUSH.md"));
    }
    if matches!(agent, "pi" | "omo") {
        for file in ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"] {
            let path = dir.join(file);
            if path.exists() {
                instructions = Some(path);
                break;
            }
        }
    }
    if matches!(agent, "gemini" | "qwen") {
        let settings = dir.join("settings.json");
        if settings.exists() {
            let data = json::parse_jsonc(&control::read_text(&settings)?)?;
            let configured = data.get("context").get("fileName");
            if let Some(file) = configured
                .as_str()
                .or_else(|| configured.as_array().first().and_then(Value::as_str))
            {
                let file = relative(file)?;
                instructions = Some(dir.join(file));
            }
        }
    }
    let mut mcp = mcp.map(|f| (dir.join(f), key, format));
    if agent == "claude" && dir == h.join(".claude") {
        mcp.as_mut().unwrap().0 = h.join(".claude.json");
    }
    if agent == "opencode" {
        let path = dir.join("opencode.jsonc");
        if path.exists() {
            mcp.as_mut().unwrap().0 = path;
        }
    }
    if agent == "pi" {
        let settings = dir.join("settings.json");
        if settings.exists() {
            let data = json::parse_jsonc(&control::read_text(&settings)?)?;
            let adapter = data.get("packages").as_array().iter().any(|v| {
                v.as_str()
                    .or_else(|| v.get("source").as_str())
                    .is_some_and(|s| s.contains("pi-mcp-adapter"))
            });
            let extension = data.get("packages").as_array().iter().any(|v| {
                v.as_str()
                    .or_else(|| v.get("source").as_str())
                    .is_some_and(|s| s.contains("pi-mcp-extension"))
            });
            if adapter && extension {
                return Err(
                    "Pi has two MCP extensions; select one before applying the library".into(),
                );
            }
            if adapter {
                mcp = Some((dir.join("mcp-adapter.json"), "mcpServers", "pi-adapter"));
            } else if extension {
                mcp = Some((h.join(".pi/agent/mcp.json"), "mcpServers", "pi-extension"));
            } else if data
                .get("extensions")
                .as_array()
                .iter()
                .any(|v| v.as_str() == Some("-builtin:mcp"))
            {
                mcp = None;
            }
        }
    }
    Ok(Target {
        instructions,
        override_file,
        skills: skill.then(|| dir.join("skills")),
        mcp,
    })
}
fn target_list(context: &Context) -> Result<(), String> {
    let mut items = Vec::new();
    for id in [
        "claude",
        "codex",
        "gemini",
        "qwen",
        "antigravity",
        "opencode",
        "copilot",
        "cursor",
        "crush",
        "grok",
        "pi",
        "omo",
        "omp",
        "zed",
        "vscode",
        "claude-desktop",
        "kiro-cli",
        "dsh",
        "shared",
    ] {
        match target(context, id) {
            Ok(t) => items.push(object([
                ("agent", id.into()),
                ("instructions", t.instructions.is_some().into()),
                ("skills", t.skills.is_some().into()),
                ("mcp", t.mcp.is_some().into()),
                (
                    "override_present",
                    t.override_file.as_ref().is_some_and(|p| p.exists()).into(),
                ),
                ("error", Value::Null),
            ])),
            Err(e) => items.push(object([("agent", id.into()), ("error", e.into())])),
        }
    }
    control::emit(object([("targets", Value::Array(items))]))
}
fn replace_refs(value: &Value, format: &str) -> Result<Value, String> {
    match value {
        Value::String(text) => {
            if !text.contains("${") {
                return Ok(value.clone());
            }
            if matches!(
                format,
                "zed" | "antigravity" | "desktop" | "pi-extension" | "codex"
            ) {
                return Err(format!(
                    "{format} does not expand environment references here; use a literal value or an agent that supports references"
                ));
            }
            if matches!(format, "opencode" | "cursor" | "vscode") {
                let mut out = String::new();
                let mut rest = text.as_str();
                while let Some(start) = rest.find("${") {
                    out.push_str(&rest[..start]);
                    let tail = &rest[start + 2..];
                    let end = tail.find('}').ok_or("unclosed environment reference")?;
                    let key = &tail[..end];
                    if key.is_empty()
                        || key
                            .bytes()
                            .any(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
                    {
                        return Err("this agent cannot represent the environment reference".into());
                    }
                    if format == "opencode" {
                        out.push_str(&format!("{{env:{key}}}"));
                    } else {
                        out.push_str(&format!("${{env:{key}}}"));
                    }
                    rest = &tail[end + 1..];
                }
                out.push_str(rest);
                Ok(out.into())
            } else {
                Ok(value.clone())
            }
        }
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(|v| replace_refs(v, format))
                .collect::<Result<_, _>>()?,
        )),
        Value::Object(map) => Ok(Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), replace_refs(v, format)?)))
                .collect::<Result<_, String>>()?,
        )),
        _ => Ok(value.clone()),
    }
}
fn whole_ref(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}
fn encode_mcp(entry: &Value, format: &str) -> Result<Value, String> {
    validate_mcp(entry)?;
    let remote = transport(entry) != "stdio";
    if transport(entry) == "sse" && matches!(format, "codex" | "zed" | "pi" | "kiro") {
        return Err(format!("{format} does not support SSE MCP transport"));
    }
    if remote && format == "desktop" {
        return Err("Claude Desktop's local config supports stdio MCP only".into());
    }
    let mut headers = entry.get("headers").clone();
    if let Some(selected) = entry.get("account").as_str().filter(|v| !v.is_empty()) {
        let account = entry
            .get("accounts")
            .as_array()
            .iter()
            .find(|v| v.get("id").as_str() == Some(selected))
            .ok_or("selected MCP header account is missing")?;
        if let Some(map) = account.get("headers").as_object() {
            for (key, value) in map {
                let duplicates = headers
                    .as_object()
                    .map(|m| {
                        m.keys()
                            .filter(|k| k.eq_ignore_ascii_case(key))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for old in duplicates {
                    headers.remove(&old);
                }
                headers.set(key, value.clone());
            }
        }
    }
    let mut out = object([]);
    if remote {
        out.set(
            match format {
                "gemini" if transport(entry) == "http" => "httpUrl",
                "antigravity" => "serverUrl",
                _ => "url",
            },
            entry.get("url").clone(),
        );
        if let Some(map) = headers.as_object()
            && !map.is_empty()
        {
            out.set(
                if format == "codex" {
                    "http_headers"
                } else {
                    "headers"
                },
                headers,
            );
        }
    } else {
        if format == "opencode" {
            let mut command = vec![entry.get("command").clone()];
            command.extend_from_slice(entry.get("args").as_array());
            out.set("command", Value::Array(command));
        } else {
            out.set("command", entry.get("command").clone());
            out.set("args", Value::Array(entry.get("args").as_array().to_vec()));
        }
        if let Some(map) = entry.get("env").as_object()
            && !map.is_empty()
        {
            out.set(
                if format == "opencode" {
                    "environment"
                } else {
                    "env"
                },
                Value::Object(map.clone()),
            );
        }
    }
    match format {
        "claude" | "vscode" => out.set("type", transport(entry).into()),
        "opencode" => {
            out.set("type", if remote { "remote" } else { "local" }.into());
            out.set("enabled", true.into());
        }
        "copilot" => {
            out.set(
                "type",
                if remote { transport(entry) } else { "local" }.into(),
            );
            out.set("tools", Value::Array(vec!["*".into()]));
        }
        "omp" if remote => out.set("type", transport(entry).into()),
        "antigravity" if transport(entry) == "sse" => out.set("type", "sse".into()),
        "pi-adapter" | "pi-extension" if remote => {
            let t = if transport(entry) == "sse" {
                "sse"
            } else {
                "streamable-http"
            };
            out.set("transport", t.into());
            out.set("httpTransport", t.into());
        }
        _ => {}
    }
    if format == "codex" {
        let mut literal = object([]);
        let mut references = object([]);
        let mut pass = Vec::new();
        let field = if remote { "http_headers" } else { "env" };
        if let Some(map) = out.get(field).as_object() {
            for (key, value) in map {
                let text = value.as_str().ok_or("MCP value is not a string")?;
                if remote
                    && key.eq_ignore_ascii_case("authorization")
                    && text.starts_with("Bearer ")
                    && whole_ref(&text[7..]).is_some()
                {
                    references.set("__bearer", whole_ref(&text[7..]).unwrap().into());
                } else if let Some(variable) = whole_ref(text) {
                    if remote {
                        references.set(key, variable.into());
                    } else if variable == key {
                        pass.push(variable.into());
                    } else {
                        return Err(
                            "Codex can pass an environment variable only under its original name"
                                .into(),
                        );
                    }
                } else if text.contains("${") {
                    return Err("Codex requires whole-variable references (or Authorization: Bearer ${NAME})".into());
                } else {
                    literal.set(key, value.clone());
                }
            }
        }
        out.remove(field);
        if literal.as_object().is_some_and(|v| !v.is_empty()) {
            out.set(field, literal);
        }
        if let Some(bearer) = references.remove("__bearer") {
            out.set("bearer_token_env_var", bearer);
        }
        if references.as_object().is_some_and(|v| !v.is_empty()) {
            out.set("env_http_headers", references);
        }
        if !pass.is_empty() {
            out.set("env_vars", Value::Array(pass));
        }
        if let Some(seconds) = entry.get("startupTimeout").as_u64().filter(|v| *v > 0) {
            out.set("startup_timeout_sec", seconds.into());
        }
        if !entry.get("disabledTools").as_array().is_empty() {
            str_list(entry.get("disabledTools"))?;
            out.set("disabled_tools", entry.get("disabledTools").clone());
        }
    } else if !entry.get("disabledTools").as_array().is_empty() {
        return Err(format!(
            "tool exclusions for {format} require its native permission configuration; no entries were changed"
        ));
    }
    replace_refs(&out, format)
}

fn optional_text(path: &Path) -> Result<String, String> {
    match fs::metadata(path) {
        Ok(_) => control::read_text(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.to_string()),
    }
}
fn reject_links(path: &Path) -> Result<(), String> {
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "refusing a symlink in managed path {}",
                    part.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}
fn read_file(path: &Path) -> Result<Option<FileData>, String> {
    reject_links(path)?;
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
        return Err(format!(
            "{} must be a regular file of at most 8 MiB",
            path.display()
        ));
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some(FileData {
        bytes: fs::read(path).map_err(|e| e.to_string())?,
        executable,
    }))
}
fn read_tree(root: &Path, require_skill: bool) -> Result<BTreeMap<PathBuf, FileData>, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err("skill source is not a directory".into());
    }
    let mut output = BTreeMap::new();
    let mut total = 0usize;
    let mut active = BTreeSet::new();
    fn walk(
        root: &Path,
        at: &Path,
        rel: &Path,
        output: &mut BTreeMap<PathBuf, FileData>,
        total: &mut usize,
        active: &mut BTreeSet<PathBuf>,
        depth: usize,
    ) -> Result<(), String> {
        let real = at.canonicalize().map_err(|e| e.to_string())?;
        if !real.starts_with(root) || depth > 32 || !active.insert(real.clone()) {
            return Err(
                "skill contains an escaping link, cycle or excessive directory depth".into(),
            );
        }
        let mut entries = fs::read_dir(&real)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|v| v.file_name());
        for entry in entries {
            if entry.file_name() == ".git" {
                continue;
            }
            if entry.file_name().to_str().is_none() {
                return Err("skill paths must be valid UTF-8".into());
            }
            let path = entry.path().canonicalize().map_err(|e| e.to_string())?;
            if !path.starts_with(root) {
                return Err("skill link leaves its source directory".into());
            }
            let dest = rel.join(entry.file_name());
            if path.is_dir() {
                walk(root, &path, &dest, output, total, active, depth + 1)?;
            } else {
                let data = read_file(&path)?.ok_or("skill file disappeared")?;
                *total = total
                    .checked_add(data.bytes.len())
                    .ok_or("skill size overflow")?;
                if *total > MAX_TREE || output.len() >= MAX_FILES {
                    return Err("library file tree exceeds 32 MiB or 2048 files".into());
                }
                output.insert(dest, data);
            }
        }
        active.remove(&real);
        Ok(())
    }
    walk(
        &root,
        &root,
        Path::new(""),
        &mut output,
        &mut total,
        &mut active,
        0,
    )?;
    if require_skill && !output.contains_key(Path::new("SKILL.md")) {
        return Err("skill directory must contain SKILL.md".into());
    }
    Ok(output)
}
#[derive(Default)]
struct Plan {
    files: BTreeMap<PathBuf, Option<FileData>>,
    original: BTreeMap<PathBuf, Option<FileData>>,
}
impl Plan {
    fn set(&mut self, path: PathBuf, data: Option<FileData>) -> Result<(), String> {
        reject_links(&path)?;
        if !self.original.contains_key(&path) {
            self.original.insert(path.clone(), read_file(&path)?);
        }
        self.files.insert(path, data);
        if self.files.len() > 4096
            || self
                .files
                .values()
                .filter_map(|v| v.as_ref())
                .map(|v| v.bytes.len())
                .sum::<usize>()
                > 64 * 1024 * 1024
        {
            return Err("library operation exceeds its 64 MiB/4096-file bound".into());
        }
        Ok(())
    }
    fn text(&mut self, path: PathBuf, text: &str) -> Result<(), String> {
        self.set(
            path,
            Some(FileData {
                bytes: text.as_bytes().to_vec(),
                executable: false,
            }),
        )
    }
    fn json(&mut self, path: PathBuf, value: &Value) -> Result<(), String> {
        self.text(path, &(value.stringify() + "\n"))
    }
    fn text_at(&mut self, path: &Path) -> Result<String, String> {
        if !self.original.contains_key(path) {
            self.original.insert(path.to_path_buf(), read_file(path)?);
        }
        match self
            .files
            .get(path)
            .unwrap_or_else(|| self.original.get(path).unwrap())
        {
            Some(file) => String::from_utf8(file.bytes.clone())
                .map_err(|_| "managed configuration is not UTF-8".into()),
            None => Ok(String::new()),
        }
    }
    fn changes(&self) -> Result<Vec<FileChange>, String> {
        let mut result = Vec::new();
        for (path, next) in &self.files {
            let before = read_file(path)?;
            if self.original.get(path) != Some(&before) {
                return Err("a library target changed while preparing the operation; retry after reviewing it".into());
            }
            if &before != next {
                result.push((path.clone(), before, next.clone()));
            }
        }
        Ok(result)
    }
    fn commit(&self, context: &Context, label: &str) -> Result<usize, String> {
        let changes = self.changes()?;
        if changes.is_empty() {
            return Ok(0);
        }
        let paths = changes
            .iter()
            .map(|(p, _, _)| p.clone())
            .collect::<Vec<_>>();
        let backup = control::backup(context, &paths, label)?;
        let mut manifest = json::read(&backup.join("manifest.json"))?;
        let mut records = manifest.get("files").as_array().to_vec();
        for (record, (_, before, _)) in records.iter_mut().zip(&changes) {
            record.set(
                "executable",
                before.as_ref().is_some_and(|f| f.executable).into(),
            );
        }
        manifest.set("files", Value::Array(records));
        json::atomic_write(
            &backup.join("manifest.json"),
            manifest.stringify().as_bytes(),
        )?;
        for (i, (path, _, next)) in changes.iter().enumerate() {
            if let Err(error) = write_file(path, next.as_ref()) {
                let mut restored = true;
                for (p, old, _) in changes[..=i].iter().rev() {
                    restored &= write_file(p, old.as_ref()).is_ok();
                }
                return Err(format!(
                    "{error}; rollback {}; private backup {}",
                    if restored {
                        "completed"
                    } else {
                        "needs recovery"
                    },
                    backup.display()
                ));
            }
        }
        for (path, _, next) in &changes {
            if next.is_none()
                && let Some(parent) = path.parent()
            {
                let _ = fs::remove_dir(parent);
            }
        }
        Ok(changes.len())
    }
}
type FileChange = (PathBuf, Option<FileData>, Option<FileData>);
fn write_file(path: &Path, file: Option<&FileData>) -> Result<(), String> {
    reject_links(path)?;
    match file {
        Some(file) => {
            json::atomic_write(path, &file.bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    path,
                    fs::Permissions::from_mode(if file.executable { 0o700 } else { 0o600 }),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        None => match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        },
    }
    Ok(())
}

fn apply_kind(context: &Context, state: &Value, args: &[String], kind: &str) -> Result<(), String> {
    let mut args = args.to_vec();
    args.push("--kind".into());
    args.push(kind.into());
    apply(context, state, &args)
}
fn apply(context: &Context, state: &Value, args: &[String]) -> Result<(), String> {
    let agent = name(control::option(args, "--agent")?.ok_or("apply/revert requires --agent")?)?;
    let target = target(context, agent)?;
    let kind = control::option(args, "--kind")?.unwrap_or("all");
    if !matches!(kind, "all" | "mcp" | "skills" | "instructions") {
        return Err("--kind must be all, mcp, skills or instructions".into());
    }
    let reverting = args.first().is_some_and(|v| v == "revert");
    let manifest_path = context.directory.join("library-managed.json");
    let mut manifest = read_state(
        &manifest_path,
        object([("format", 1u64.into()), ("agents", object([]))]),
    )?;
    if manifest.get("format").as_u64() != Some(1)
        || !matches!(manifest.get("agents"), Value::Object(_))
    {
        return Err("unsupported or corrupt library ownership record".into());
    }
    let mut agents = manifest.get("agents").clone();
    let mut owned = agents.get(agent).clone();
    if matches!(owned, Value::Null) {
        owned = object([]);
    }
    if !matches!(owned, Value::Object(_)) {
        return Err("invalid agent library ownership record".into());
    }
    let mut plan = Plan::default();
    let mut shared = 0usize;
    let mut servers = 0usize;
    let mut skill_count = 0usize;
    let mut warnings = Vec::new();
    if matches!(kind, "all" | "mcp") {
        let wanted = if reverting {
            Vec::new()
        } else {
            state
                .get("mcp")
                .as_array()
                .iter()
                .filter(|v| assigned(v, agent, true))
                .collect::<Vec<_>>()
        };
        let old = owned.get("mcp");
        if !wanted.is_empty() || !matches!(old, Value::Null) {
            let (path, key, format) = target.mcp.as_ref().ok_or_else(|| format!("{agent} has no supported native MCP configuration adapter; no changes were made"))?;
            let mut desired = BTreeMap::new();
            for entry in wanted {
                desired.insert(
                    entry.get("name").as_str().unwrap().to_owned(),
                    encode_mcp(entry, format)?,
                );
            }
            servers = desired.len();
            let records = if matches!(*format, "codex" | "grok") {
                apply_toml(&mut plan, path, old, &desired)?
            } else {
                apply_json(
                    &mut plan,
                    path,
                    key,
                    old,
                    &desired,
                    control::flag(args, "--adopt-existing"),
                    &mut shared,
                )?
            };
            owned.set("mcp", records);
        } else if kind == "mcp" && target.mcp.is_none() {
            return Err(format!("{agent} has no supported native MCP adapter"));
        }
    }
    if matches!(kind, "all" | "instructions") {
        let enabled = !reverting && assigned(state.get("instructions"), agent, false);
        if enabled || !matches!(owned.get("instructions"), Value::Null) {
            let path = target
                .instructions
                .as_ref()
                .ok_or_else(|| format!("{agent} has no user-wide instruction file"))?;
            if enabled && target.override_file.as_ref().is_some_and(|p| p.exists()) {
                return Err("AGENTS.override.md takes precedence; move or edit it explicitly before applying shared instructions".into());
            }
            let wanted = if enabled {
                let source = set_path(context, state, active_set(state))?;
                if source.exists()
                    && path.exists()
                    && source.canonicalize().ok() == path.canonicalize().ok()
                {
                    return Err("the linked instruction set is the destination file; choose a separate source to avoid self-copying".into());
                }
                if !source.exists()
                    && (active_set(state) != "default"
                        || sets(state).iter().any(|v| {
                            v.get("id").as_str() == Some("default")
                                && v.get("file").as_str().is_some_and(|s| !s.is_empty())
                        }))
                {
                    return Err("the selected instruction source is missing; existing agent files were not changed".into());
                }
                let shared_text = optional_text(&source)?;
                let extra = optional_text(&extra_path(context, agent)?)?;
                [shared_text.trim(), extra.trim()]
                    .into_iter()
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            } else {
                String::new()
            };
            let record = apply_instructions(&mut plan, path, owned.get("instructions"), &wanted)?;
            owned.set("instructions", record);
        } else if kind == "instructions" && target.instructions.is_none() {
            return Err(format!("{agent} has no user-wide instruction file"));
        }
    }
    if matches!(kind, "all" | "skills") {
        let wanted = if reverting {
            Vec::new()
        } else {
            state
                .get("skills")
                .as_array()
                .iter()
                .filter(|v| assigned(v, agent, false))
                .collect::<Vec<_>>()
        };
        if !wanted.is_empty() || !matches!(owned.get("skills"), Value::Null) {
            let dest = target
                .skills
                .as_ref()
                .ok_or_else(|| format!("{agent} has no supported user-wide skill directory"))?;
            let mut desired = BTreeMap::new();
            for entry in wanted {
                if entry.get("pending").as_bool() == Some(true)
                    || !entry.get("missing").as_array().is_empty()
                {
                    return Err("a selected skill has missing/unfetched files; update it from a complete directory before applying".into());
                }
                let id = entry.get("name").as_str().unwrap();
                desired.insert(
                    id.to_owned(),
                    read_tree(&context.directory.join("library/skills").join(id), true)?,
                );
            }
            skill_count = desired.len();
            owned.set(
                "skills",
                apply_skills(&mut plan, dest, owned.get("skills"), &desired)?,
            );
        } else if kind == "skills" && target.skills.is_none() {
            return Err(format!(
                "{agent} has no supported user-wide skill directory"
            ));
        }
    }
    for field in ["mcpHub", "skillRouter", "sharedSkills", "autoUpdateSkills"] {
        if state.get(field).as_bool() == Some(true) {
            warnings.push(format!("{field} is preserved as imported metadata; this command performs only explicit native-file application").into());
        }
    }
    if shared > 0 {
        warnings.push("Identical existing MCP entries were left unowned. Use --adopt-existing explicitly if zcalld should update/remove them later.".into());
    }
    agents.set(agent, owned);
    manifest.set("agents", agents);
    plan.json(manifest_path, &manifest)?;
    let changes = plan.changes()?;
    let changed_paths = changes
        .iter()
        .map(|(p, _, _)| p.to_string_lossy().into_owned().into())
        .collect();
    let dry = control::flag(args, "--dry-run");
    if !dry {
        plan.commit(
            context,
            if reverting {
                "library-revert"
            } else {
                "library-apply"
            },
        )?;
    }
    control::emit(object([
        ("ok", true.into()), ("agent", agent.into()), ("kind", kind.into()), ("dry_run", dry.into()),
        ("changed_files", changes.len().into()), ("paths", Value::Array(changed_paths)),
        ("servers", servers.into()), ("skills", skill_count.into()), ("unowned_equal_servers", shared.into()),
        ("warnings", Value::Array(warnings)), ("restart_required", false.into()),
        ("message", if dry { "Preview only. No agent files were changed." } else { "Agent files updated. Start a new agent session to load its instructions and skills; the gateway does not need a restart." }.into()),
    ]))
}
fn records(value: &Value) -> Result<BTreeMap<String, Value>, String> {
    match value {
        Value::Null => Ok(BTreeMap::new()),
        Value::Object(v) => Ok(v.clone()),
        _ => Err("invalid library ownership record".into()),
    }
}
fn check_record_path(record: &Value, path: &Path) -> Result<(), String> {
    if !matches!(record, Value::Null)
        && record.get("path").as_str() != Some(path.to_string_lossy().as_ref())
    {
        return Err("agent configuration location changed; revert the old library binding before applying to a new path".into());
    }
    Ok(())
}
fn json_at<'a>(value: &'a Value, path: &[String]) -> &'a Value {
    let mut current = value;
    for key in path {
        current = current.get(key);
    }
    current
}
fn apply_json(
    plan: &mut Plan,
    path: &Path,
    key: &str,
    previous: &Value,
    desired: &BTreeMap<String, Value>,
    adopt: bool,
    shared: &mut usize,
) -> Result<Value, String> {
    let mut text = plan.text_at(path)?;
    if text.trim().is_empty() {
        text = "{}\n".into();
    }
    let previous = records(previous)?;
    let mut current = json::parse_jsonc(&text)?;
    let mut base = vec![key.to_owned()];
    if key == "mcp"
        && matches!(current.get("mcp").get("servers"), Value::Object(_))
        && ["type", "command", "url"]
            .iter()
            .all(|key| matches!(current.get("mcp").get("servers").get(key), Value::Null))
    {
        base.push("servers".into());
    }
    let names = previous
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut output = BTreeMap::new();
    for id in names {
        let mut member = base.clone();
        member.push(id.clone());
        let before = json_at(&current, &member);
        let record = previous.get(&id);
        if let Some(record) = record {
            check_record_path(record, path)?;
            if record.get("keys")
                != &Value::Array(member.iter().cloned().map(Value::from).collect())
            {
                return Err("MCP layout changed; revert the previous binding first".into());
            }
            if before != record.get("written") {
                return Err(format!(
                    "MCP entry {id} changed outside zcalld; no files were changed"
                ));
            }
        }
        if let Some(next) = desired.get(&id) {
            let mut next = next.clone();
            if base.len() == 2 && key == "mcp" {
                next.remove("enabled");
            }
            if record.is_none() && !matches!(before, Value::Null) {
                if before != &next {
                    return Err(format!(
                        "MCP entry {id} already belongs to another writer; review or rename it before applying"
                    ));
                }
                if !adopt {
                    *shared += 1;
                    continue;
                }
            }
            text = json::patch_json(
                &text,
                &member.iter().map(String::as_str).collect::<Vec<_>>(),
                &next,
            )?;
            output.insert(
                id,
                object([
                    ("path", path.to_string_lossy().into_owned().into()),
                    (
                        "keys",
                        Value::Array(member.into_iter().map(Value::from).collect()),
                    ),
                    ("written", next),
                ]),
            );
        } else if record.is_some() {
            text = remove_json_member(&text, &member)?;
        }
        current = json::parse_jsonc(&text)?;
    }
    plan.text(path.to_path_buf(), &text)?;
    Ok(Value::Object(output))
}

// JSONC is validated first. This scanner only finds byte spans to remove one member;
// it leaves every unrelated value, whitespace and comment exactly as written.
fn skip_space(text: &[u8], mut at: usize) -> usize {
    loop {
        while at < text.len() && text[at].is_ascii_whitespace() {
            at += 1;
        }
        if text[at..].starts_with(b"//") {
            while at < text.len() && text[at] != b'\n' {
                at += 1;
            }
        } else if text[at..].starts_with(b"/*") {
            at += 2;
            while at + 1 < text.len() && !text[at..].starts_with(b"*/") {
                at += 1;
            }
            at = (at + 2).min(text.len());
        } else {
            return at;
        }
    }
}
fn string_end(text: &[u8], mut at: usize) -> usize {
    at += 1;
    while at < text.len() {
        match text[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    at
}
fn value_end(text: &[u8], at: usize) -> usize {
    if text[at] == b'"' {
        return string_end(text, at);
    }
    if !matches!(text[at], b'{' | b'[') {
        let mut end = at;
        while end < text.len()
            && !matches!(
                text[end],
                b',' | b'}' | b']' | b'/' | b' ' | b'\n' | b'\r' | b'\t'
            )
        {
            end += 1;
        }
        return end;
    }
    let mut depth = 0;
    let mut end = at;
    while end < text.len() {
        match text[end] {
            b'"' => {
                end = string_end(text, end);
                continue;
            }
            b'/' => {
                let next = skip_space(text, end);
                if next != end {
                    end = next;
                    continue;
                }
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return end + 1;
                }
            }
            _ => {}
        }
        end += 1;
    }
    end
}
fn remove_json_member(text: &str, path: &[String]) -> Result<String, String> {
    json::parse_jsonc(text)?;
    let bytes = text.as_bytes();
    let mut object_start = skip_space(bytes, 0);
    for (depth, key) in path.iter().enumerate() {
        if bytes.get(object_start) != Some(&b'{') {
            return Err("JSON member parent is not an object".into());
        }
        let mut at = skip_space(bytes, object_start + 1);
        let mut preceding_comma = None;
        let mut found = None;
        while bytes.get(at) == Some(&b'"') {
            let key_end = string_end(bytes, at);
            let decoded = json::parse(&bytes[at..key_end])?;
            let colon = skip_space(bytes, key_end);
            let start = skip_space(bytes, colon + 1);
            let end = value_end(bytes, start);
            let next = skip_space(bytes, end);
            let comma = (bytes.get(next) == Some(&b',')).then_some(next);
            if decoded.as_str() == Some(key) {
                found = Some((at, start, end, comma, preceding_comma));
                break;
            }
            if let Some(comma) = comma {
                preceding_comma = Some(comma);
                at = skip_space(bytes, comma + 1);
            } else {
                break;
            }
        }
        let Some((key_start, start, end, after_comma, before_comma)) = found else {
            return Ok(text.to_owned());
        };
        if depth + 1 == path.len() {
            let mut spans = vec![(key_start, end)];
            if let Some(comma) = after_comma.or(before_comma) {
                spans.push((comma, comma + 1));
            }
            spans.sort_unstable();
            let mut out = String::new();
            let mut from = 0;
            for (a, b) in spans {
                out.push_str(&text[from..a]);
                from = b;
            }
            out.push_str(&text[from..]);
            json::parse_jsonc(&out)?;
            return Ok(out);
        }
        object_start = start;
    }
    Err("cannot remove the JSON document root".into())
}
fn block_range(text: &str, begin: &str, end: &str) -> Result<Option<(usize, usize)>, String> {
    let starts = text
        .match_indices(begin)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let ends = text.match_indices(end).map(|(i, _)| i).collect::<Vec<_>>();
    if starts.is_empty() && ends.is_empty() {
        return Ok(None);
    }
    if starts.len() != 1 || ends.len() != 1 || starts[0] >= ends[0] {
        return Err("managed text markers are malformed or repeated; no files were changed".into());
    }
    Ok(Some((starts[0], ends[0] + end.len())))
}
fn replace_block(
    text: &str,
    begin: &str,
    end: &str,
    old: Option<&str>,
    next: &str,
) -> Result<String, String> {
    match block_range(text, begin, end)? {
        Some((a, b)) => {
            if old != Some(&text[a..b]) {
                return Err("managed text changed outside zcalld; no files were changed".into());
            }
            Ok(format!("{}{}{}", &text[..a], next, &text[b..]))
        }
        None if old.is_some() => {
            Err("managed text was removed outside zcalld; no files were changed".into())
        }
        None if next.is_empty() => Ok(text.to_owned()),
        None => Ok(format!(
            "{}{}{}\n",
            text,
            if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            },
            next
        )),
    }
}
fn apply_instructions(
    plan: &mut Plan,
    path: &Path,
    previous: &Value,
    desired: &str,
) -> Result<Value, String> {
    check_record_path(previous, path)?;
    let text = plan.text_at(path)?;
    if matches!(previous, Value::Null) && text.contains("<!-- magpie:begin") && !desired.is_empty()
    {
        return Err("the destination has a Magpie instruction block; review/remove that block explicitly before adding another writer".into());
    }
    let next = if desired.is_empty() {
        String::new()
    } else {
        format!(
            "{BEGIN}\n{}\n{END}",
            desired.replace("<!-- zcalld:", "<!-- zcalld :")
        )
    };
    let updated = replace_block(&text, BEGIN, END, previous.get("written").as_str(), &next)?;
    plan.text(path.to_path_buf(), &updated)?;
    Ok(if next.is_empty() {
        Value::Null
    } else {
        object([
            ("path", path.to_string_lossy().into_owned().into()),
            ("written", next.into()),
        ])
    })
}
fn toml_value(value: &Value) -> Result<String, String> {
    match value {
        Value::String(_) | Value::Bool(_) | Value::Number(_) => Ok(value.stringify()),
        Value::Array(items) => Ok(format!(
            "[{}]",
            items
                .iter()
                .map(toml_value)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        )),
        Value::Object(map) => Ok(format!(
            "{{ {} }}",
            map.iter()
                .map(|(k, v)| Ok(format!(
                    "{} = {}",
                    Value::from(k.as_str()).stringify(),
                    toml_value(v)?
                )))
                .collect::<Result<Vec<_>, String>>()?
                .join(", ")
        )),
        Value::Null => Err("null cannot be written to TOML".into()),
    }
}
fn toml_has_server(text: &str, id: &str) -> Result<bool, String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("mcp_servers") && line.contains('=') {
            return Err("inline/dotted MCP tables need manual migration before applying a native TOML table".into());
        }
        if !line.starts_with('[') {
            continue;
        }
        let Some(close) = line.find(']') else {
            return Err("unterminated TOML table header".into());
        };
        let raw = line[1..close].trim();
        let mut keys = Vec::new();
        let mut start = 0;
        let mut quoted = None;
        let mut escaped = false;
        for (i, c) in raw.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if quoted == Some('"') && c == '\\' {
                escaped = true;
                continue;
            }
            if let Some(q) = quoted {
                if c == q {
                    quoted = None;
                }
            } else if matches!(c, '"' | '\'') {
                quoted = Some(c);
            } else if c == '.' {
                keys.push(raw[start..i].trim());
                start = i + 1;
            }
        }
        keys.push(raw[start..].trim());
        let decoded = keys
            .into_iter()
            .map(|v| {
                if v.starts_with('"') {
                    json::parse(v.as_bytes())
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                } else {
                    v.trim_matches('\'').to_owned()
                }
            })
            .collect::<Vec<_>>();
        if decoded.first().map(String::as_str) == Some("mcp_servers")
            && decoded.get(1).map(String::as_str) == Some(id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn apply_toml(
    plan: &mut Plan,
    path: &Path,
    previous: &Value,
    desired: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let previous = records(previous)?;
    let mut text = plan.text_at(path)?;
    let mut output = BTreeMap::new();
    for id in previous
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        let begin = format!("# zcalld:mcp:{id}:begin");
        let end = format!("# zcalld:mcp:{id}:end");
        let old = previous.get(&id);
        if let Some(old) = old {
            check_record_path(old, path)?;
        }
        if old.is_none() && toml_has_server(&text, &id)? {
            return Err(format!(
                "TOML MCP server {id} already belongs to another writer; no files were changed"
            ));
        }
        let mut next = String::new();
        if let Some(value) = desired.get(&id) {
            next = format!(
                "{begin}\n[mcp_servers.{}]\n",
                Value::from(id.as_str()).stringify()
            );
            for (key, value) in value.as_object().ok_or("invalid MCP object")? {
                next.push_str(&format!(
                    "{} = {}\n",
                    Value::from(key.as_str()).stringify(),
                    toml_value(value)?
                ));
            }
            next.push_str(&end);
        }
        text = replace_block(
            &text,
            &begin,
            &end,
            old.and_then(|v| v.get("written").as_str()),
            &next,
        )?;
        if !next.is_empty() {
            output.insert(
                id,
                object([
                    ("path", path.to_string_lossy().into_owned().into()),
                    ("written", next.into()),
                ]),
            );
        }
    }
    plan.text(path.to_path_buf(), &text)?;
    Ok(Value::Object(output))
}

fn apply_skills(
    plan: &mut Plan,
    dest: &Path,
    previous: &Value,
    desired: &BTreeMap<String, BTreeMap<PathBuf, FileData>>,
) -> Result<Value, String> {
    let previous = records(previous)?;
    let mut output = BTreeMap::new();
    for id in previous
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        name(&id)?;
        let folder = dest.join(&id);
        reject_links(&folder)?;
        let current = if folder.exists() {
            read_tree(&folder, false)?
        } else {
            BTreeMap::new()
        };
        if let Some(record) = previous.get(&id) {
            check_record_path(record, &folder)?;
            if tree_record(&current) != *record.get("files") {
                return Err(format!(
                    "skill {id} changed in the agent directory; no files were changed"
                ));
            }
        } else if !current.is_empty() {
            return Err(format!(
                "skill directory {id} belongs to another writer; no files were changed"
            ));
        }
        if let Some(tree) = desired.get(&id) {
            for path in current.keys() {
                if !tree.contains_key(path) {
                    plan.set(folder.join(path), None)?;
                }
            }
            for (path, file) in tree {
                plan.set(folder.join(path), Some(file.clone()))?;
            }
            output.insert(
                id,
                object([
                    ("path", folder.to_string_lossy().into_owned().into()),
                    ("files", tree_record(tree)),
                ]),
            );
        } else {
            for path in current.keys() {
                plan.set(folder.join(path), None)?;
            }
        }
    }
    Ok(Value::Object(output))
}
fn tree_record(tree: &BTreeMap<PathBuf, FileData>) -> Value {
    Value::Object(
        tree.iter()
            .map(|(path, file)| {
                (
                    path.to_string_lossy().into_owned(),
                    object([
                        ("sha256", sha256(&file.bytes).into()),
                        ("executable", file.executable.into()),
                    ]),
                )
            })
            .collect(),
    )
}

// SHA-256 for local change detection; no process or crypto runtime is kept alive.
fn sha256(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut input = bytes.to_vec();
    input.push(0x80);
    while input.len() % 64 != 56 {
        input.push(0);
    }
    input.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in input.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
    state.iter().map(|v| format!("{v:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_resource_names_use_portable_separators() {
        let path = Path::new("references").join("nested").join("说明.md");
        let resource = resource_path(&path);
        assert_eq!(resource, "references/nested/说明.md");
        assert_eq!(relative(&resource).unwrap(), path);
        // On Unix a literal backslash is a filename character, not a separator.
        #[cfg(unix)]
        assert_eq!(
            resource_path(Path::new(r"literal\name.md")),
            r"literal\name.md"
        );
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn jsonc_removal_preserves_other_members_and_comments() {
        for key in ["first", "middle", "last"] {
            let original = "{\n // outer\n \"mcp\": {\n // keep first\n \"first\": {\"x\": \"}\"}, /* between */\n \"middle\": [1,{\"y\":\"\\\"\"}],\n // keep last\n \"last\": true,\n },\n \"large\":9007199254740993\n}";
            let result = remove_json_member(original, &["mcp".into(), key.into()]).unwrap();
            let parsed = json::parse_jsonc(&result).unwrap();
            assert_eq!(parsed.get("mcp").get(key), &Value::Null);
            assert_eq!(parsed.get("mcp").as_object().unwrap().len(), 2);
            for expected in [
                "// outer",
                "// keep first",
                "/* between */",
                "// keep last",
                "9007199254740993",
            ] {
                assert!(result.contains(expected));
            }
        }
        assert_eq!(
            json::parse_jsonc(&remove_json_member("{\"a\":1}", &["a".into()]).unwrap()).unwrap(),
            object([])
        );
    }

    #[test]
    fn instructions_preserve_surrounding_text_and_reject_edits() {
        let block = format!("{BEGIN}\nours\n{END}");
        let original = format!("manual before\n{block}\nmanual after\n");
        let next = format!("{BEGIN}\nupdated\n{END}");
        let output = replace_block(&original, BEGIN, END, Some(&block), &next).unwrap();
        assert_eq!(output, format!("manual before\n{next}\nmanual after\n"));
        assert!(
            replace_block(
                &original.replace("ours", "user edit"),
                BEGIN,
                END,
                Some(&block),
                ""
            )
            .is_err()
        );
        assert!(replace_block(&format!("{BEGIN}\n{BEGIN}\n{END}"), BEGIN, END, None, "").is_err());
        assert_eq!(
            replace_block(&original, BEGIN, END, Some(&block), "").unwrap(),
            "manual before\n\nmanual after\n"
        );
    }

    #[test]
    fn codex_maps_environment_references_without_resolving_secrets() {
        let remote = object([
            ("name", "remote".into()),
            ("transport", "http".into()),
            ("url", "https://example.invalid/mcp".into()),
            (
                "headers",
                object([
                    ("Authorization", "Bearer ${TOKEN}".into()),
                    ("X-Header", "${OTHER}".into()),
                    ("Literal", "text".into()),
                ]),
            ),
        ]);
        let out = encode_mcp(&remote, "codex").unwrap();
        assert_eq!(out.get("bearer_token_env_var").as_str(), Some("TOKEN"));
        assert_eq!(
            out.get("env_http_headers").get("X-Header").as_str(),
            Some("OTHER")
        );
        assert_eq!(
            out.get("http_headers").get("Literal").as_str(),
            Some("text")
        );
        assert!(encode_mcp(&remote, "zed").is_err());
        let local = object([
            ("name", "local".into()),
            ("command", "tool".into()),
            ("env", object([("TOKEN", "${TOKEN}".into())])),
        ]);
        assert_eq!(
            encode_mcp(&local, "codex").unwrap().get("env_vars"),
            &Value::Array(vec!["TOKEN".into()])
        );
    }

    #[test]
    fn formats_choose_real_transport_keys_and_overlay_accounts() {
        let server = object([
            ("name", "x".into()),
            ("transport", "http".into()),
            ("url", "https://example.invalid/mcp".into()),
            ("headers", object([("authorization", "old".into())])),
            ("account", "selected".into()),
            (
                "accounts",
                Value::Array(vec![object([
                    ("id", "selected".into()),
                    ("headers", object([("Authorization", "new".into())])),
                ])]),
            ),
        ]);
        let gemini = encode_mcp(&server, "gemini").unwrap();
        assert_eq!(gemini.get("httpUrl"), server.get("url"));
        assert_eq!(gemini.get("headers").as_object().unwrap().len(), 1);
        assert_eq!(
            gemini.get("headers").get("Authorization").as_str(),
            Some("new")
        );
        assert_eq!(
            encode_mcp(&server, "antigravity").unwrap().get("serverUrl"),
            server.get("url")
        );
        assert!(encode_mcp(&server, "desktop").is_err());
        let mut sse = server.clone();
        sse.set("transport", "sse".into());
        assert!(encode_mcp(&sse, "codex").is_err());
        assert_eq!(
            encode_mcp(&sse, "gemini").unwrap().get("url"),
            server.get("url")
        );
    }

    #[test]
    fn assignment_distinguishes_legacy_missing_and_explicit_empty() {
        assert!(assigned(&object([]), "claude", true));
        assert!(!assigned(
            &object([("agents", Value::Array(vec![]))]),
            "claude",
            true
        ));
        assert!(assigned(
            &object([("agents", Value::Array(vec!["agy".into()]))]),
            "antigravity",
            false
        ));
        assert!(!assigned(
            &object([("hidden", true.into())]),
            "claude",
            true
        ));
    }

    #[test]
    fn toml_collision_recognizes_quoted_and_dotted_headers() {
        for input in [
            "[mcp_servers.foo]\ncommand='x'",
            "[\"mcp_servers\" . 'foo' . env]\nx='y'",
            "[mcp_servers.\"foo\"] # existing",
        ] {
            assert!(toml_has_server(input, "foo").unwrap());
        }
        assert!(!toml_has_server("[projects.\"/tmp/foo\"]\ntrust_level='trusted'", "foo").unwrap());
        assert!(toml_has_server("mcp_servers = { foo = {} }", "foo").is_err());
        assert_eq!(
            toml_value(&object([("header", "line\nquote\"".into())])).unwrap(),
            "{ \"header\" = \"line\\nquote\\\"\" }"
        );
    }

    #[test]
    fn plan_rejects_change_after_preview() {
        // macOS exposes its temporary directory through /var -> /private/var.
        // Resolve the fixture root without weakening managed-path validation.
        let root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = root.join(format!(
            "zcalld-library-plan-{}-{}",
            std::process::id(),
            control::now()
        ));
        fs::write(&path, b"before").unwrap();
        let mut plan = Plan::default();
        plan.text(path.clone(), "after").unwrap();
        fs::write(&path, b"external writer").unwrap();
        assert!(plan.changes().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"external writer");
        fs::remove_file(path).unwrap();
    }
}
