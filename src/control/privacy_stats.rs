//! Numerical summaries of existing persisted privacy counters only.
use super::{Context, emit, json, object};
use crate::state_json::Value;
use std::{fs, io::Read, path::PathBuf};

pub(super) fn run(context: &Context) -> Result<(), String> {
    let settings = crate::extensions::privacy_settings_path(&context.path)?;
    let binding = context.directory.join("privacy/plugin.json");
    let directory = if binding.exists() {
        json::read(&binding)?
            .get("history_dir")
            .as_str()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    } else {
        None
    }
    .unwrap_or_else(|| settings.with_file_name("history"));
    if !directory.is_absolute() {
        return Err("privacy history directory must be absolute".into());
    }
    let mut files = Vec::new();
    match fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| error.to_string())?;
                if let Some(day) = date(&entry.file_name().to_string_lossy()) {
                    files.push((day.to_owned(), entry.path()));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("privacy counters are unavailable".into()),
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for (day, path) in files.into_iter().rev().take(7) {
        let read = || -> Option<(u64, u64)> {
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.is_file() || metadata.len() > 1024 * 1024 {
                return None;
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .ok()?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            let value = json::parse_with_limit(&bytes, 1024 * 1024).ok()?;
            let mut looked = 0u64;
            let mut masked = 0u64;
            for row in value.as_object()?.values() {
                let fields = row.as_object()?;
                let count = |name| fields.get(name).map_or(Some(0), Value::as_u64);
                looked = looked.saturating_add(count("looked")?);
                masked = masked.saturating_add(count("masked")?);
            }
            (masked <= looked).then_some((looked, masked))
        };
        let Some((looked, masked)) = read() else {
            skipped += 1;
            continue;
        };
        rows.push(object([
            ("date", day.into()),
            ("checked", looked.into()),
            ("masked", masked.into()),
        ]));
    }
    rows.reverse();
    emit(object([
        ("days", Value::Array(rows)),
        ("skipped_days", skipped.into()),
    ]))
}

fn date(file: &str) -> Option<&str> {
    let date = file.strip_prefix("checked-")?.strip_suffix(".json")?;
    (date.len() == 10
        && date.bytes().enumerate().all(|(index, ch)| {
            if matches!(index, 4 | 7) {
                ch == b'-'
            } else {
                ch.is_ascii_digit()
            }
        }))
    .then_some(date)
}
