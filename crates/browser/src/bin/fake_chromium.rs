//! Fake Chromium/CDP fixture for the faktor-browser offline test suite.
//!
//! This binary is NOT part of the runtime surface: it is a minimal CDP
//! server over a loopback WebSocket that emulates the small slice of
//! Chromium the browser authority uses, driven by a JSON scenario file. It
//! lets every test run offline in CI:
//!
//! ```text
//! fake_chromium <chromium flags...> \
//!   --fake-scenario <scenario.json> \
//!   --fake-dump-launch <launch.json> \
//!   --fake-journal <journal.jsonl>
//! ```
//!
//! It prints `DevTools listening on ws://127.0.0.1:<port>/devtools/browser/<id>`
//! on stderr (exactly like Chromium), journals every received command and
//! every event it emits, and can exit abruptly to simulate a crash.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

#[derive(Default)]
struct Scenario {
    verification: Value,
    html: String,
    page_url: String,
    eval_value: Value,
    network: Vec<Value>,
    pause_requests: bool,
    body_bytes: usize,
    body_text: Option<String>,
    exit_on_method: Option<String>,
    exit_after_ms: Option<u64>,
    suppress_lifecycle: bool,
    download_will_begin: Option<Value>,
}

impl Scenario {
    fn load(path: &str) -> Scenario {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Scenario::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return Scenario::default();
        };
        Scenario {
            verification: value.get("verification").cloned().unwrap_or(Value::Null),
            html: value
                .get("html")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            page_url: value
                .get("page_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            eval_value: value.get("eval_value").cloned().unwrap_or(Value::Null),
            network: value
                .get("network")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            pause_requests: value
                .get("pause_requests")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            body_bytes: value.get("body_bytes").and_then(Value::as_u64).unwrap_or(0) as usize,
            body_text: value
                .get("body_text")
                .and_then(Value::as_str)
                .map(str::to_string),
            exit_on_method: value
                .get("exit_on_method")
                .and_then(Value::as_str)
                .map(str::to_string),
            exit_after_ms: value.get("exit_after_ms").and_then(Value::as_u64),
            suppress_lifecycle: value
                .get("suppress_lifecycle")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            download_will_begin: value.get("download_will_begin").cloned(),
        }
    }

    fn body_for(&self, request_id: &str) -> String {
        use base64::Engine as _;
        for item in &self.network {
            if item.get("request_id").and_then(Value::as_str) == Some(request_id) {
                if let Some(text) = item.get("body").and_then(Value::as_str) {
                    return base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                }
            }
        }
        if let Some(text) = &self.body_text {
            return base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        }
        if self.body_bytes > 0 {
            let bytes = vec![b'A'; self.body_bytes];
            return base64::engine::general_purpose::STANDARD.encode(bytes);
        }
        base64::engine::general_purpose::STANDARD.encode(b"")
    }
}

struct Journal {
    file: Option<Mutex<std::fs::File>>,
}

impl Journal {
    fn open(path: Option<&str>) -> Self {
        let file = path
            .and_then(|p| std::fs::File::create(p).ok())
            .map(Mutex::new);
        Self { file }
    }

    fn write(&self, entry: Value) {
        let Some(file) = &self.file else {
            return;
        };
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{entry}");
            let _ = file.flush();
        }
    }
}

struct Server {
    scenario: Scenario,
    next_target: AtomicU64,
    next_session: AtomicU64,
    next_context: AtomicU64,
    sessions: Mutex<HashMap<String, String>>,
    targets: Mutex<HashMap<String, String>>,
}

