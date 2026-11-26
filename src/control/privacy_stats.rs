//! Aggregate counters and a metadata-only projection of recent audit events.
use super::{Context, emit, json, object};
use crate::state_json::Value;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

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
    let mut audit_files = Vec::new();
    match fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| error.to_string())?;
                if let Some(day) = date(&entry.file_name().to_string_lossy()) {
                    files.push((day.to_owned(), entry.path()));
                } else if let Some(day) = entry
                    .file_name()
                    .to_str()
                    .and_then(|file| file.strip_suffix(".jsonl"))
                    .filter(|day| valid_day(day))
                {
                    audit_files.push((day.to_owned(), entry.path()));
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
    audit_files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut events = Vec::new();
    let mut events_partial = false;
    for (_, path) in audit_files.into_iter().rev().take(7) {
        match recent_events(&path, &mut events) {
            Ok(()) => {}
            Err(_) => events_partial = true,
        }
        if events.len() == EVENT_LIMIT {
            break;
        }
    }
    emit(object([
        ("days", Value::Array(rows)),
        ("skipped_days", skipped.into()),
        ("events", Value::Array(events)),
        ("events_partial", events_partial.into()),
    ]))
}

const EVENT_LIMIT: usize = 20;
const AUDIT_TAIL: u64 = 2 * 1024 * 1024;

fn recent_events(path: &std::path::Path, events: &mut Vec<Value>) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut file = fs::File::open(path)?;
    let start = metadata.len().saturating_sub(AUDIT_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(metadata.len() - start).read_to_end(&mut bytes)?;
    // An append can still be in progress. Only accept complete JSONL records.
    let mut lines = bytes.split(|byte| *byte == b'\n');
    if start > 0 {
        lines.next();
    }
    lines.next_back();
    for line in lines.rev() {
        if events.len() == EVENT_LIMIT {
            break;
        }
        if let Some(event) = event_summary(line) {
            events.push(event);
        }
    }
    Ok(())
}

fn event_summary(line: &[u8]) -> Option<Value> {
    let row = json::parse_with_limit(line, 1024 * 1024).ok()?;
    let timestamp = row.get("t").as_str()?;
    let day = timestamp.get(..10)?;
    let clock = timestamp.get(11..19)?;
    if !valid_day(day)
        || timestamp.as_bytes().get(10) != Some(&b'T')
        || !clock.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 2 | 5) {
                byte == b':'
            } else {
                byte.is_ascii_digit()
            }
        })
    {
        return None;
    }
    let count = row.get("n").as_u64()?;
    let blocked = row.get("blocked").as_bool().unwrap_or(false);
    Some(object([
        ("time", format!("{day} {clock}").into()),
        ("replacements", count.into()),
        ("blocked", blocked.into()),
    ]))
}

fn date(file: &str) -> Option<&str> {
    let date = file.strip_prefix("checked-")?.strip_suffix(".json")?;
    valid_day(date).then_some(date)
}

fn valid_day(date: &str) -> bool {
    date.len() == 10
        && date.bytes().enumerate().all(|(index, ch)| {
            if matches!(index, 4 | 7) {
                ch == b'-'
            } else {
                ch.is_ascii_digit()
            }
        })
}
