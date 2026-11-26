//! Read and edit the exact files identified by built-in and registered adapters.
use super::{Context, agents, control, custom_definitions, fs, json, object, under_home};
use crate::digest;
use std::{io::Read, path::Path};

const LIMIT: usize = 256 * 1024;

fn syntax(path: &Path, agent: &str) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("env") => "env",
        Some("jsonc") => "jsonc",
        Some("json") if matches!(agent, "zed" | "cursor" | "vscode" | "opencode") => "jsonc",
        _ => "json",
    }
}

fn starter(format: &str, path: &Path) -> &'static str {
    if matches!(format, "json" | "jsonc") {
        if path
            .file_name()
            .is_some_and(|name| name == "chatLanguageModels.json")
        {
            "[]\n"
        } else {
            "{}\n"
        }
    } else {
        ""
    }
}

fn snapshot(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => Err("configuration must be a regular file".into()),
        Ok(_) => {
            let mut content = String::new();
            fs::File::open(path)
                .and_then(|file| file.take((LIMIT + 1) as u64).read_to_string(&mut content))
                .map_err(|error| error.to_string())?;
            if content.len() > LIMIT {
                return Err("configuration exceeds the 256 KiB editor limit".into());
            }
            Ok(Some(content))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn validate(format: &str, content: &str) -> Result<(), String> {
    match format {
        "toml" => content.parse::<toml::Table>().map(|_| ()).map_err(|error| {
            let line = error.span().map_or(1, |span| {
                content
                    .as_bytes()
                    .get(..span.start)
                    .unwrap_or_default()
                    .iter()
                    .filter(|&&byte| byte == b'\n')
                    .count()
                    + 1
            });
            format!("TOML line {line}: {}", error.message())
        }),
        "yaml" => serde_yaml::from_str::<serde_yaml::Value>(content)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        // dotenv implementations differ on multiline strings and interpolation.
        // Only check the portable requirement for the assignment's variable name.
        "env" => {
            let mut quote = None;
            for (line, text) in content.lines().enumerate() {
                let text = text.trim();
                let value = if quote.is_some() {
                    text
                } else {
                    if text.is_empty() || text.starts_with('#') {
                        continue;
                    }
                    let text = text.strip_prefix("export ").unwrap_or(text);
                    let (key, value) = text
                        .split_once('=')
                        .ok_or_else(|| format!("dotenv line {}: expected KEY=value", line + 1))?;
                    let key = key.trim();
                    if !key
                        .as_bytes()
                        .first()
                        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                        || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    {
                        return Err(format!("dotenv line {}: invalid variable name", line + 1));
                    }
                    let value = value.trim_start();
                    quote = value.chars().next().filter(|c| matches!(c, '\'' | '"'));
                    if quote.is_some() { &value[1..] } else { "" }
                };
                if let Some(delimiter) = quote {
                    let mut escaped = false;
                    for character in value.chars() {
                        if !escaped && character == delimiter {
                            quote = None;
                            break;
                        }
                        escaped = !escaped && character == '\\';
                    }
                }
            }
            if quote.is_some() {
                Err("dotenv: unclosed quoted value".into())
            } else {
                Ok(())
            }
        }
        "jsonc" => json::parse_jsonc(content).map(|_| ()),
        _ => json::parse(content.as_bytes()).map(|_| ()),
    }
}

pub(super) fn run(context: &Context, args: &[String]) -> Result<(), String> {
    let id = args.get(1).ok_or("agent name required")?;
    let files = if let Some(agent) = agents(context).into_iter().find(|agent| agent.id == id) {
        agent.files
    } else {
        let definitions = custom_definitions(context)?;
        definitions
            .get(id)
            .ok_or("unknown agent")?
            .get("files")
            .as_array()
            .iter()
            .map(|file| {
                under_home(
                    context,
                    file.get("path")
                        .as_str()
                        .ok_or("configuration path missing")?,
                )
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    let index = control::option(args, "--index")?
        .unwrap_or("0")
        .parse::<usize>()
        .map_err(|_| "invalid configuration index")?;
    let path = files.get(index).ok_or("configuration index out of range")?;
    let format = syntax(path, id);
    let write = control::flag(args, "--write");
    let check = control::flag(args, "--check");
    if write || check {
        let mut content = String::new();
        std::io::stdin()
            .take((LIMIT + 1) as u64)
            .read_to_string(&mut content)
            .map_err(|error| error.to_string())?;
        if content.len() > LIMIT {
            return Err("configuration exceeds the 256 KiB editor limit".into());
        }
        validate(format, &content)?;
        if write {
            let expected =
                control::option(args, "--revision")?.ok_or("configuration revision required")?;
            if revision(snapshot(path)?.as_deref()) != expected {
                return Err(
                    "configuration changed on disk; reopen the editor before saving".into(),
                );
            }
            json::atomic_write(path, content.as_bytes())?;
        }
        return control::emit(object([("revision", revision(Some(&content)).into())]));
    }
    let content = snapshot(path)?;
    control::emit(object([
        (
            "files",
            json::Value::Array(
                files
                    .iter()
                    .map(|file| file.to_string_lossy().into_owned().into())
                    .collect(),
            ),
        ),
        ("format", format.into()),
        ("revision", revision(content.as_deref()).into()),
        (
            "content",
            content
                .unwrap_or_else(|| starter(format, path).into())
                .into(),
        ),
    ]))
}

fn revision(content: Option<&str>) -> String {
    content.map_or_else(|| "missing".into(), |text| digest::sha256(text.as_bytes()))
}