fn arg_value(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find(|a| a.starts_with(prefix))
        .map(|a| a[prefix.len()..].to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario_path = arg_value(&args, "--fake-scenario=");
    let dump_path = arg_value(&args, "--fake-dump-launch=");
    let journal_path = arg_value(&args, "--fake-journal=");
    let scenario = scenario_path
        .as_deref()
        .map(Scenario::load)
        .unwrap_or_default();
    if let Some(path) = &dump_path {
        let env: HashMap<String, String> = std::env::vars().collect();
        let dump = json!({ "argv": args, "env": env });
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&dump).unwrap_or_default());
    }
    let journal = Arc::new(Journal::open(journal_path.as_deref()));
    journal.write(json!({
        "dir": "launch",
        "argv": args,
        "pid": std::process::id(),
    }));
    if let Some(ms) = scenario.exit_after_ms {
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            std::process::exit(9);
        });
    }
    let requested_port: u16 = arg_value(&args, "--remote-debugging-port=")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", requested_port))
        .await
        .expect("fake chromium bind");
    let port = listener.local_addr().expect("fake chromium addr").port();
    eprintln!("fake chromium starting (offline fixture)");
    eprintln!(
        "DevTools listening on ws://127.0.0.1:{port}/devtools/browser/{}",
        uuid::Uuid::new_v4()
    );
    std::io::stderr().flush().ok();
    let server = Arc::new(Server {
        scenario,
        next_target: AtomicU64::new(1),
        next_session: AtomicU64::new(1),
        next_context: AtomicU64::new(1),
        sessions: Mutex::new(HashMap::new()),
        targets: Mutex::new(HashMap::new()),
    });
    // Share the same journal handle across connections.
    let journal = journal.clone();
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            break;
        };
        let server = server.clone();
        let journal = journal.clone();
        tokio::spawn(async move {
            serve_connection(stream, server, journal).await;
        });
    }
}

async fn serve_connection(stream: TcpStream, server: Arc<Server>, journal: Arc<Journal>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut source) = ws.split();
    while let Some(Ok(message)) = source.next().await {
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(command) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let id = command.get("id").cloned().unwrap_or(Value::Null);
        let method = command
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let session = command
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = command.get("params").cloned().unwrap_or(Value::Null);
        journal.write(json!({
            "dir": "recv",
            "method": method,
            "session": session,
            "params": params,
        }));
        if server.scenario.exit_on_method.as_deref() == Some(method.as_str()) {
            std::process::exit(9);
        }
        if method == "Browser.close" {
            let _ = sink
                .send(Message::text(reply(id, session, json!({}))))
                .await;
            break;
        }
        let response = handle_command(&server, &method, &params, &journal);
        let payload = match response {
            Ok(result) => reply(id, session.clone(), result),
            Err((code, message)) => error_reply(id, session.clone(), code, message),
        };
        if sink.send(Message::text(payload)).await.is_err() {
            break;
        }
        if method == "Page.navigate" {
            for event in navigation_events(&server, session.as_deref()) {
                journal.write(json!({
                    "dir": "sent",
                    "method": event.get("method").cloned().unwrap_or(Value::Null),
                    "session": event.get("sessionId").cloned().unwrap_or(Value::Null),
                }));
                let _ = sink.send(Message::text(event.to_string())).await;
            }
        }
    }
    journal.write(json!({"dir": "closed"}));
}

fn reply(id: Value, session: Option<String>, result: Value) -> String {
    let mut message = json!({ "id": id, "result": result });
    if let Some(session) = session {
        message["sessionId"] = json!(session);
    }
    message.to_string()
}

fn error_reply(id: Value, session: Option<String>, code: i64, message: String) -> String {
    let mut value = json!({ "id": id, "error": { "code": code, "message": message } });
    if let Some(session) = session {
        value["sessionId"] = json!(session);
    }
    value.to_string()
}

