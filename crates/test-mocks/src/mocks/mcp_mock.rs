use std::io::{BufRead, Write};

fn read_frame<R: BufRead>(reader: &mut R) -> Option<Vec<u8>> {
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().ok()?;
        }
    }
    if length == 0 {
        return Some(Vec::new());
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

fn write_frame(body: &str) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn send(id: &str, result: &str) {
    write_frame(&format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}"
    ));
}

fn id_of(message: &J) -> Option<String> {
    message.get("id").map(J::raw)
}

fn text_argument(message: &J) -> String {
    message
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("text"))
        .and_then(J::as_str)
        .unwrap_or("")
        .to_string()
}

fn executed_result(items: &[String]) -> String {
    let values = items
        .iter()
        .map(|item| {
            let mut out = String::from("\"");
            for ch in item.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    other => out.push(other),
                }
            }
            out.push('"');
            out
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"executed\":[{values}]}}")
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn close_stdin_and_sleep() -> ! {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg("exec 0<&-; exec sleep 300");
    let _ = command.exec();
    sleep_forever()
}

#[cfg(not(unix))]
fn close_stdin_and_sleep() -> ! {
    sleep_forever()
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "bridge".into());
    let mut stdin = std::io::stdin().lock();
    match mode.as_str() {
        "close" => {
            let _ = read_frame(&mut stdin);
        }
        "garbage" => loop {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"garbage\n");
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(10));
        },
        "oversized-init" => {
            let pad: usize = std::env::args()
                .nth(2)
                .and_then(|value| value.parse().ok())
                .unwrap_or(1 << 20);
            let _ = read_frame(&mut stdin);
            let body = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"pad\":\"{}\"}}}}",
                "x".repeat(pad)
            );
            write_frame(&body);
            sleep_forever();
        }
        "close-stdin" | "stop-draining" => {
            let _ = read_frame(&mut stdin);
            send("1", "{}");
            if mode == "close-stdin" {
                close_stdin_and_sleep()
            } else {
                sleep_forever()
            }
        }
        "echo" => {
            while let Some(body) = read_frame(&mut stdin) {
                if let Some(message) = parse(&body) {
                    if let Some(id) = id_of(&message) {
                        send(&id, "{}");
                    }
                }
            }
        }
        "stall" => {
            let mut executed: Vec<String> = Vec::new();
            while let Some(body) = read_frame(&mut stdin) {
                let Some(message) = parse(&body) else {
                    continue;
                };
                let method = message
                    .get("method")
                    .and_then(J::as_str)
                    .unwrap_or("")
                    .to_string();
                if method == "stall" {
                    executed = vec!["stall".into()];
                    if let Some(id) = id_of(&message) {
                        send(&id, &executed_result(&executed));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    continue;
                }
                executed.push(method);
                if let Some(id) = id_of(&message) {
                    send(&id, &executed_result(&executed));
                }
            }
        }
        _ => {
            send(
                "0",
                "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"1\"}}",
            );
            while let Some(body) = read_frame(&mut stdin) {
                let Some(message) = parse(&body) else {
                    continue;
                };
                let Some(id) = id_of(&message) else {
                    continue;
                };
                match message.get("method").and_then(J::as_str) {
                    Some("tools/list") => send(
                        &id,
                        "{\"tools\":[{\"name\":\"mock_echo\",\"description\":\"echo text back\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}}}}]}",
                    ),
                    Some("tools/call") => {
                        let text = text_argument(&message);
                        if text == "boom" {
                            send(
                                &id,
                                "{\"content\":[{\"type\":\"text\",\"text\":\"exploded\"}],\"isError\":true}",
                            );
                        } else {
                            let mut escaped = String::new();
                            for ch in text.chars() {
                                match ch {
                                    '"' => escaped.push_str("\\\""),
                                    '\\' => escaped.push_str("\\\\"),
                                    '\n' => escaped.push_str("\\n"),
                                    other => escaped.push(other),
                                }
                            }
                            send(
                                &id,
                                &format!(
                                    "{{\"content\":[{{\"type\":\"text\",\"text\":\"echo:{escaped}\"}}],\"isError\":false}}"
                                ),
                            );
                        }
                    }
                    _ => send(&id, "{}"),
                }
            }
        }
    }
}
