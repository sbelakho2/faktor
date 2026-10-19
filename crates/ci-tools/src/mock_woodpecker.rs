use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

fn default_repo(repo_id: u64) -> Option<Value> {
    match repo_id {
        7 => Some(json!({
            "id": 7,
            "full_name": "acme/widgets",
            "config_file": ".woodpecker/untrusted/",
            "trusted": {"volumes": false},
        })),
        8 => Some(json!({
            "id": 8,
            "full_name": "acme/widgets",
            "config_file": ".woodpecker/trusted/",
            "trusted": {"volumes": true},
        })),
        _ => None,
    }
}

fn load_state(state_dir: &str) -> Option<Value> {
    let text = std::fs::read_to_string(PathBuf::from(state_dir).join("state.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn reply(stream: &mut TcpStream, status: u16, payload: &Value) {
    let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn route(path: &str, query: &str, state: &Value) -> (u16, Value) {
    if path == "/api/repos/lookup/acme/widgets" {
        let trusted = query.split('&').any(|part| part == "project=trusted");
        let key = if trusted { "lookup_trusted" } else { "lookup" };
        let fallback = if trusted {
            default_repo(8)
        } else {
            default_repo(7)
        };
        let value = state
            .get(key)
            .cloned()
            .filter(|value| !value.is_null())
            .or(fallback)
            .unwrap_or_else(|| json!({"message": "repo not found"}));
        return (200, value);
    }
    let Some(rest) = path.strip_prefix("/api/repos/") else {
        return (404, json!({"message": "not found"}));
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() == 1 && segments[0].chars().all(|ch| ch.is_ascii_digit()) {
        let repo_id: u64 = segments[0].parse().unwrap_or(0);
        let value = state
            .get(format!("repo{repo_id}"))
            .cloned()
            .filter(|value| !value.is_null())
            .or_else(|| default_repo(repo_id));
        return match value {
            Some(value) => (200, value),
            None => (404, json!({"message": "repo not found"})),
        };
    }
    if segments.len() == 4 && segments[1] == "logs" {
        let step = segments[3];
        let value = state.get("logs").and_then(|logs| logs.get(step)).cloned();
        return match value {
            Some(value) if !value.is_null() => (200, value),
            _ => (404, json!({"message": "log not found"})),
        };
    }
    if segments.len() >= 2 && segments[1] == "pipelines" {
        if segments.len() == 2 {
            let value = state
                .get("pipelines")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or_else(|| json!([]));
            return (200, value);
        }
        let number = segments[2];
        if segments.len() == 3 {
            let details = state.get("details");
            let mut detail = details
                .and_then(|details| details.get(number))
                .cloned()
                .filter(|value| !value.is_null());
            if detail.is_none() {
                if let Some(candidate) = state.get("detail") {
                    if candidate
                        .get("number")
                        .map(|value| match value {
                            Value::String(text) => text == number,
                            Value::Number(value) => value.to_string() == number,
                            _ => false,
                        })
                        .unwrap_or(false)
                    {
                        detail = Some(candidate.clone());
                    }
                }
            }
            return match detail.filter(|value| value.is_object()) {
                Some(value) => (200, value),
                None => (404, json!({"message": "pipeline not found"})),
            };
        }
        if segments.len() == 5 && segments[3] == "logs" {
            let step = segments[4];
            let value = state.get("logs").and_then(|logs| logs.get(step)).cloned();
            return match value {
                Some(value) if !value.is_null() => (200, value),
                _ => (404, json!({"message": "log not found"})),
            };
        }
    }
    (404, json!({"message": "not found"}))
}

fn handle(mut stream: TcpStream, state_dir: Arc<String>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            }
        }
    }
    if authorization != "Bearer selftest-token" {
        reply(&mut stream, 401, &json!({"message": "unauthorized"}));
        return;
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some(state) = load_state(&state_dir) else {
        reply(&mut stream, 500, &json!({"message": "state unavailable"}));
        return;
    };
    let (status, payload) = route(path, query, &state);
    reply(&mut stream, status, &payload);
}

pub fn run(args: &[String]) -> i32 {
    let mut port = 0u16;
    let mut state_dir = String::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" if index + 1 < args.len() => {
                port = args[index + 1].parse().unwrap_or(0);
                index += 2;
            }
            "--state-dir" if index + 1 < args.len() => {
                state_dir = args[index + 1].clone();
                index += 2;
            }
            value if value.chars().all(|ch| ch.is_ascii_digit()) && port == 0 => {
                port = value.parse().unwrap_or(0);
                index += 1;
            }
            value => {
                state_dir = value.to_string();
                index += 1;
            }
        }
    }
    if state_dir.is_empty() {
        eprintln!("usage: faktor-ci-tools mock-woodpecker --port N --state-dir DIR");
        return 2;
    }
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("mock-woodpecker: bind 127.0.0.1:{port}: {error}");
            return 1;
        }
    };
    let state_dir = Arc::new(state_dir);
    for stream in listener.incoming().flatten() {
        let state_dir = state_dir.clone();
        std::thread::spawn(move || handle(stream, state_dir));
    }
    0
}

pub fn free_port() -> i32 {
    match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => match listener.local_addr() {
            Ok(address) => {
                println!("{}", address.port());
                0
            }
            Err(error) => {
                eprintln!("free-port: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("free-port: {error}");
            1
        }
    }
}
