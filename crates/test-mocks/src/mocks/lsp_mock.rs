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

fn send(id: &str, result: &str) {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}");
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn log(message: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{message}");
    let _ = err.flush();
}

fn check(condition: bool, what: &str) {
    log(&format!("{} {what}", if condition { "OK" } else { "FAIL" }));
}

fn range_json(line: u32, start: u32, end: u32) -> String {
    format!(
        "{{\"start\":{{\"line\":{line},\"character\":{start}}},\
         \"end\":{{\"line\":{line},\"character\":{end}}}}}"
    )
}

fn location_json(uri: &str, line: u32) -> String {
    format!(
        "{{\"uri\":{},\"range\":{}}}",
        quote(uri),
        range_json(line, 4, 12)
    )
}

fn hierarchy_item(name: &str, uri: &str, line: u32) -> String {
    format!(
        "{{\"name\":{},\"kind\":12,\"uri\":{},\"range\":{},\
         \"selectionRange\":{},\"detail\":{},\"data\":{{\"mock\":{}}}}}",
        quote(name),
        quote(uri),
        range_json(line, 0, 20),
        range_json(line, 4, 12),
        quote(&format!("fn {name}")),
        quote(name)
    )
}

fn array_of(count: usize, mut render: impl FnMut(usize) -> String) -> String {
    let mut out = String::from("[");
    for index in 0..count {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&render(index));
    }
    out.push(']');
    out
}

fn capabilities_for(mode: &str) -> String {
    if mode == "no-capabilities" {
        return "{}".into();
    }
    let mut providers = vec![
        "\"definitionProvider\":true",
        "\"referencesProvider\":true",
        "\"workspaceSymbolProvider\":true",
        "\"documentSymbolProvider\":true",
        "\"textDocumentSync\":{\"change\":1}",
    ];
    if mode != "partial-capabilities" {
        providers.push("\"callHierarchyProvider\":true");
    }
    format!("{{{}}}", providers.join(","))
}

/// Answer `slow` requests asynchronously so the client can cancel/time out
/// while the main read loop keeps serving later frames.
fn reply(id: String, result: String, mode: &str) {
    if mode == "slow" {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(4));
            send(&id, &result);
        });
    } else {
        send(&id, &result);
    }
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
    command
        .arg("-c")
        .arg("exec 0<&-; echo 'CLOSED stdin' >&2; exec sleep 300");
    let _ = command.exec();
    sleep_forever()
}

