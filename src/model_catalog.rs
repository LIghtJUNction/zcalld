//! Public models.dev metadata. Fetching is an explicit management operation.
use crate::{
    control::{self, Context},
    state_json::{self as json, Value, object},
};
use std::{io::ErrorKind, path::Path, time::Duration};

const URL: &str = "https://models.dev/models.json?type=all";
const LIMIT: usize = 2 * 1024 * 1024;
const CACHE: &str = "models-dev.json";

fn snapshot(directory: &Path, refresh: bool) -> Result<Value, String> {
    let path = directory.join(CACHE);
    if !refresh {
        match path.try_exists() {
            Ok(true) => return json::read(&path),
            Ok(false) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let (status, body, _) = control::command(
        "curl",
        &[
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--max-time",
            "15",
            "--max-filesize",
            "2097151",
            URL,
        ]
        .map(str::to_owned),
        None,
        Duration::from_secs(20),
    )?;
    if status != 0 || body.len() >= LIMIT {
        return Err("could not download models.dev metadata".into());
    }
    let data = json::parse_with_limit(body.as_bytes(), LIMIT)?;
    if !data.as_object().is_some_and(|models| {
        !models.is_empty()
            && models
                .iter()
                .all(|(key, model)| model.get("id").as_str() == Some(key))
    }) {
        return Err("unexpected models.dev model catalog".into());
    }
    json::atomic_write(&path, body.as_bytes())?;
    Ok(data)
}

pub fn lookup(directory: &Path, id: &str) -> Result<Value, String> {
    let data = snapshot(directory, false)?;
    let model = data.get(id);
    if model.as_object().is_none() {
        return Err("unknown models.dev model ID".into());
    }
    Ok(model.clone())
}

pub fn run(context: &Context, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    if !matches!(action, "list" | "refresh") {
        return Err("model-catalog expects list or refresh".into());
    }
    let query = control::option(args, "--query")?
        .unwrap_or("")
        .to_lowercase();
    let kind = control::option(args, "--type")?;
    let data = snapshot(&context.directory, action == "refresh")?;
    let models = data.as_object().ok_or("invalid models.dev cache")?;
    let mut results = Vec::new();
    for (id, model) in models {
        if kind.is_some_and(|kind| model.get("type").as_str() != Some(kind)) {
            continue;
        }
        let name = model.get("name").as_str().unwrap_or(id);
        if !id.to_lowercase().contains(&query) && !name.to_lowercase().contains(&query) {
            continue;
        }
        let mut entry = object([("id", id.clone().into()), ("name", name.into())]);
        for key in [
            "type",
            "description",
            "family",
            "modalities",
            "limit",
            "cost",
            "release_date",
        ] {
            if !matches!(model.get(key), Value::Null) {
                entry.set(key, model.get(key).clone());
            }
        }
        results.push(entry);
    }
    control::emit(object([
        ("source", "models.dev".into()),
        ("models", Value::Array(results)),
    ]))
}
