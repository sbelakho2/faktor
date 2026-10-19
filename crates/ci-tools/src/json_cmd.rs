use crate::util::{self, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::Path;

fn load(path: &str) -> Option<Value> {
    util::read_json(Path::new(path)).ok()
}

fn print_scalar(value: Option<&Value>) {
    match value {
        None | Some(Value::Null) => println!(),
        Some(Value::Bool(value)) => println!("{}", if *value { "true" } else { "false" }),
        Some(Value::String(text)) => println!("{text}"),
        Some(Value::Number(number)) => println!("{number}"),
        Some(other) => println!(
            "{}",
            serde_json::to_string(other).unwrap_or_else(|_| "null".into())
        ),
    }
}

fn number(value: Option<&Value>) -> i64 {
    value
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn repo_id(data: &Value) {
    print_scalar(data.get("id"));
}

fn field(data: &Value, path: &str) {
    print_scalar(util::field(data, path));
}

fn candidates(data: &Value) {
    let mut rows: Vec<&Value> = data
        .as_array()
        .map(|rows| rows.iter().collect())
        .unwrap_or_default();
    rows.sort_by_key(|row| std::cmp::Reverse(number(row.get("number"))));
    for row in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            util::as_str(row.get("number")),
            util::as_str(row.get("id")),
            util::as_str(row.get("status")),
            util::as_str(row.get("commit")),
            util::as_str(row.get("event")),
        );
    }
}

fn workflow_state(data: &Value, name: &str) {
    let Some(workflows) = data.get("workflows").and_then(Value::as_array) else {
        return;
    };
    for workflow in workflows {
        if workflow.get("name").and_then(Value::as_str) == Some(name) {
            print_scalar(workflow.get("state"));
            return;
        }
    }
}

fn attestation_step(data: &Value, name: &str) {
    let mut steps: Vec<&Value> = Vec::new();
    if let Some(workflows) = data.get("workflows").and_then(Value::as_array) {
        for workflow in workflows {
            for key in ["children", "steps", "tasks"] {
                if let Some(items) = workflow.get(key).and_then(Value::as_array) {
                    steps.extend(items.iter());
                }
            }
        }
    }
    if let Some(items) = data.get("steps").and_then(Value::as_array) {
        steps.extend(items.iter());
    }
    for step in steps {
        if step.get("name").and_then(Value::as_str) == Some(name) {
            if step.get("id").is_some() {
                print_scalar(step.get("id"));
            } else {
                print_scalar(step.get("pid"));
            }
            return;
        }
    }
}

fn context_boundaries(data: &Value, agents: Option<Value>) {
    let mut platforms = std::collections::HashSet::new();
    if let Some(Value::Array(agents)) = agents {
        for agent in agents {
            if agent.get("last_contact").is_none_or(Value::is_null) {
                continue;
            }
            if let Some(platform) = agent
                .get("custom_labels")
                .and_then(|labels| labels.get("platform"))
                .and_then(Value::as_str)
            {
                platforms.insert(platform.to_string());
            } else if let Some(platform) = agent.get("platform").and_then(Value::as_str) {
                platforms.insert(platform.to_string());
            }
        }
    }
    let Some(workflows) = data.get("workflows").and_then(Value::as_array) else {
        return;
    };
    for workflow in workflows {
        let name = workflow.get("name").and_then(Value::as_str).unwrap_or("");
        let state = workflow.get("state").and_then(Value::as_str).unwrap_or("");
        if state == "success" {
            continue;
        }
        let platform = workflow
            .get("environ")
            .and_then(|environ| environ.get("platform"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(
            state,
            "pending" | "running" | "blocked" | "created" | "started"
        ) && !platform.is_empty()
            && !platforms.contains(platform)
        {
            println!(
                "BOUNDARY\tworkflow {name} is {state} and no connected agent advertises platform={platform} (self-hosted lane absent; typed, visible, never green)"
            );
        } else {
            println!(
                "PROBLEM\tworkflow {name} is {state} (platform={})",
                if platform.is_empty() {
                    "unknown"
                } else {
                    platform
                }
            );
        }
    }
}

fn extract(data: &Value, marker: &str, prefix: &str) -> Result<i32> {
    let mut raw_parts = Vec::new();
    let mut decoded_parts = Vec::new();
    let mut push = |text: &str| {
        raw_parts.push(text.to_string());
        decoded_parts.push(
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_else(|| text.to_string()),
        );
    };
    if let Some(items) = data.as_array() {
        for item in items {
            if let Some(text) = item.get("data").and_then(Value::as_str) {
                push(text);
            }
        }
    } else if let Some(object) = data.as_object() {
        if let Some(text) = object.get("data").and_then(Value::as_str) {
            push(text);
        } else if let Some(lines) = object.get("lines").and_then(Value::as_array) {
            for line in lines.iter().filter_map(Value::as_str) {
                decoded_parts.push(line.to_string());
            }
        }
    }
    let text = format!("{}\n{}", raw_parts.join("\n"), decoded_parts.join("\n"));
    let begin = format!("-----BEGIN {marker}-----");
    let end = format!("-----END {marker}-----");
    let Some(start) = text.find(&begin) else {
        return Ok(3);
    };
    let Some(end_index) = text[start + begin.len()..].find(&end) else {
        return Ok(3);
    };
    let blob: String = text[start + begin.len()..start + begin.len() + end_index]
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(blob.as_bytes())
        .map_err(|error| format!("{prefix}: {error}"))?;
    let value: Value =
        serde_json::from_slice(&decoded).map_err(|error| format!("{prefix}: {error}"))?;
    if !value.is_object() {
        return Ok(4);
    }
    println!(
        "{}",
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
    );
    Ok(0)
}

pub fn json(args: &[String]) -> i32 {
    let Some(mode) = args.first().map(String::as_str) else {
        eprintln!("usage: faktor-ci-tools json MODE FILE [ARG...]");
        return 2;
    };
    let Some(path) = args.get(1) else {
        eprintln!("json: missing file");
        return 2;
    };
    if mode == "validate" {
        return if load(path).is_some() { 0 } else { 1 };
    }
    let Some(data) = load(path) else {
        match mode {
            "field" | "repo_id" => {
                println!();
                return 0;
            }
            "attestation_extract" | "release_signature_extract" => return 3,
            _ => return 0,
        }
    };
    let arg = args.get(2).map(String::as_str).unwrap_or("");
    let arg2 = args.get(3).map(String::as_str).unwrap_or("");
    match mode {
        "repo_id" => repo_id(&data),
        "field" => field(&data, arg),
        "candidates" => candidates(&data),
        "workflow_state" => workflow_state(&data, arg),
        "attestation_step" => attestation_step(&data, arg),
        "context_boundaries" => {
            let agents = if arg2.is_empty() { None } else { load(arg2) };
            context_boundaries(&data, agents);
        }
        "attestation_extract" => {
            return match extract(&data, "FAKTOR ATTESTATION", "attestation-extract") {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("{error}");
                    4
                }
            };
        }
        "release_signature_extract" => {
            return match extract(
                &data,
                "FAKTOR RELEASE SIGNATURE",
                "release-signature-extract",
            ) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("{error}");
                    4
                }
            };
        }
        "cron_id" => {
            let Some(jobs) = data.as_array() else {
                return 0;
            };
            for job in jobs {
                if job.get("name").and_then(Value::as_str) == Some(arg) {
                    print_scalar(job.get("id"));
                    break;
                }
            }
        }
        "len" => {
            let length = util::field(&data, arg)
                .map(|value| match value {
                    Value::Array(items) => items.len(),
                    Value::Object(map) => map.len(),
                    _ => 0,
                })
                .unwrap_or(0);
            println!("{length}");
        }
        other => {
            eprintln!("json: unknown mode '{other}'");
            return 2;
        }
    }
    0
}