fn handle_command(
    server: &Arc<Server>,
    method: &str,
    params: &Value,
    journal: &Arc<Journal>,
) -> Result<Value, (i64, String)> {
    match method {
        "Browser.getVersion" => Ok(json!({
            "protocolVersion": "1.3",
            "product": "FakeChromium/1.0 (faktor-browser fixture)",
            "revision": "fixture",
            "userAgent": "FakeChromium/1.0",
            "jsVersion": "0",
        })),
        "Browser.setDownloadBehavior" => Ok(json!({})),
        "Browser.cancelDownload" => Ok(json!({})),
        "Target.createBrowserContext" => {
            let id = format!("bc{}", server.next_context.fetch_add(1, Ordering::SeqCst));
            Ok(json!({ "browserContextId": id }))
        }
        "Target.disposeBrowserContext" => Ok(json!({})),
        "Target.createTarget" => {
            let target = format!("t{}", server.next_target.fetch_add(1, Ordering::SeqCst));
            server
                .targets
                .lock()
                .unwrap()
                .insert(target.clone(), String::new());
            Ok(json!({ "targetId": target }))
        }
        "Target.attachToTarget" => {
            let target = params
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let session_id = format!("s{}", server.next_session.fetch_add(1, Ordering::SeqCst));
            server
                .sessions
                .lock()
                .unwrap()
                .insert(session_id.clone(), target);
            Ok(json!({ "sessionId": session_id }))
        }
        "Target.closeTarget" => {
            if let Some(target) = params.get("targetId").and_then(Value::as_str) {
                server.targets.lock().unwrap().remove(target);
            }
            Ok(json!({}))
        }
        "Page.enable"
        | "Network.enable"
        | "Runtime.enable"
        | "Fetch.enable"
        | "Network.setCacheDisabled"
        | "Network.setBlockedURLs" => Ok(json!({})),
        "Page.navigate" => Ok(json!({ "frameId": "f1", "loaderId": "l1" })),
        "Page.stopLoading" => Ok(json!({})),
        "Page.captureScreenshot" => {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(b"fake-png-bytes");
            Ok(json!({ "data": data }))
        }
        "Runtime.evaluate" => {
            let expression = params
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let value = if expression.contains("faktor-verify-probe") {
                serde_json::to_string(&server.scenario.verification).unwrap_or_default()
            } else if expression.contains("outerHTML") {
                server.scenario.html.clone()
            } else if expression.contains("location.href") {
                server.scenario.page_url.clone()
            } else {
                return Ok(
                    json!({ "result": { "type": "object", "value": server.scenario.eval_value } }),
                );
            };
            Ok(json!({ "result": { "type": "string", "value": value } }))
        }
        "Network.getResponseBody" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(json!({ "body": server.scenario.body_for(request_id), "base64Encoded": true }))
        }
        "Fetch.continueRequest" | "Fetch.failRequest" => Ok(json!({})),
        other => {
            journal.write(json!({"dir": "unknown", "method": other}));
            Err((-32601, format!("method not found: {other}")))
        }
    }
}

/// Emit the scripted navigation lifecycle, network and interception events
/// for the session that issued `Page.navigate`.
fn navigation_events(server: &Arc<Server>, session: Option<&str>) -> Vec<Value> {
    let mut events = Vec::new();
    let mut push = |method: &str, params: Value| {
        let mut event = json!({ "method": method, "params": params });
        if let Some(session) = session {
            event["sessionId"] = json!(session);
        }
        events.push(event);
    };
    if !server.scenario.suppress_lifecycle {
        push(
            "Page.frameNavigated",
            json!({ "frame": { "id": "f1", "url": server.scenario.page_url } }),
        );
        push("Page.domContentEventFired", json!({ "timestamp": 1.0 }));
    }
    for (index, item) in server.scenario.network.iter().enumerate() {
        let request_id = item
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("n{index}"));
        let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
        let resource_type = item
            .get("resource_type")
            .and_then(Value::as_str)
            .unwrap_or("Other");
        push(
            "Network.requestWillBeSent",
            json!({
                "requestId": request_id,
                "type": resource_type,
                "request": { "url": url, "method": "GET" }
            }),
        );
        if server.scenario.pause_requests {
            push(
                "Fetch.requestPaused",
                json!({
                    "requestId": request_id,
                    "resourceType": resource_type,
                    "request": { "url": url, "method": "GET" }
                }),
            );
        } else {
            push(
                "Network.responseReceived",
                json!({
                    "requestId": request_id,
                    "type": resource_type,
                    "response": {
                        "status": item.get("status").and_then(Value::as_i64).unwrap_or(200),
                        "mimeType": item.get("mime_type").and_then(Value::as_str).unwrap_or("text/plain")
                    }
                }),
            );
            push(
                "Network.loadingFinished",
                json!({ "requestId": request_id, "encodedDataLength": 42 }),
            );
        }
    }
    if let Some(download) = &server.scenario.download_will_begin {
        push("Browser.downloadWillBegin", download.clone());
    }
    if !server.scenario.suppress_lifecycle {
        push("Page.loadEventFired", json!({ "timestamp": 2.0 }));
    }
    events
}
