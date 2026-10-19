use crate::util;
use serde_json::{json, Map, Value};
use std::path::Path;

fn env_json(name: &str, fallback: Value) -> Value {
    std::env::var(name)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(fallback)
}

fn print_json(value: &Value) -> i32 {
    println!(
        "{}",
        serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
    );
    0
}

fn pipeline(args: &[String]) -> i32 {
    if args.len() < 5 {
        eprintln!("usage: fixture pipeline NUMBER ID STATUS COMMIT EVENT");
        return 2;
    }
    print_json(&json!({
        "number": args[0].parse::<i64>().unwrap_or(0),
        "id": args[1].parse::<i64>().unwrap_or(0),
        "status": args[2],
        "commit": args[3],
        "event": args[4],
    }))
}

fn detail(args: &[String]) -> i32 {
    if args.len() < 6 {
        eprintln!("usage: fixture detail NUMBER ID STATUS COMMIT EVENT WORKFLOWS [ATT] [REL]");
        return 2;
    }
    let mut children = Vec::new();
    if let Some(pid) = args.get(6).filter(|value| !value.is_empty()) {
        children.push(json!({"name": "attestation", "pid": pid.parse::<i64>().unwrap_or(0), "state": "success"}));
    }
    if let Some(pid) = args.get(7).filter(|value| !value.is_empty()) {
        children.push(json!({"name": "release-signature", "pid": pid.parse::<i64>().unwrap_or(0), "state": "success"}));
    }
    let mut workflows = Vec::new();
    for pair in args[5].split(',') {
        if pair.is_empty() {
            continue;
        }
        let (name, state) = pair.split_once('=').unwrap_or((pair, ""));
        workflows.push(json!({
            "name": name,
            "state": state,
            "children": if name == "trusted" { Value::Array(children.clone()) } else { Value::Array(Vec::new()) },
        }));
    }
    print_json(&json!({
        "number": args[0].parse::<i64>().unwrap_or(0),
        "id": args[1].parse::<i64>().unwrap_or(0),
        "status": args[2],
        "commit": args[3],
        "event": args[4],
        "workflows": workflows,
    }))
}

fn details(args: &[String]) -> i32 {
    let mut output = Map::new();
    let mut index = 0;
    while index + 1 < args.len() {
        let value: Value = serde_json::from_str(&args[index + 1]).unwrap_or_else(|_| json!({}));
        output.insert(args[index].clone(), value);
        index += 2;
    }
    print_json(&Value::Object(output))
}

fn state(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        return 2;
    };
    let mut state = json!({
        "lookup": {"id": 7, "full_name": "acme/widgets", "config_file": ".woodpecker/untrusted/", "trusted": {"volumes": false}},
        "lookup_trusted": {"id": 8, "full_name": "acme/widgets", "config_file": ".woodpecker/trusted/", "trusted": {"volumes": true}},
        "repo7": {"id": 7, "full_name": "acme/widgets", "config_file": ".woodpecker/untrusted/", "trusted": {"volumes": false}},
        "repo8": {"id": 8, "full_name": "acme/widgets", "config_file": ".woodpecker/trusted/", "trusted": {"volumes": true}},
        "pipelines": env_json("CERTIFY_TEST_PIPELINES", json!([])),
        "detail": env_json("CERTIFY_TEST_DETAIL", Value::Null),
        "details": env_json("CERTIFY_TEST_DETAILS", json!({})),
        "logs": env_json("CERTIFY_TEST_LOGS", json!({})),
    });
    if let Some(object) = state.as_object_mut() {
        if let Ok(repo) = std::env::var("CERTIFY_TEST_REPO7") {
            if !repo.is_empty() {
                object.insert(
                    "repo7".into(),
                    serde_json::from_str(&repo).unwrap_or(Value::Null),
                );
            }
        }
        if let Ok(repo) = std::env::var("CERTIFY_TEST_REPO8") {
            if !repo.is_empty() {
                object.insert(
                    "repo8".into(),
                    serde_json::from_str(&repo).unwrap_or(Value::Null),
                );
            }
        }
    }
    match util::write_json_compact(Path::new(path), &state) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("fixture state: {error}");
            1
        }
    }
}

fn logs(args: &[String]) -> i32 {
    let mut output = Map::new();
    let mut index = 0;
    while index + 1 < args.len() {
        let block = std::fs::read_to_string(&args[index]).unwrap_or_default();
        output.insert(args[index + 1].clone(), json!([{"data": block}]));
        index += 2;
    }
    print_json(&Value::Object(output))
}

fn repo(args: &[String]) -> i32 {
    if args.len() < 2 {
        return 2;
    }
    print_json(&json!({"full_name": args[0], "description": args[1]}))
}

fn status(args: &[String]) -> i32 {
    if args.len() < 4 {
        return 2;
    }
    let (sha, top_state, context, state) = (&args[0], &args[1], &args[2], &args[3]);
    let mut entry = json!({"context": context, "state": state});
    if state == "success" {
        entry["updated_at"] = json!("2026-01-01T00:00:00Z");
    }
    let statuses = if context.is_empty() {
        json!([])
    } else {
        json!([entry])
    };
    print_json(&json!({"sha": sha, "state": top_state, "statuses": statuses}))
}

pub fn run(command: &str, args: &[String]) -> i32 {
    match command {
        "pipeline-json" => pipeline(args),
        "detail-json" => detail(args),
        "details-json" => details(args),
        "state" => state(args),
        "logs-json" => logs(args),
        "repo-json" => repo(args),
        "status-json" => status(args),
        other => {
            eprintln!("fixture: unknown command '{other}'");
            2
        }
    }
}