#[cfg(not(unix))]
fn close_stdin_and_sleep() -> ! {
    sleep_forever()
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "plain".into());
    let expected_root = std::env::args().nth(2).unwrap_or_default();
    let expected_uri = std::env::args().nth(3).unwrap_or_default();
    if mode == "close" {
        log("mock closing before handshake");
        return;
    }
    if mode == "flood" {
        std::thread::spawn(|| loop {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(&vec![b'z'; 8192]);
            let _ = err.flush();
        });
    }
    let mut stdin = std::io::stdin().lock();
    let mut initialized_seen = false;
    while let Some(body) = read_frame(&mut stdin) {
        let Some(message) = parse(&body) else {
            continue;
        };
        let Some(method) = message.get("method").and_then(J::as_str) else {
            continue;
        };
        let id = message.get("id").map(J::raw);
        match method {
            "initialize" => {
                let params = message.get("params");
                let root_uri = params
                    .and_then(|params| params.get("rootUri"))
                    .and_then(J::as_str)
                    .unwrap_or("");
                check(root_uri == expected_uri, "initialize rootUri");
                let cwd = std::env::current_dir()
                    .ok()
                    .and_then(|path| std::fs::canonicalize(path).ok());
                let root = std::fs::canonicalize(&expected_root).ok();
                check(cwd == root, "initialize cwd");
                if let Some(id) = id {
                    let capabilities = capabilities_for(&mode);
                    send(
                        &id,
                        &format!(
                            "{{\"capabilities\":{capabilities},\
                             \"serverInfo\":{{\"name\":\"mock-lsp\",\"version\":\"1\"}}}}"
                        ),
                    );
                }
                log("SENT initialize result");
            }
            "initialized" => {
                initialized_seen = true;
                log("GOT initialized");
            }
            "textDocument/didOpen" => {
                check(initialized_seen, "didOpen after initialized");
                log("GOT didOpen");
                if mode == "stops-draining" {
                    log("STOPPED draining");
                    sleep_forever();
                }
                if mode == "close-stdin" {
                    // The close MUST happen before the log: the test uses the
                    // log as the ordering proof that the next request can no
                    // longer reach the server.
                    close_stdin_and_sleep();
                }
            }
            "textDocument/didChange" => log("GOT didChange"),
            "textDocument/didClose" => log("GOT didClose"),
            "textDocument/documentSymbol" => {
                log("GOT documentSymbol");
                if let Some(id) = id {
                    reply(id, "[]".into(), &mode);
                }
            }
            "textDocument/definition" => {
                log("GOT definition");
                let target = format!("{expected_uri}/target.rs");
                let result = match mode.as_str() {
                    "hostile" => "\"garbage\"".to_string(),
                    "bounded" => array_of(5000, |line| location_json(&target, line as u32)),
                    _ => location_json(&target, 7),
                };
                if let Some(id) = id {
                    reply(id, result, &mode);
                }
            }
            "textDocument/references" => {
                log("GOT references");
                let target = format!("{expected_uri}/target.rs");
                let include_declaration = !matches!(
                    message
                        .get("params")
                        .and_then(|params| params.get("context"))
                        .and_then(|context| context.get("includeDeclaration")),
                    Some(J::Bool(false))
                );
                let result = if mode == "bounded" {
                    array_of(5000, |line| location_json(&target, line as u32))
                } else if include_declaration {
                    format!(
                        "[{},{}]",
                        location_json(&target, 7),
                        location_json(&target, 11)
                    )
                } else {
                    format!("[{}]", location_json(&target, 11))
                };
                if let Some(id) = id {
                    reply(id, result, &mode);
                }
            }
            "workspace/symbol" => {
                log("GOT workspaceSymbol");
                let target = format!("{expected_uri}/target.rs");
                let result = match mode.as_str() {
                    "hostile" => "{\"name\":\"not-an-array\"}".to_string(),
                    "bounded" => array_of(5000, |line| {
                        format!(
                            "{{\"name\":\"symbol_{line}\",\"kind\":12,\
                             \"location\":{{\"uri\":{},\"range\":{}}}}}",
                            quote(&target),
                            range_json(line as u32, 4, 12)
                        )
                    }),
                    _ => format!(
                        "[{{\"name\":\"Counter\",\"kind\":23,\"containerName\":\"crate\",\
                          \"location\":{{\"uri\":{},\"range\":{}}}}}]",
                        quote(&target),
                        range_json(3, 4, 12)
                    ),
                };
                if let Some(id) = id {
                    reply(id, result, &mode);
                }
            }
            "textDocument/prepareCallHierarchy" => {
                log("GOT prepareCallHierarchy");
                let target = format!("{expected_uri}/target.rs");
                let result = match mode.as_str() {
                    "hostile" => "\"garbage\"".to_string(),
                    "bounded" => array_of(2000, |line| {
                        hierarchy_item(&format!("f{line}"), &target, line as u32)
                    }),
                    _ => format!("[{}]", hierarchy_item("add_one", &target, 7)),
                };
                if let Some(id) = id {
                    reply(id, result, &mode);
                }
            }
            "callHierarchy/incomingCalls" => {
                log("GOT incomingCalls");
                let target = format!("{expected_uri}/target.rs");
                let result = if mode == "bounded" {
                    array_of(2000, |line| {
                        format!(
                            "{{\"from\":{},\"fromRanges\":[{}]}}",
                            hierarchy_item(&format!("f{line}"), &target, line as u32),
                            range_json(line as u32, 0, 8)
                        )
                    })
                } else {
                    format!(
                        "[{{\"from\":{},\"fromRanges\":[{}]}}]",
                        hierarchy_item("compute", &target, 11),
                        range_json(12, 0, 8)
                    )
                };
                if let Some(id) = id {
                    reply(id, result, &mode);
                }
            }
            "callHierarchy/outgoingCalls" => {
                log("GOT outgoingCalls");
                let target = format!("{expected_uri}/target.rs");
                let result = if mode == "bounded" {
                    array_of(2000, |line| {
                        format!(
                            "{{\"to\":{},\"fromRanges\":[{}]}}",
                            hierarchy_item(&format!("f{line}"), &target, line as u32),
                            range_json(line as u32, 0, 8)
                        )
                    })
                } else {
                    format!(
                        "[{{\"to\":{},\"fromRanges\":[{}]}}]",
                        hierarchy_item("add_one", &target, 7),
                        range_json(12, 4, 11)
                    )
                };
                if let Some(id) = id {
                    reply(id, result, &mode);
                }
            }
            "$/cancelRequest" => log("GOT cancel"),
            "shutdown" => {
                if let Some(id) = id {
                    send(&id, "null");
                }
                log("SENT shutdown result");
            }
            "exit" => {
                log("GOT exit");
                break;
            }
            _ => {}
        }
    }
    log("mock exiting");
}