pub fn json_get(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: faktor-ci-tools json-get FILE PATH [DEFAULT]");
        return 2;
    };
    let Some(field_path) = args.get(1) else {
        eprintln!("json-get: missing path");
        return 2;
    };
    let default = args.get(2).cloned().unwrap_or_default();
    match load(path) {
        Some(data) => match util::field(&data, field_path) {
            Some(Value::String(text)) => println!("{text}"),
            Some(Value::Bool(value)) => println!("{value}"),
            Some(Value::Number(value)) => println!("{value}"),
            Some(Value::Null) | None => println!("{default}"),
            Some(other) => println!(
                "{}",
                serde_json::to_string(other).unwrap_or_else(|_| default.clone())
            ),
        },
        None => println!("{default}"),
    }
    0
}

fn json_compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
}

pub fn wo_body(args: &[String]) -> i32 {
    let Some(mode) = args.first().map(String::as_str) else {
        eprintln!("usage: faktor-ci-tools wo-body secret|secret-value|settings|cron ...");
        return 2;
    };
    match mode {
        "secret-value" => {
            let value = args.get(1).cloned().unwrap_or_default();
            println!("{}", json_compact(&json!({"value": value})));
        }
        "secret" => {
            let name = args.get(1).cloned().unwrap_or_default();
            let value = args.get(2).cloned().unwrap_or_default();
            println!("{}", json_compact(&json!({"name": name, "value": value})));
        }
        "settings" => {
            let config_file = args.get(1).cloned().unwrap_or_default();
            let allow_pr = args.get(2).map(String::as_str) == Some("true");
            let volumes = args.get(3).cloned().unwrap_or_else(|| "skip".into());
            let mut body = serde_json::Map::new();
            body.insert("config_file".into(), json!(config_file));
            body.insert("allow_pr".into(), json!(allow_pr));
            if volumes != "skip" {
                body.insert("trusted".into(), json!({"volumes": volumes == "true"}));
            }
            println!("{}", json_compact(&Value::Object(body)));
        }
        "cron" => {
            let name = args.get(1).cloned().unwrap_or_default();
            let schedule = args.get(2).cloned().unwrap_or_default();
            let branch = args.get(3).cloned().unwrap_or_default();
            let timezone = args.get(4).cloned().unwrap_or_default();
            println!(
                "{}",
                json_compact(&json!({
                    "name": name,
                    "schedule": schedule,
                    "branch": branch,
                    "timezone": timezone,
                    "enabled": true,
                }))
            );
        }
        other => {
            eprintln!("wo-body: unknown mode '{other}'");
            return 2;
        }
    }
    0
}
