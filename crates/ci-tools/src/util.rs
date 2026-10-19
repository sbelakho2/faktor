use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

pub type Result<T> = std::result::Result<T, String>;

pub fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", display(path)))
}

pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", display(path)))
}

pub fn read_stdin() -> Result<String> {
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("stdin: {error}"))?;
    Ok(text)
}

pub fn parse_json(text: &str, label: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| format!("{label}: invalid JSON: {error}"))
}

pub fn read_json(path: &Path) -> Result<Value> {
    parse_json(&read_text(path)?, &display(path))
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", display(parent)))?;
        }
    }
    let mut text =
        serde_json::to_string_pretty(value).map_err(|error| format!("serialize: {error}"))?;
    text.push('\n');
    fs::write(path, text).map_err(|error| format!("{}: {error}", display(path)))
}

pub fn write_json_compact(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", display(parent)))?;
        }
    }
    let mut text = serde_json::to_string(value).map_err(|error| format!("serialize: {error}"))?;
    text.push('\n');
    fs::write(path, text).map_err(|error| format!("{}: {error}", display(path)))
}

pub fn display(path: &Path) -> String {
    path.display().to_string()
}

pub fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = read_bytes(path)?;
    Ok(format!("sha256:{}", sha256_hex(&bytes)))
}

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn iso_from_epoch(raw: &str) -> Option<String> {
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let seconds: i64 = raw.parse().ok()?;
    let time = chrono::DateTime::from_timestamp(seconds, 0)?;
    Some(time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

pub fn normalize_lane(lane: &str) -> String {
    let mut output = String::new();
    let mut last_underscore = false;
    for ch in lane.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            output.push(ch);
            last_underscore = false;
        } else if !last_underscore {
            output.push('_');
            last_underscore = true;
        }
    }
    output.trim_matches('_').to_string()
}

pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
        Value::Array(items) => {
            let mut output = String::from("[");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&canonical_json(item));
            }
            output.push(']');
            output
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut output = String::from("{");
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()));
                output.push(':');
                output.push_str(&canonical_json(&map[*key]));
            }
            output.push('}');
            output
        }
    }
}

pub fn as_str(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Bool(value)) => value.to_string(),
        Some(Value::Number(value)) => value.to_string(),
        Some(other) => other.to_string(),
    }
}

pub fn field<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.').filter(|part| !part.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

pub fn fsync_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", display(parent)))?;
        }
    }
    let mut file = fs::File::create(path).map_err(|error| format!("{}: {error}", display(path)))?;
    file.write_all(bytes)
        .map_err(|error| format!("{}: {error}", display(path)))?;
    file.flush()
        .map_err(|error| format!("{}: {error}", display(path)))
}
