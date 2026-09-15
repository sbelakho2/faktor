//! Adversarial end-to-end ACP v1 wire tests over in-memory duplex pipes.
//!
//! Every scenario drives a real `AcpServer` through `serve_connection` with
//! raw bytes on one side and parsed JSON-RPC on the other: handshake
//! lifecycle, hostile framing, fragmentation, pipelining, malformed
//! payloads, backend failures, ordering with a slow backend, size bounds,
//! and hostile UTF-8/NUL round-trips. Assertions target the official ACP
//! v1 wire shapes (`sessionId`, `stopReason`, official error format).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_agent::PermissionRequester;
use faktor_core::capability::{Capability, PermissionDecision};
use faktor_core::id::{EventSeq, OpId, SessionId};
use faktor_protocol::v756::{
    Message as NativeMessage, MessagesPage, PageMeta, Part as NativePart, ToolResultBody,
};
use faktor_server::ChannelPermissionRequester;
use faktor_session::ops::PermissionRequest;

use faktor_acp::protocol::{frame, notification_frame, parse_frame};
use faktor_acp::{
    agent_thought_chunk_update, session_update_params, text_chunk_update, tool_call_from_native,
    tool_result_from_native, user_message_chunk_update, AcpBackend, AcpConfig, AcpServer,
    AcpStreamBackend, PermissionOption, PromptCtx, TerminalAuthority, TerminalError,
    TerminalHandle, TerminalSpec,
};
use serde_json::{json, Value};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::time::timeout;

const RX_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// Deterministic echo backend: `sess-0`, `sess-1`, …; prompt echoes text.
struct EchoBackend {
    sessions: Mutex<Vec<String>>,
    next: AtomicU64,
}

impl EchoBackend {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }
}

impl AcpBackend for EchoBackend {
    fn agent_info(&self) -> Value {
        json!({
            "name": "faktor-test-agent",
            "version": "0.0.0",
            "capabilities": { "prompt": true, "sessions": true },
        })
    }
    fn create_session(&self, params: &Value) -> Result<String, String> {
        if params.get("fail").is_some() {
            return Err("session creation refused by backend".into());
        }
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let id = format!("sess-{n}");
        self.sessions.lock().unwrap().push(id.clone());
        Ok(id)
    }
    fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
        Ok(json!({ "echo": text, "session": session_id }))
    }
    fn abort(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn list_sessions(&self) -> Vec<String> {
        self.sessions.lock().unwrap().clone()
    }
}

/// Backend whose every call fails: failures must surface as official
/// internal errors (`-32603` with the message in `data`).
struct FailingBackend;

impl AcpBackend for FailingBackend {
    fn agent_info(&self) -> Value {
        json!({ "name": "failing", "version": "0.0.0" })
    }
    fn create_session(&self, _params: &Value) -> Result<String, String> {
        Err("create exploded".into())
    }
    fn prompt(&self, _session_id: &str, _text: &str) -> Result<Value, String> {
        Err("backend exploded".into())
    }
    fn abort(&self, _session_id: &str) -> Result<(), String> {
        Err("abort exploded".into())
    }
    fn list_sessions(&self) -> Vec<String> {
        vec![]
    }
}

/// Prompt answers with a ~9 MiB blob: must be refused by the 8 MiB bound.
struct HugeBackend;

impl AcpBackend for HugeBackend {
    fn agent_info(&self) -> Value {
        json!({ "name": "huge", "version": "0.0.0" })
    }
    fn create_session(&self, _params: &Value) -> Result<String, String> {
        Ok("sess-huge".into())
    }
    fn prompt(&self, _session_id: &str, _text: &str) -> Result<Value, String> {
        Ok(json!({ "blob": "x".repeat(9 * 1024 * 1024) }))
    }
    fn abort(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn list_sessions(&self) -> Vec<String> {
        vec!["sess-huge".into()]
    }
}

// ---------------------------------------------------------------------------
// Wire harness
// ---------------------------------------------------------------------------

type ClientRead = ReadHalf<DuplexStream>;
type ClientWrite = WriteHalf<DuplexStream>;

/// Byte-level ACP client over the other end of the duplex pipe.
struct WireClient {
    read: ClientRead,
    write: ClientWrite,
    recv_buf: Vec<u8>,
    next_id: u64,
}

fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

impl WireClient {
    async fn recv_frame(&mut self) -> Result<Option<Value>, String> {
        loop {
            match parse_frame(&self.recv_buf) {
                Ok(Some((consumed, value))) => {
                    self.recv_buf.drain(..consumed);
                    return Ok(Some(value));
                }
                Ok(None) => {
                    let mut chunk = [0u8; 8192];
                    let n = timeout(RX_TIMEOUT, self.read.read(&mut chunk))
                        .await
                        .map_err(|_| "timed out waiting for server bytes".to_string())?
                        .map_err(|e| format!("read: {e}"))?;
                    if n == 0 {
                        return Ok(None);
                    }
                    self.recv_buf.extend_from_slice(&chunk[..n]);
                }
                Err(msg) => return Err(format!("client framing error: {msg}")),
            }
        }
    }

    async fn expect_message(&mut self) -> Value {
        self.recv_frame()
            .await
            .expect("server response frame")
            .unwrap_or_else(|| panic!("server closed the pipe before answering"))
    }

    async fn expect_eof(&mut self) {
        let rest = self.recv_frame().await.expect("clean server close");
        assert!(rest.is_none(), "expected EOF, got frame: {rest:?}");
    }

    /// Send one raw JSON value as a request/notification frame.
    async fn send_value(&mut self, value: &Value) {
        let bytes = faktor_acp::protocol::encode(value).expect("test frame encodes");
        self.write.write_all(&bytes).await.expect("client write");
        self.write.flush().await.expect("client flush");
    }

    /// Send one pre-encoded raw frame (bytes).
    async fn send_bytes(&mut self, bytes: &[u8]) {
        self.write.write_all(bytes).await.expect("client write");
        self.write.flush().await.expect("client flush");
    }

    /// Read frames until `predicate` matches.
    async fn recv_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..10_000 {
            let msg = self.expect_message().await;
            if predicate(&msg) {
                return msg;
            }
        }
        panic!("timed out waiting for a matching frame");
    }

    /// Send a request and read frames until the matching id answers (turn
    /// responses and updates may arrive before it). Server→client requests
    /// (permissions, client fs) are skipped here; scenarios that must
    /// answer them read frames explicitly.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let bytes = frame(method.to_string(), id, params);
        self.write.write_all(&bytes).await.expect("client write");
        self.write.flush().await.expect("client flush");
        loop {
            let msg = self.expect_message().await;
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            if msg["id"] == json!(id) && is_response {
                return msg;
            }
        }
    }

    /// Send one JSON-RPC response to a server→client request.
    async fn respond(&mut self, id: u64, result: Value) {
        self.send_value(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await;
    }

    /// Send one JSON-RPC *error* response to a server→client request.
    #[allow(dead_code)]
    async fn respond_error(&mut self, id: u64, code: i64, message: &str) {
        self.send_value(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await;
    }

    /// Read frames until a server→client request with `method` arrives,
    /// returning its id and params. Updates/responses in between are
    /// returned to the caller via `seen` for ordering assertions.
    async fn recv_server_request(&mut self, method: &str, seen: &mut Vec<Value>) -> (u64, Value) {
        for _ in 0..10_000 {
            let msg = self.expect_message().await;
            if msg.get("method").and_then(Value::as_str) == Some(method) {
                let id = msg["id"].as_u64().expect("server request id");
                return (id, msg["params"].clone());
            }
            seen.push(msg);
        }
        panic!("server request {method} never arrived");
    }

    async fn error_code_of(&mut self, method: &str, params: Value) -> i64 {
        let msg = self.request(method, params).await;
        msg["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("expected error response, got: {msg}"))
    }
}

fn start_server<B: AcpBackend + 'static>(
    backend: B,
) -> (WireClient, tokio::task::JoinHandle<Result<(), String>>) {
    start_server_with_config(backend, AcpConfig::default())
}

fn start_server_with_config<B: AcpBackend + 'static>(
    backend: B,
    config: AcpConfig,
) -> (WireClient, tokio::task::JoinHandle<Result<(), String>>) {
    let (server_side, client_side) = duplex(4 * 1024 * 1024);
    let (server_r, server_w) = tokio::io::split(server_side);
    let (client_r, client_w) = tokio::io::split(client_side);
    let server = AcpServer::new(backend).with_config(config);
    let handle = tokio::spawn(async move { server.serve_connection(server_r, server_w).await });
    let client = WireClient {
        read: client_r,
        write: client_w,
        recv_buf: Vec::new(),
        next_id: 1,
    };
    (client, handle)
}

fn start_streaming_server_with_config<B: AcpStreamBackend + 'static>(
    backend: B,
    config: AcpConfig,
) -> (WireClient, tokio::task::JoinHandle<Result<(), String>>) {
    let (server_side, client_side) = duplex(4 * 1024 * 1024);
    let (server_r, server_w) = tokio::io::split(server_side);
    let (client_r, client_w) = tokio::io::split(client_side);
    let server = AcpServer::new_streaming(backend).with_config(config);
    let handle = tokio::spawn(async move { server.serve_connection(server_r, server_w).await });
    let client = WireClient {
        read: client_r,
        write: client_w,
        recv_buf: Vec::new(),
        next_id: 1,
    };
    (client, handle)
}

/// Assert the frame carries exactly the fixture's shape. Key order is
/// deliberately not compared: the workspace may build `serde_json` with
/// `preserve_order` (other workspace members' test dependencies enable it),
/// which makes canonical key order build-dependent. Field names, values,
/// presence and null behavior are still compared exactly.
fn assert_canonical(frame: &Value, expected: &str) {
    let expected: Value = serde_json::from_str(expected).expect("expected frame is JSON");
    assert_eq!(*frame, expected, "frame differs from the canonical golden");
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_handshake_lifecycle_initialize_session_prompt_shutdown() {
    let (mut client, server_task) = start_server(EchoBackend::new());

    // initialize: official v1 handshake.
    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "clientInfo": { "name": "test-client", "version": "1.0.0" } }),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], false);
    assert_eq!(init["result"]["authMethods"], json!([]));

    // agent_info passthrough (extension).
    let info = client.request("agent_info", json!({})).await;
    assert_eq!(info["result"]["capabilities"]["prompt"], true);

    // session/new -> official {sessionId}.
    let new = client.request("session/new", json!({})).await;
    assert_eq!(new["result"]["sessionId"], "sess-0");
    let new2 = client.request("session/new", json!({})).await;
    assert_eq!(new2["result"]["sessionId"], "sess-1");

    // session/list reflects both sessions as objects.
    let list = client.request("session/list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["sess-0", "sess-1"]
    );

    // session/prompt answers with the official stopReason; the backend
    // report rides the official _meta extension member.
    let p = client
        .request("session/prompt", prompt_params("sess-0", "hello agent"))
        .await;
    assert_eq!(p["result"]["stopReason"], "end_turn");
    assert_eq!(p["result"]["_meta"]["echo"], "hello agent");

    // deprecated session/abort alias: cancel semantics (empty ack).
    let abort = client
        .request("session/abort", json!({ "sessionId": "sess-0" }))
        .await;
    assert_eq!(abort["result"], json!({}));

    // shutdown answers, then the serve loop ends and the pipe closes.
    let bye = client.request("shutdown", json!({})).await;
    assert_eq!(bye["result"], json!({ "ok": true }));
    client.expect_eof().await;
    timeout(RX_TIMEOUT, server_task)
        .await
        .expect("server task hung")
        .unwrap()
        .expect("server clean exit");
}

#[tokio::test]
async fn version_handshake_accepts_1_and_rejects_2_loudly() {
    let (mut client, _task) = start_server(EchoBackend::new());

    let msg = client
        .request("initialize", json!({ "protocolVersion": 2 }))
        .await;
    assert_eq!(msg["error"]["code"], -32602);
    assert_eq!(msg["error"]["data"]["supportedProtocolVersion"], 1);
    assert_eq!(msg["error"]["data"]["protocolVersion"], 2);
    // No silent fallback: the old string version is rejected too.
    let msg = client
        .request("initialize", json!({ "protocolVersion": "0.1.0" }))
        .await;
    assert_eq!(msg["error"]["code"], -32602);

    // v1 accepted after rejections; the server keeps serving.
    let msg = client
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    assert_eq!(msg["result"]["protocolVersion"], 1);
    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "faktor-test-agent");
}

#[tokio::test]
async fn unknown_method_is_32601() {
    let (mut client, _task) = start_server(EchoBackend::new());
    let msg = client.request("bogus/method", json!({})).await;
    assert_eq!(msg["error"]["code"], -32601);
    let msg = client.request("session/close", json!({})).await;
    assert_eq!(msg["error"]["code"], -32601);
}

#[tokio::test]
async fn malformed_json_body_is_32700_and_loop_survives() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // Broken body with a VALID Content-Length: official parse error
    // (canonical message, null id), loop survives.
    let broken = b"Content-Length: 10\r\n\r\n{\"broken\":".to_vec();
    client.write.write_all(&broken).await.unwrap();
    client.write.flush().await.unwrap();
    let err = client.expect_message().await;
    assert!(err["id"].is_null());
    assert_eq!(err["error"]["code"], -32700);
    assert_eq!(err["error"]["message"], "Parse error");
    assert!(
        err["error"].get("data").is_none(),
        "no data member when absent"
    );

    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "faktor-test-agent");
}

#[tokio::test]
async fn hostile_20_mib_content_length_rejected_no_oom_then_connection_closed() {
    let (mut client, task) = start_server(EchoBackend::new());

    // Header alone declares a 20 MiB body: rejected instantly, before any
    // body byte is buffered (no OOM, no 20 MiB allocation).
    let hostile = b"Content-Length: 20971520\r\n\r\n".to_vec();
    client.write.write_all(&hostile).await.unwrap();
    client.write.flush().await.unwrap();
    let err = client.expect_message().await;
    assert!(err["id"].is_null());
    assert_eq!(err["error"]["code"], -32700);

    // Framing is unrecoverable: server closes the connection.
    client.expect_eof().await;
    timeout(RX_TIMEOUT, task)
        .await
        .expect("server task hung")
        .unwrap()
        .expect("server clean exit");
}

#[tokio::test]
async fn fragmented_frames_reassemble_across_arbitrary_boundaries() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // Write one request in 1–3 byte dribbles; the server must reassemble
    // across header/body byte boundaries.
    let bytes = frame(
        "session/prompt".to_string(),
        42,
        prompt_params("sess-0", "fragmented"),
    );
    let mut step = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let take = (i + step).min(bytes.len());
        client.write.write_all(&bytes[i..take]).await.unwrap();
        client.write.flush().await.unwrap();
        i = take;
        step = step % 3 + 1;
    }
    let msg = client.expect_message().await;
    assert_eq!(msg["id"], 42);
    assert_eq!(msg["result"]["stopReason"], "end_turn");
    assert_eq!(msg["result"]["_meta"]["echo"], "fragmented");
}

#[tokio::test]
async fn two_requests_pipelined_in_one_write_answered_in_order() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // Same-session prompts pipeline through the per-session queue: the
    // second runs only after the first terminal, so answers arrive in
    // request order.
    let a = frame("session/prompt".to_string(), 1, prompt_params("s", "first"));
    let b = frame(
        "session/prompt".to_string(),
        2,
        prompt_params("s", "second"),
    );
    let mut both = a;
    both.extend_from_slice(&b);
    client.write.write_all(&both).await.unwrap();
    client.write.flush().await.unwrap();

    let first = client.expect_message().await;
    let second = client.expect_message().await;
    assert_eq!(first["id"], 1);
    assert_eq!(first["result"]["_meta"]["echo"], "first");
    assert_eq!(second["id"], 2);
    assert_eq!(second["result"]["_meta"]["echo"], "second");
}

#[tokio::test]
async fn backend_errors_surface_as_internal_error_with_data() {
    let (mut client, _task) = start_server(FailingBackend);

    // Official into_internal_error convention: -32603 + data carries the
    // backend message. (-32000 is officially "Authentication required"
    // and must NOT be used for backend failures.)
    let msg = client
        .request("session/prompt", prompt_params("s", "hi"))
        .await;
    assert_eq!(msg["error"]["code"], -32603);
    assert_eq!(msg["error"]["message"], "Internal error");
    assert_eq!(msg["error"]["data"], "backend exploded");

    let msg = client.request("session/new", json!({})).await;
    assert_eq!(msg["error"]["code"], -32603);
    assert_eq!(msg["error"]["data"], "create exploded");
}

#[tokio::test]
async fn notification_does_not_deadlock_and_shutdown_notification_ends_loop() {
    let (mut client, task) = start_server(EchoBackend::new());

    // A notification (null id) must be ignored without blocking the loop.
    let note = notification_frame("session/update".into(), json!({ "sessionId": "sess-0" }));
    client.write.write_all(&note).await.unwrap();
    client.write.flush().await.unwrap();

    // Also an invalid-but-ignorable notification.
    let note = notification_frame("no/such/method".into(), json!({}));
    client.write.write_all(&note).await.unwrap();
    client.write.flush().await.unwrap();

    // The next request is answered: the loop never stalled.
    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "faktor-test-agent");

    // A shutdown-shaped notification cannot be answered but still ends the
    // loop ("shutdown-like handling") — clean EOF follows.
    let note = notification_frame("shutdown".into(), json!({}));
    client.write.write_all(&note).await.unwrap();
    client.write.flush().await.unwrap();
    client.expect_eof().await;
    timeout(RX_TIMEOUT, task)
        .await
        .expect("server task hung")
        .unwrap()
        .expect("server clean exit");
}

#[tokio::test]
async fn pipelined_burst_across_sessions_is_deterministic_and_lossless() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // 20 pipelined prompts across 10 sessions: every turn must be
    // answered with its own echo, exactly once (per-session ordering holds
    // by construction; nothing may be dropped or duplicated).
    let mut burst = Vec::new();
    for i in 0..20u64 {
        let session = i % 10;
        burst.extend(frame(
            "session/prompt".to_string(),
            i + 1,
            prompt_params(&format!("sess-{session}"), &format!("turn-{i}")),
        ));
    }
    client.write.write_all(&burst).await.unwrap();
    client.write.flush().await.unwrap();

    let mut echoes = Vec::new();
    for _ in 0..20u64 {
        let msg = client.expect_message().await;
        assert_eq!(msg["result"]["stopReason"], "end_turn");
        echoes.push(msg["result"]["_meta"]["echo"].as_str().unwrap().to_string());
    }
    echoes.sort();
    let mut expected: Vec<String> = (0..20).map(|i| format!("turn-{i}")).collect();
    expected.sort();
    assert_eq!(echoes, expected);
}

#[tokio::test]
async fn prompt_params_missing_or_wrong_type_is_32602() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // Params entirely absent.
    assert_eq!(
        client.error_code_of("session/prompt", json!({})).await,
        -32602
    );
    // sessionId missing.
    assert_eq!(
        client
            .error_code_of(
                "session/prompt",
                json!({ "prompt": [{ "type": "text", "text": "hi" }] })
            )
            .await,
        -32602
    );
    // prompt missing.
    assert_eq!(
        client
            .error_code_of("session/prompt", json!({ "sessionId": "s" }))
            .await,
        -32602
    );
    // Wrong types.
    assert_eq!(
        client
            .error_code_of(
                "session/prompt",
                json!({ "sessionId": 7, "prompt": [{ "type": "text", "text": "hi" }] })
            )
            .await,
        -32602
    );
    assert_eq!(
        client
            .error_code_of(
                "session/prompt",
                json!({ "sessionId": "s", "prompt": "hi" })
            )
            .await,
        -32602
    );
    // Content blocks this agent cannot honestly represent are refused.
    assert_eq!(
        client
            .error_code_of(
                "session/prompt",
                json!({ "sessionId": "s", "prompt": [{ "type": "image", "data": "x" }] })
            )
            .await,
        -32602
    );
    // Non-object params at all.
    let msg = client.request("session/prompt", json!([1, 2, 3])).await;
    assert_eq!(msg["error"]["code"], -32602);
    // Cancel needs a sessionId too.
    assert_eq!(
        client.error_code_of("session/cancel", json!({})).await,
        -32602
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_backend_same_session_prompts_stay_ordered() {
    #[derive(Clone)]
    struct SlowBackend {
        inner: std::sync::Arc<EchoBackend>,
        delay: Duration,
    }
    impl AcpBackend for SlowBackend {
        fn agent_info(&self) -> Value {
            self.inner.agent_info()
        }
        fn create_session(&self, params: &Value) -> Result<String, String> {
            self.inner.create_session(params)
        }
        fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
            std::thread::sleep(self.delay);
            self.inner.prompt(session_id, text)
        }
        fn abort(&self, session_id: &str) -> Result<(), String> {
            self.inner.abort(session_id)
        }
        fn list_sessions(&self) -> Vec<String> {
            self.inner.list_sessions()
        }
    }
    let slow = SlowBackend {
        inner: std::sync::Arc::new(EchoBackend::new()),
        delay: Duration::from_millis(30),
    };
    let (mut client, _task) = start_server(slow);

    // Two pipelined prompts to the SAME session: the second queues behind
    // the first (FIFO) and its terminal follows the first's.
    let mut burst = Vec::new();
    for i in 0..2u64 {
        burst.extend(frame(
            "session/prompt".to_string(),
            i + 1,
            prompt_params("s", &format!("slow-{i}")),
        ));
    }
    let started = std::time::Instant::now();
    client.write.write_all(&burst).await.unwrap();
    client.write.flush().await.unwrap();
    for i in 0..2u64 {
        let msg = client.expect_message().await;
        assert_eq!(msg["id"], i + 1);
        assert_eq!(msg["result"]["_meta"]["echo"], format!("slow-{i}"));
    }
    // 2 × 30 ms sleeps ran serially on the session's operation task: wall
    // time proves per-session ordering was preserved.
    assert!(started.elapsed() >= Duration::from_millis(55));
}

#[tokio::test]
async fn hostile_utf8_nul_and_control_bytes_round_trip_via_json() {
    let (mut client, _task) = start_server(EchoBackend::new());

    let evil = "h\u{e9}llo\u{0}wor\r\nld\u{7f}\u{1}\u{b}\t\u{2028}\u{2029}\u{ffff}💥\u{10ffff}"
        .to_string();
    let msg = client
        .request(
            "session/prompt",
            json!({
                "sessionId": "sess-\u{0}",
                "prompt": [{ "type": "text", "text": evil }],
            }),
        )
        .await;
    assert_eq!(msg["error"].as_object(), None, "unexpected error: {msg}");
    assert_eq!(msg["result"]["stopReason"], "end_turn");
    assert_eq!(msg["result"]["_meta"]["echo"].as_str().unwrap(), evil);

    // Deeply hostile but valid JSON: extreme nesting is bounded by the
    // parser (no stack overflow, no panic).
    let mut evil2 = String::from("turn");
    for _ in 0..1000 {
        evil2.push_str("\u{0}\u{1}\u{2}");
    }
    let msg = client
        .request("session/prompt", prompt_params("s", &evil2))
        .await;
    assert_eq!(msg["result"]["_meta"]["echo"].as_str().unwrap(), evil2);
}

#[tokio::test]
async fn oversized_request_params_refused_with_32600() {
    let (mut client, _task) = start_server(EchoBackend::new());

    let huge_text = "z".repeat(1024 * 1024 + 1000);
    let msg = client
        .request(
            "session/prompt",
            json!({
                "sessionId": "s",
                "prompt": [{ "type": "text", "text": huge_text }],
            }),
        )
        .await;
    assert_eq!(msg["error"]["code"], -32600);
    let message = msg["error"]["message"].as_str().unwrap();
    assert!(message.contains("1 MiB"), "{message}");
    // Loop still alive.
    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "faktor-test-agent");
}

#[tokio::test]
async fn oversized_backend_result_refused_not_truncated() {
    let (mut client, task) = start_server(HugeBackend);

    let msg = client
        .request("session/prompt", prompt_params("sess-huge", "blob"))
        .await;
    assert_eq!(msg["error"]["code"], -32603);
    let data = msg["error"]["data"].as_str().unwrap();
    assert!(data.contains("8 MiB"), "{data}");
    // Nothing half-written: the next request still parses cleanly.
    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "huge");

    let bye = client.request("shutdown", json!({})).await;
    assert_eq!(bye["result"], json!({ "ok": true }));
    client.expect_eof().await;
    timeout(RX_TIMEOUT, task)
        .await
        .expect("server task hung")
        .unwrap()
        .expect("server clean exit");
}

#[tokio::test]
async fn invalid_request_shapes_get_32600() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // Not an object at all.
    for body in [
        b"42".as_slice(),
        b"\"str\"".as_slice(),
        b"[1,2,3]".as_slice(),
    ] {
        let bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        let mut framed = bytes;
        framed.extend_from_slice(body);
        client.write.write_all(&framed).await.unwrap();
        client.write.flush().await.unwrap();
        let msg = client.expect_message().await;
        assert_eq!(msg["error"]["code"], -32600, "{msg}");
    }

    // Missing method / wrong jsonrpc.
    let msg = client.request("bogus", json!({})).await;
    assert_eq!(msg["error"]["code"], -32601, "{msg}");
    let bad_version = json!({ "jsonrpc": "1.0", "id": 5, "method": "agent_info" });
    let framed = faktor_acp::protocol::encode(&bad_version).unwrap();
    client.write.write_all(&framed).await.unwrap();
    client.write.flush().await.unwrap();
    let msg = client.expect_message().await;
    assert_eq!(msg["error"]["code"], -32600);

    // Negative and fractional ids are invalid requests.
    for id in [json!(-1), json!(1.5)] {
        let bad = json!({ "jsonrpc": "2.0", "id": id, "method": "agent_info" });
        let framed = faktor_acp::protocol::encode(&bad).unwrap();
        client.write.write_all(&framed).await.unwrap();
        client.write.flush().await.unwrap();
        let msg = client.expect_message().await;
        assert_eq!(msg["error"]["code"], -32600, "{msg}");
    }

    // String ids are valid JSON-RPC 2.0 and are echoed verbatim (the
    // official SDK allocates UUID strings). Conformance gap fix: the server
    // used to refuse every string id with -32600 and a null id, which hung
    // the official client.
    let string_id = json!({ "jsonrpc": "2.0", "id": "e70f649f-bb05-42b2-9b08-380299012ea8", "method": "agent_info" });
    let framed = faktor_acp::protocol::encode(&string_id).unwrap();
    client.write.write_all(&framed).await.unwrap();
    client.write.flush().await.unwrap();
    let msg = client.expect_message().await;
    assert_eq!(
        msg["id"], "e70f649f-bb05-42b2-9b08-380299012ea8",
        "string request id must be echoed: {msg}"
    );
    assert_eq!(msg["result"]["name"], "faktor-test-agent");

    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "faktor-test-agent");
}

#[tokio::test]
async fn cancel_is_an_ack_and_session_abort_alias_keeps_sessions_usable() {
    let (mut client, _task) = start_server(EchoBackend::new());
    let new = client.request("session/new", json!({})).await;
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    // Cancel with no turn running: uniform empty-result ack (official
    // notification semantics; no error, no state corruption).
    let ok = client
        .request("session/cancel", json!({ "sessionId": sid }))
        .await;
    assert_eq!(ok["result"], json!({}));

    // The deprecated session/abort alias behaves identically.
    let ok = client
        .request("session/abort", json!({ "sessionId": sid }))
        .await;
    assert_eq!(ok["result"], json!({}));

    // Cancel on an unknown session: same clean ack.
    let ok = client
        .request("session/cancel", json!({ "sessionId": "does-not-exist" }))
        .await;
    assert_eq!(ok["result"], json!({}));

    // The session remains listed and usable.
    let list = client.request("session/list", json!({})).await;
    assert_eq!(list["result"]["sessions"][0]["sessionId"], sid);
    let p = client
        .request("session/prompt", prompt_params(&sid, "still works"))
        .await;
    assert_eq!(p["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn empty_and_whitespace_frames_do_not_hang_or_crash() {
    let (mut client, _task) = start_server(EchoBackend::new());

    // Frame of length 0: JSON parse error (consumed, loop survives).
    client
        .write
        .write_all(b"Content-Length: 0\r\n\r\n")
        .await
        .unwrap();
    client.write.flush().await.unwrap();
    let msg = client.expect_message().await;
    assert!(msg["id"].is_null());
    assert_eq!(msg["error"]["code"], -32700);

    // Whitespace-only body.
    let ws = b"Content-Length: 3\r\n\r\n   ";
    client.write.write_all(ws).await.unwrap();
    client.write.flush().await.unwrap();
    let msg = client.expect_message().await;
    assert_eq!(msg["error"]["code"], -32700);

    let ok = client.request("agent_info", json!({})).await;
    assert_eq!(ok["result"]["name"], "faktor-test-agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_reaches_a_running_turn_and_subsequent_prompts_work() {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;

    // A backend whose prompt parks until released and whose abort records.
    #[derive(Clone)]
    struct ParkBackend {
        started: Arc<AtomicBool>,
        aborted: Arc<AtomicUsize>,
        release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
        tx: std::sync::mpsc::Sender<()>,
    }
    impl ParkBackend {
        fn new() -> Self {
            let (tx, rx) = std::sync::mpsc::channel();
            Self {
                started: Arc::new(AtomicBool::new(false)),
                aborted: Arc::new(AtomicUsize::new(0)),
                release: Arc::new(Mutex::new(Some(rx))),
                tx,
            }
        }
    }
    impl AcpBackend for ParkBackend {
        fn agent_info(&self) -> Value {
            json!({ "name": "park", "version": "0.0.0" })
        }
        fn create_session(&self, _p: &Value) -> Result<String, String> {
            Ok("sess-park".into())
        }
        fn list_sessions(&self) -> Vec<String> {
            vec!["sess-park".into()]
        }
        fn prompt(&self, _sid: &str, text: &str) -> Result<Value, String> {
            self.started.store(true, Ordering::SeqCst);
            if text == "park" {
                let rx = self.release.lock().unwrap().take().unwrap();
                let _ = rx.recv();
            }
            Ok(json!({ "echo": text }))
        }
        fn abort(&self, _sid: &str) -> Result<(), String> {
            self.aborted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let backend = ParkBackend::new();
    let (mut client, _task) = start_server(backend.clone());

    // session/prompt parks inside the backend: a RUNNING prompt.
    let park_id = 41u64;
    let bytes = frame(
        "session/prompt".to_string(),
        park_id,
        prompt_params("sess-park", "park"),
    );
    client.write.write_all(&bytes).await.unwrap();
    client.write.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + RX_TIMEOUT;
    while !backend.started.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "park prompt never started"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // session/cancel reaches the running prompt (abort hook fired).
    let cancel_id = 42u64;
    let bytes = frame(
        "session/cancel".to_string(),
        cancel_id,
        json!({ "sessionId": "sess-park" }),
    );
    client.write.write_all(&bytes).await.unwrap();
    client.write.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + RX_TIMEOUT;
    while backend.aborted.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "abort hook never fired"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Release the parked prompt: the turn was cancelled, so its terminal
    // is the official cancelled state, not end_turn.
    backend.tx.send(()).unwrap();
    loop {
        let msg = client.expect_message().await;
        if msg["id"] == json!(cancel_id) {
            assert_eq!(msg["result"], json!({}));
        } else if msg["id"] == json!(park_id) {
            assert_eq!(msg["result"]["stopReason"], "cancelled");
            assert!(
                msg["result"].get("_meta").is_none(),
                "no _meta for cancelled"
            );
            break;
        }
    }

    // No dead session: the next prompt completes normally.
    let p = client
        .request("session/prompt", prompt_params("sess-park", "again"))
        .await;
    assert_eq!(p["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn client_eof_is_a_clean_exit_without_shutdown() {
    let (client, task) = start_server(EchoBackend::new());
    // Dropping the client closes the pipe; the server must exit Ok, not
    // hang or error.
    drop(client);
    let result = timeout(RX_TIMEOUT, task)
        .await
        .expect("server stuck after EOF")
        .expect("server task panicked");
    assert!(result.is_ok(), "server errored on client EOF: {result:?}");
}

// ---------------------------------------------------------------------------
// Wave: advertised-surface mapping over the native services
// ---------------------------------------------------------------------------

/// Sync backend with a bounded native v756 history for one owned session.
#[derive(Clone)]
struct HistoryBackend {
    session: String,
}

impl HistoryBackend {
    fn new(session: &str) -> Self {
        Self {
            session: session.to_string(),
        }
    }
}

fn native_history_page(session: &str) -> MessagesPage {
    MessagesPage {
        session_id: session.to_string(),
        // Native pages are newest-first.
        messages: vec![
            NativeMessage {
                id: "2".into(),
                role: "assistant".into(),
                session_id: session.to_string(),
                seq: 2,
                created_ms: 0,
                parts: vec![
                    NativePart::ToolCall {
                        tool_call_id: "call-1".into(),
                        name: "echo".into(),
                        input: json!({ "x": 1 }),
                        state: "running".into(),
                    },
                    NativePart::Text {
                        text: "done".into(),
                    },
                ],
            },
            NativeMessage {
                id: "1".into(),
                role: "user".into(),
                session_id: session.to_string(),
                seq: 1,
                created_ms: 0,
                parts: vec![
                    NativePart::Text {
                        text: "hello".into(),
                    },
                    NativePart::Reasoning {
                        text: "thinking".into(),
                    },
                    NativePart::Summary {
                        text: "summarized".into(),
                    },
                    NativePart::ToolResult {
                        tool_call_id: "call-1".into(),
                        result: ToolResultBody {
                            excerpt: "fine".into(),
                            exit_code: Some(0),
                            artifact: None,
                            slice_hint: None,
                        },
                    },
                ],
            },
        ],
        has_more: false,
        next_before: None,
        page: PageMeta::default(),
    }
}

impl AcpBackend for HistoryBackend {
    fn agent_info(&self) -> Value {
        json!({ "name": "history", "version": "0.0.0" })
    }
    fn create_session(&self, _params: &Value) -> Result<String, String> {
        Ok(self.session.clone())
    }
    fn prompt(&self, _session_id: &str, text: &str) -> Result<Value, String> {
        Ok(json!({ "echo": text }))
    }
    fn abort(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn list_sessions(&self) -> Vec<String> {
        vec![self.session.clone()]
    }
    fn capabilities(&self) -> faktor_acp::BackendCapabilities {
        faktor_acp::BackendCapabilities {
            load_session: true,
            mcp_http: true,
            mcp_sse: false,
        }
    }
    fn load_session(&self, session_id: &str) -> Result<MessagesPage, faktor_acp::LoadSessionError> {
        match session_id {
            "sess-1" => Ok(native_history_page("sess-1")),
            "incomplete" => {
                let mut page = native_history_page("incomplete");
                page.has_more = true;
                page.next_before = Some(1);
                page.page.has_more = true;
                page.page.cursor = Some(1);
                Ok(page)
            }
            "mismatch" => Ok(native_history_page("some-other-session")),
            "huge" => {
                let mut page = native_history_page("huge");
                page.messages = (0..=faktor_acp::MAX_LOAD_MESSAGES)
                    .map(|seq| NativeMessage {
                        id: seq.to_string(),
                        role: "assistant".into(),
                        session_id: "huge".into(),
                        seq: seq as i64,
                        created_ms: 0,
                        parts: vec![NativePart::Text {
                            text: format!("m{seq}"),
                        }],
                    })
                    .collect();
                Ok(page)
            }
            "manyframes" => {
                let mut page = native_history_page("manyframes");
                let parts: Vec<NativePart> = (0..5)
                    .map(|i| NativePart::Text {
                        text: format!("p{i}"),
                    })
                    .collect();
                page.messages = (0..4096)
                    .map(|seq| NativeMessage {
                        id: seq.to_string(),
                        role: "assistant".into(),
                        session_id: "manyframes".into(),
                        seq: seq as i64,
                        created_ms: 0,
                        parts: parts.clone(),
                    })
                    .collect();
                Ok(page)
            }
            _ => Err(faktor_acp::LoadSessionError::NotFound),
        }
    }
}

/// Streaming backend whose permission flow is resolved by the ACP client
/// through the REAL native `ChannelPermissionRequester` (durable pending
/// table, first-decision-wins, cleanup).
#[derive(Clone)]
struct NativePermissionBackend {
    requester: Arc<ChannelPermissionRequester>,
    next_id: Arc<Mutex<i64>>,
    decisions: Arc<Mutex<Vec<String>>>,
}

impl NativePermissionBackend {
    fn new() -> Self {
        Self {
            requester: ChannelPermissionRequester::new(Duration::from_secs(5)),
            next_id: Arc::new(Mutex::new(1)),
            decisions: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AcpStreamBackend for NativePermissionBackend {
    fn agent_info(&self) -> Value {
        json!({ "name": "native-permissions", "version": "0.0.0" })
    }
    fn create_session(&self, _params: &Value) -> Result<String, String> {
        Ok("sess-1".into())
    }
    fn list_sessions(&self) -> Vec<String> {
        vec!["sess-1".into()]
    }
    fn prompt<'a>(
        &'a self,
        _session_id: &'a str,
        ctx: &'a PromptCtx,
        _text: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let id = {
                let mut next = self.next_id.lock().unwrap();
                let id = *next;
                *next += 1;
                id
            };
            let requester = self.requester.clone();
            let decisions = self.decisions.clone();
            let permission = PermissionRequest {
                id,
                op_id: OpId::new(1),
                capability: Capability::ExecuteShell {
                    command: "echo hi".into(),
                },
                event_seq: EventSeq::new(1),
            };
            let tool_call = json!({ "toolCallId": "call-1", "title": "echo" });
            let permission_id = permission.id;
            let client = ctx.client();
            let acp = async move {
                let outcome = client
                    .request_permission(
                        &tool_call,
                        &[
                            PermissionOption::allow_once(),
                            PermissionOption::reject_once(),
                        ],
                    )
                    .await;
                let decision = match &outcome {
                    Ok(outcome) if outcome.selected_option_id() == Some("allow_once") => {
                        PermissionDecision::Allow
                    }
                    _ => PermissionDecision::Deny,
                };
                let outcome_value = match &outcome {
                    Ok(outcome) => outcome
                        .selected_option_id()
                        .map(str::to_string)
                        .unwrap_or_else(|| "cancelled".to_string()),
                    Err(error) => format!("error:{error}"),
                };
                // The same decision path the daemon adapter uses: resolve
                // the durable native request exactly once.
                requester.resolve(permission_id, decision);
                decisions.lock().unwrap().push(format!("{decision:?}"));
                outcome_value
            };
            let native = self
                .requester
                .clone()
                .request(SessionId::new(7), &permission);
            let (decision, outcome) = tokio::join!(native, acp);
            Ok(json!({
                "decision": format!("{:?}", decision.expect("native decision")),
                "outcome": outcome,
            }))
        })
    }
}

/// Streaming backend that reads one client file through the negotiated
/// client fs capability, or reports the typed refusal.
#[derive(Clone)]
struct FsBackend;

impl AcpStreamBackend for FsBackend {
    fn agent_info(&self) -> Value {
        json!({ "name": "fs", "version": "0.0.0" })
    }
    fn create_session(&self, _params: &Value) -> Result<String, String> {
        Ok("sess-1".into())
    }
    fn list_sessions(&self) -> Vec<String> {
        vec!["sess-1".into()]
    }
    fn prompt<'a>(
        &'a self,
        _session_id: &'a str,
        ctx: &'a PromptCtx,
        _text: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            match ctx
                .client()
                .read_text_file("/work/notes.txt", None, None)
                .await
            {
                Ok(content) => {
                    ctx.emit_text(&content)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(json!({ "read": content }))
                }
                Err(error) => Ok(json!({ "error": error.to_string() })),
            }
        })
    }
}

const INITIALIZE_EXTENSIONS_CAPABLE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"extensions":["faktor.agentStateChanged","unknown.extension"],"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":false}}}}"#;

const INITIALIZE_RESPONSE_CAPABLE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"loadSession":true,"mcpCapabilities":{"http":true,"sse":false},"promptCapabilities":{"audio":false,"embeddedContext":false,"image":false}},"authMethods":[],"extensions":["faktor.agentStateChanged"],"protocolVersion":1}}"#;

#[tokio::test]
async fn load_capability_advertisement_is_honest_and_byte_canonical() {
    let (mut client, _task) = start_server(HistoryBackend::new("sess-1"));
    let request: Value = serde_json::from_str(INITIALIZE_EXTENSIONS_CAPABLE).unwrap();
    client.send_value(&request).await;
    let frame = client.expect_message().await;
    assert_canonical(&frame, INITIALIZE_RESPONSE_CAPABLE);
    assert_eq!(frame["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        frame["result"]["agentCapabilities"]["mcpCapabilities"]["http"],
        true
    );
    assert_eq!(
        frame["result"]["extensions"],
        json!(["faktor.agentStateChanged"])
    );

    // The default backend advertises nothing it cannot do.
    let (mut plain, _task2) = start_server(EchoBackend::new());
    let init = plain
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], false);
    assert!(init["result"]["agentCapabilities"]
        .get("mcpCapabilities")
        .is_none());
    assert_eq!(init["result"]["authMethods"], json!([]));
}

#[tokio::test]
async fn session_load_replays_native_history_chronologically_and_bounded() {
    let (mut client, _task) = start_server(HistoryBackend::new("sess-1"));
    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.agentStateChanged"] }),
        )
        .await;
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);

    let load = frame(
        "session/load".to_string(),
        4,
        json!({ "sessionId": "sess-1", "cwd": "/work", "mcpServers": [] }),
    );
    client.write.write_all(&load).await.unwrap();
    client.write.flush().await.unwrap();

    let expected = vec![
        user_message_chunk_update("hello"),
        agent_thought_chunk_update("thinking"),
        agent_thought_chunk_update("summarized"),
        tool_result_from_native("call-1", "fine", Some(0), None, None),
        tool_call_from_native("call-1", "echo", &json!({ "x": 1 }), "running"),
        text_chunk_update("done"),
    ];
    let mut seen = Vec::new();
    loop {
        let msg = client.expect_message().await;
        if msg.get("method").is_some() {
            assert_eq!(msg["method"], "session/update");
            assert_eq!(msg["params"]["sessionId"], "sess-1");
            seen.push(msg["params"]["update"].clone());
            continue;
        }
        assert_eq!(msg["id"], 4);
        assert_eq!(
            msg["result"],
            json!({}),
            "session/load answers the empty result"
        );
        assert_canonical(&msg, r#"{"jsonrpc":"2.0","id":4,"result":{}}"#);
        break;
    }
    assert_eq!(
        seen, expected,
        "replay order must be chronological and part-ordered"
    );

    // Every replayed frame is the canonical serialization of its builder;
    // notifications omit `id` entirely (official SDK classification).
    for (update, expected) in seen.iter().zip(expected.iter()) {
        let actual = faktor_acp::protocol::encode(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": session_update_params("sess-1", update.clone()),
        }))
        .unwrap();
        let canonical = faktor_acp::protocol::encode(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": session_update_params("sess-1", expected.clone()),
        }))
        .unwrap();
        assert_eq!(actual, canonical);
    }
}

#[tokio::test]
async fn session_load_refusals_are_official_and_never_silent() {
    let (mut client, _task) = start_server(HistoryBackend::new("sess-1"));
    client
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;

    // Foreign session: official invalid params, never an empty replay.
    let msg = client
        .request(
            "session/load",
            json!({ "sessionId": "foreign", "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    assert_canonical(
        &msg,
        r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"unknown session \"foreign\""}}"#,
    );

    // Incomplete bounded window: internal error naming the bound.
    let msg = client
        .request(
            "session/load",
            json!({ "sessionId": "incomplete", "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    assert_canonical(
        &msg,
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"data":"session history exceeds the bounded load window (older messages exist)","message":"Internal error"}}"#,
    );

    // Missing sessionId.
    let msg = client.request("session/load", json!({})).await;
    assert_eq!(msg["error"]["code"], -32602);

    // A backend that answers for a different session is refused loudly:
    // foreign history must never be replayed under this session id.
    let msg = client
        .request("session/load", json!({ "sessionId": "mismatch" }))
        .await;
    assert_eq!(msg["error"]["code"], -32603);
    assert!(
        msg["error"]["data"]
            .as_str()
            .unwrap()
            .contains("not the requested"),
        "{msg}"
    );

    // Message-count bound: the whole page is refused, never truncated.
    let msg = client
        .request("session/load", json!({ "sessionId": "huge" }))
        .await;
    assert_eq!(msg["error"]["code"], -32603);
    assert!(
        msg["error"]["data"]
            .as_str()
            .unwrap()
            .contains("message load bound"),
        "{msg}"
    );

    // Frame-count bound: more parts than the bounded replay window.
    let msg = client
        .request("session/load", json!({ "sessionId": "manyframes" }))
        .await;
    assert_eq!(msg["error"]["code"], -32603);
    assert!(
        msg["error"]["data"]
            .as_str()
            .unwrap()
            .contains("frame load bound"),
        "{msg}"
    );

    // Capability absent: official method-not-found, never a silent replay.
    let (mut plain, _task2) = start_server(EchoBackend::new());
    plain
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    let msg = plain
        .request("session/load", json!({ "sessionId": "sess-0" }))
        .await;
    assert_eq!(msg["error"]["code"], -32601);

    // MCP servers refused while unsupported.
    let msg = plain
        .request(
            "session/new",
            json!({ "mcpServers": [{ "name": "m", "command": "run" }] }),
        )
        .await;
    assert_canonical(
        &msg,
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"this agent does not support MCP servers"}}"#,
    );
}

#[tokio::test]
async fn permission_allow_deny_round_trip_through_native_requester() {
    let backend = NativePermissionBackend::new();
    let requester = backend.requester.clone();
    let decisions = backend.decisions.clone();
    let (mut client, _task) = start_streaming_server_with_config(backend, AcpConfig::default());

    client
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;

    // Turn 1: allow.
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            2,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "gate" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (request_id, params) = client
        .recv_server_request("session/request_permission", &mut seen)
        .await;
    assert_eq!(
        request_id, 1,
        "first server request id on a fresh connection"
    );
    assert_canonical(
        &json!({ "jsonrpc": "2.0", "id": request_id, "method": "session/request_permission", "params": params }),
        r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"call-1","title":"echo"},"options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"},{"optionId":"reject_once","name":"Reject once","kind":"reject_once"}]}}"#,
    );
    assert_eq!(
        requester.pending_ids(),
        vec![1],
        "durable native pending row"
    );
    client
        .respond(
            request_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        )
        .await;
    let terminal = client.recv_until(|msg| msg["id"] == json!(2)).await;
    assert_eq!(terminal["result"]["stopReason"], "end_turn");
    assert_eq!(terminal["result"]["_meta"]["decision"], "Allow");
    assert_eq!(terminal["result"]["_meta"]["outcome"], "allow_once");
    assert!(requester.pending_ids().is_empty(), "native row cleaned up");

    // Turn 2: deny.
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            4,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "gate" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (request_id, _params) = client
        .recv_server_request("session/request_permission", &mut seen)
        .await;
    assert_eq!(
        request_id, 2,
        "server request ids increment deterministically"
    );
    client
        .respond(
            request_id,
            json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }),
        )
        .await;
    let terminal = client.recv_until(|msg| msg["id"] == json!(4)).await;
    assert_eq!(terminal["result"]["_meta"]["decision"], "Deny");
    assert_eq!(terminal["result"]["_meta"]["outcome"], "reject_once");
    assert!(requester.pending_ids().is_empty());
    assert_eq!(*decisions.lock().unwrap(), vec!["Allow", "Deny"]);
}

#[tokio::test]
async fn permission_cancelled_hostile_duplicate_and_timeout_keep_native_state_clean() {
    let backend = NativePermissionBackend::new();
    let requester = backend.requester.clone();
    let config = AcpConfig {
        client_request_timeout: Duration::from_millis(250),
        ..AcpConfig::default()
    };
    let (mut client, _task) = start_streaming_server_with_config(backend, config);
    client
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;

    // Cancelled outcome -> adapter denies; the native row is resolved once.
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            1,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "gate" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (id, _) = client
        .recv_server_request("session/request_permission", &mut seen)
        .await;
    client
        .respond(id, json!({ "outcome": { "outcome": "cancelled" } }))
        .await;
    let terminal = client.recv_until(|msg| msg["id"] == json!(1)).await;
    assert_eq!(terminal["result"]["_meta"]["decision"], "Deny");
    assert!(requester.pending_ids().is_empty());

    // Hostile option id -> malformed client response, mapped to deny.
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            3,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "gate" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (id, _) = client
        .recv_server_request("session/request_permission", &mut seen)
        .await;
    client
        .respond(
            id,
            json!({ "outcome": { "outcome": "selected", "optionId": "not-offered" } }),
        )
        .await;
    let terminal = client.recv_until(|msg| msg["id"] == json!(3)).await;
    assert_eq!(terminal["result"]["_meta"]["decision"], "Deny");
    assert!(requester.pending_ids().is_empty());

    // Duplicate client answer: the second response is for an id that is no
    // longer outstanding and must not overwrite the first decision.
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            5,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "gate" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (id, _) = client
        .recv_server_request("session/request_permission", &mut seen)
        .await;
    client
        .respond(
            id,
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        )
        .await;
    client
        .respond(
            id,
            json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }),
        )
        .await;
    let terminal = client.recv_until(|msg| msg["id"] == json!(5)).await;
    assert_eq!(
        terminal["result"]["_meta"]["decision"], "Allow",
        "first decision wins, duplicate responses are dropped"
    );
    assert!(requester.pending_ids().is_empty());

    // No client answer at all: bounded timeout, typed error, deny outcome.
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            7,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "gate" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (_id, _) = client
        .recv_server_request("session/request_permission", &mut seen)
        .await;
    let started = std::time::Instant::now();
    let terminal = client.recv_until(|msg| msg["id"] == json!(7)).await;
    assert_eq!(terminal["result"]["_meta"]["decision"], "Deny");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timeout must be bounded by the configured client_request_timeout"
    );
    assert!(requester.pending_ids().is_empty());

    // The connection remains usable after every hostile answer.
    let info = client.request("agent_info", json!({})).await;
    assert_eq!(info["result"]["name"], "native-permissions");
}

#[tokio::test]
async fn client_fs_is_gated_by_negotiated_capabilities() {
    // Negotiated: the official fs request is sent and answered.
    let (mut client, _task) = start_streaming_server_with_config(FsBackend, AcpConfig::default());
    client
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true } },
            }),
        )
        .await;
    client
        .send_bytes(&frame(
            "session/prompt".to_string(),
            2,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "read" }] }),
        ))
        .await;
    let mut seen = Vec::new();
    let (id, params) = client
        .recv_server_request("fs/read_text_file", &mut seen)
        .await;
    assert_eq!(id, 1);
    assert_canonical(
        &json!({ "jsonrpc": "2.0", "id": id, "method": "fs/read_text_file", "params": params }),
        r#"{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{"sessionId":"sess-1","path":"/work/notes.txt"}}"#,
    );
    client.respond(id, json!({ "content": "alpha" })).await;
    let mut saw_content = false;
    let terminal = loop {
        let msg = client.expect_message().await;
        if msg["id"] == json!(2) && msg.get("result").is_some() {
            break msg;
        }
        if msg["params"]["update"]["content"]["text"] == json!("alpha") {
            saw_content = true;
        }
    };
    assert_eq!(terminal["result"]["_meta"]["read"], "alpha");
    assert!(
        saw_content,
        "the read content must be emitted to the client as an official update"
    );

    // Not negotiated: typed refusal, and no fs frame ever hits the wire.
    let (mut strict, _task2) = start_streaming_server_with_config(FsBackend, AcpConfig::default());
    strict
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    strict
        .send_bytes(&frame(
            "session/prompt".to_string(),
            2,
            json!({ "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "read" }] }),
        ))
        .await;
    let mut frames = Vec::new();
    let terminal = loop {
        let msg = strict.expect_message().await;
        if msg["id"] == json!(2) {
            break msg;
        }
        frames.push(msg);
    };
    let error = terminal["result"]["_meta"]["error"].as_str().unwrap();
    assert!(error.contains("not negotiated"), "{error}");
    assert!(
        frames
            .iter()
            .all(|frame| frame.get("method") != Some(&json!("fs/read_text_file"))),
        "no unnegotiated fs request may be sent"
    );
}

#[tokio::test]
async fn mcp_authenticate_terminal_and_unknown_methods_never_silent() {
    let (mut client, _task) = start_server(EchoBackend::new());
    client
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;

    // authenticate: authMethods is empty and stays empty; the call is an
    // official typed refusal.
    let msg = client
        .request("authenticate", json!({ "methodId": "api-key" }))
        .await;
    assert_canonical(
        &msg,
        r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"data":{"methodId":"api-key"},"message":"no authentication methods are available"}}"#,
    );
    let msg = client.request("authenticate", json!({})).await;
    assert_eq!(msg["error"]["code"], -32602);
    assert!(msg["error"]["message"]
        .as_str()
        .unwrap()
        .contains("methodId"));

    // Terminal methods are unreachable here: the client declared no
    // `faktor.terminal` extension, so even the methods this crate
    // implements keep the official method-not-found (the client-side
    // `terminal/output`/`wait_for_exit`/`release` are never implemented).
    for method in [
        "terminal/create",
        "terminal/output",
        "terminal/wait_for_exit",
        "terminal/kill",
        "terminal/release",
    ] {
        assert_eq!(
            client
                .error_code_of(method, json!({ "sessionId": "sess-0" }))
                .await,
            -32601,
            "{method} must be an official method-not-found"
        );
    }
    // Server does not accept client-side fs methods either.
    assert_eq!(
        client
            .error_code_of(
                "fs/read_text_file",
                json!({ "sessionId": "sess-0", "path": "/x" })
            )
            .await,
        -32601
    );
    assert_eq!(
        client
            .error_code_of("session/set_mode", json!({ "sessionId": "sess-0" }))
            .await,
        -32601
    );

    // MCP with a declared capability is accepted (the backend owns it).
    let (mut mcp, _task2) = start_server(HistoryBackend::new("sess-1"));
    mcp.request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    let ok = mcp
        .request(
            "session/new",
            json!({ "mcpServers": [{ "name": "m", "command": "run" }] }),
        )
        .await;
    assert_eq!(ok["result"]["sessionId"], "sess-1");
}

#[tokio::test]
async fn non_text_prompt_blocks_refuse_typed_and_text_carries_structured_output() {
    let (mut client, _task) = start_server(EchoBackend::new());

    for block in [
        json!({ "type": "image", "data": "aGk=", "mimeType": "image/png" }),
        json!({ "type": "audio", "data": "aGk=", "mimeType": "audio/wav" }),
        json!({ "type": "resource_link", "uri": "file:///tmp/x", "name": "x" }),
        json!({ "type": "resource", "resource": { "uri": "file:///tmp/x", "text": "x" } }),
    ] {
        let msg = client
            .request(
                "session/prompt",
                json!({ "sessionId": "sess-0", "prompt": [block] }),
            )
            .await;
        assert_eq!(msg["error"]["code"], -32602, "{msg}");
        assert!(
            msg["error"]["message"]
                .as_str()
                .unwrap()
                .contains("text blocks only"),
            "{msg}"
        );
    }

    // The text-only path is the structured-output path: structured content
    // rides a text block verbatim and is never inspected or rewritten.
    let structured = r#"{"schema":"v1","items":[1,2,3],"nested":{"ok":true}}"#;
    let msg = client
        .request(
            "session/prompt",
            json!({ "sessionId": "sess-0", "prompt": [{ "type": "text", "text": structured }] }),
        )
        .await;
    assert_eq!(msg["result"]["stopReason"], "end_turn");
    assert_eq!(msg["result"]["_meta"]["echo"], structured);
}

// ---------------------------------------------------------------------------
// Negotiated `faktor.terminal` extension
// ---------------------------------------------------------------------------

/// In-memory terminal authority: real ownership semantics (session-scoped
/// rows, ownership ids, kill semantics) without spawning any OS process.
/// Used for the negotiation matrix, hostile-param and output-bound tests;
/// the real-PTY tests live behind `cfg(unix)` below.
#[derive(Clone, Default)]
struct FakeAuthority {
    inner: Arc<FakeAuthorityInner>,
}

#[derive(Default)]
struct FakeAuthorityInner {
    next: AtomicU64,
    creates: Mutex<Vec<TerminalSpec>>,
    handles: Mutex<HashMap<String, Arc<FakeHandle>>>,
}

struct FakeHandle {
    id: String,
    ownership: String,
    pid: u32,
    alive: AtomicBool,
    rows: AtomicU64,
    cols: AtomicU64,
    output: Mutex<Vec<u8>>,
    input: Mutex<Vec<u8>>,
    kills: AtomicU64,
}

impl FakeHandle {
    fn feed(&self, bytes: &[u8]) {
        self.output.lock().unwrap().extend_from_slice(bytes);
    }

    fn input(&self) -> Vec<u8> {
        self.input.lock().unwrap().clone()
    }

    fn size(&self) -> (u16, u16) {
        (
            self.rows.load(Ordering::SeqCst) as u16,
            self.cols.load(Ordering::SeqCst) as u16,
        )
    }

    fn kills(&self) -> u64 {
        self.kills.load(Ordering::SeqCst)
    }
}

impl TerminalHandle for FakeHandle {
    fn terminal_id(&self) -> &str {
        &self.id
    }
    fn pid(&self) -> u32 {
        self.pid
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn ownership_id(&self) -> &str {
        &self.ownership
    }
    fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.input.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        if rows == 0 || cols == 0 {
            return Err(TerminalError::Invalid("bad size".into()));
        }
        self.rows.store(rows as u64, Ordering::SeqCst);
        self.cols.store(cols as u64, Ordering::SeqCst);
        Ok(())
    }
    fn drain_output(&self) -> Vec<u8> {
        std::mem::take(&mut *self.output.lock().unwrap())
    }
    fn kill(&self) -> Result<(), TerminalError> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl FakeAuthority {
    fn new() -> Self {
        Self::default()
    }

    fn handle(&self, terminal_id: &str) -> Arc<FakeHandle> {
        self.inner
            .handles
            .lock()
            .unwrap()
            .get(terminal_id)
            .expect("fake handle exists")
            .clone()
    }

    fn create_count(&self) -> usize {
        self.inner.creates.lock().unwrap().len()
    }

    fn last_spec(&self) -> TerminalSpec {
        self.inner
            .creates
            .lock()
            .unwrap()
            .last()
            .expect("at least one create")
            .clone()
    }
}

impl TerminalAuthority for FakeAuthority {
    fn create(
        &self,
        _session_id: &str,
        spec: &TerminalSpec,
    ) -> Result<Arc<dyn TerminalHandle>, TerminalError> {
        match spec.command.as_str() {
            "refuse-spawn" => return Err(TerminalError::Refused("spawn refused".into())),
            "invalid-spec" => return Err(TerminalError::Invalid("bad terminal spec".into())),
            _ => {}
        }
        let n = self.inner.next.fetch_add(1, Ordering::SeqCst);
        let handle = Arc::new(FakeHandle {
            id: format!("t-{n}"),
            ownership: format!("own-{n}"),
            pid: 9000 + n as u32,
            alive: AtomicBool::new(true),
            rows: AtomicU64::new(spec.rows as u64),
            cols: AtomicU64::new(spec.cols as u64),
            output: Mutex::new(Vec::new()),
            input: Mutex::new(Vec::new()),
            kills: AtomicU64::new(0),
        });
        self.inner.creates.lock().unwrap().push(spec.clone());
        self.inner
            .handles
            .lock()
            .unwrap()
            .insert(handle.id.clone(), handle.clone());
        Ok(handle)
    }
}

fn start_server_with_terminals<B: AcpBackend + 'static>(
    backend: B,
    authority: Arc<dyn TerminalAuthority>,
) -> (WireClient, tokio::task::JoinHandle<Result<(), String>>) {
    start_server_with_terminals_and_config(backend, authority, AcpConfig::default())
}

fn start_server_with_terminals_and_config<B: AcpBackend + 'static>(
    backend: B,
    authority: Arc<dyn TerminalAuthority>,
    config: AcpConfig,
) -> (WireClient, tokio::task::JoinHandle<Result<(), String>>) {
    let (server_side, client_side) = duplex(4 * 1024 * 1024);
    let (server_r, server_w) = tokio::io::split(server_side);
    let (client_r, client_w) = tokio::io::split(client_side);
    let server = AcpServer::new(backend)
        .with_config(config)
        .with_terminal_authority(authority);
    let handle = tokio::spawn(async move { server.serve_connection(server_r, server_w).await });
    let client = WireClient {
        read: client_r,
        write: client_w,
        recv_buf: Vec::new(),
        next_id: 1,
    };
    (client, handle)
}

async fn new_session(client: &mut WireClient) -> String {
    let msg = client.request("session/new", json!({})).await;
    msg["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

fn terminal_updates(frames: &[Value], terminal_id: &str) -> Vec<Value> {
    frames
        .iter()
        .filter(|frame| {
            frame.get("method").and_then(Value::as_str) == Some("session/update")
                && frame["params"]["update"]["kind"] == "terminalOutput"
                && frame["params"]["update"]["terminalId"] == terminal_id
        })
        .map(|frame| frame["params"]["update"].clone())
        .collect()
}

/// Collect terminal output frames for `terminal_id` until `pred` matches
/// one of their `data` strings (returns the matching update).
async fn recv_terminal_output_until(
    client: &mut WireClient,
    terminal_id: &str,
    what: &str,
    mut pred: impl FnMut(&str) -> bool,
) -> Value {
    for _ in 0..20_000 {
        let msg = client.expect_message().await;
        if msg.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let update = &msg["params"]["update"];
        if update["kind"] != "terminalOutput" || update["terminalId"] != terminal_id {
            continue;
        }
        if pred(update["data"].as_str().unwrap_or_default()) {
            return update.clone();
        }
    }
    panic!("timed out waiting for terminal output {what}");
}

fn terminal_probes(session: &str) -> Vec<(&'static str, Value)> {
    vec![
        (
            "terminal/create",
            json!({ "sessionId": session, "command": "sh" }),
        ),
        (
            "terminal/input",
            json!({ "sessionId": session, "terminalId": "t-0", "data": "x" }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": session, "terminalId": "t-0", "rows": 1, "cols": 1 }),
        ),
        (
            "terminal/kill",
            json!({ "sessionId": session, "terminalId": "t-0" }),
        ),
        (
            "terminal/close",
            json!({ "sessionId": session, "terminalId": "t-0" }),
        ),
        ("terminal/list", json!({ "sessionId": session })),
    ]
}

#[tokio::test]
async fn terminal_negotiation_matrix_declared_vs_not() {
    let authority = Arc::new(FakeAuthority::new());

    // (a) Authority attached + client declares: granted, and every method is
    // reachable.
    let (mut client, _task) = start_server_with_terminals(
        EchoBackend::new(),
        authority.clone() as Arc<dyn TerminalAuthority>,
    );
    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    assert_eq!(init["result"]["extensions"], json!(["faktor.terminal"]));
    let sid = new_session(&mut client).await;
    let created = client
        .request(
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "sh",
                "args": ["-c", "echo hi"],
                "cwd": "/",
                "env": ["PATH"],
                "rows": 30,
                "cols": 100,
            }),
        )
        .await;
    let tid = created["result"]["terminalId"]
        .as_str()
        .expect("terminalId")
        .to_string();
    assert_eq!(created["result"]["sessionId"], sid);
    assert!(created["result"]["pid"].as_u64().unwrap() > 0);
    assert_eq!(created["result"]["ownershipId"], "own-0");
    // The authority received the allowlist NAMES and the geometry; the wire
    // never carried environment values.
    let spec = authority.last_spec();
    assert_eq!(spec.env, vec!["PATH".to_string()]);
    assert_eq!((spec.rows, spec.cols), (30, 100));
    assert_eq!(spec.cwd.as_deref(), Some("/"));
    assert_eq!(authority.create_count(), 1);

    // Resize and kill reach the authority; the row reflects both.
    let msg = client
        .request(
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 33, "cols": 121 }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    assert_eq!(authority.handle(&tid).size(), (33, 121));
    let msg = client
        .request(
            "terminal/kill",
            json!({ "sessionId": sid, "terminalId": tid }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    assert_eq!(authority.handle(&tid).kills(), 1);
    let list = client
        .request("terminal/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(list["result"]["terminals"][0]["alive"], false);

    // Close releases the row; further operations are typed refusals.
    let msg = client
        .request(
            "terminal/close",
            json!({ "sessionId": sid, "terminalId": tid }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    let list = client
        .request("terminal/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(list["result"]["terminals"], json!([]));
    let msg = client
        .request(
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 1, "cols": 1 }),
        )
        .await;
    assert_eq!(msg["error"]["code"], -32602);

    // The official client-side terminal methods are NOT part of this
    // extension's table: even a negotiated client gets -32601 for them.
    for method in [
        "terminal/output",
        "terminal/wait_for_exit",
        "terminal/release",
    ] {
        assert_eq!(
            client
                .error_code_of(method, json!({ "sessionId": sid, "terminalId": "t-0" }))
                .await,
            -32601,
            "{method} is not implemented and must stay method-not-found"
        );
    }

    // (b) Authority attached + client does not declare: every terminal
    // method keeps the official -32601, and the extension is not echoed.
    let (mut strict, _task2) = start_server_with_terminals(
        EchoBackend::new(),
        authority.clone() as Arc<dyn TerminalAuthority>,
    );
    let init = strict
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    assert!(init["result"].get("extensions").is_none());
    let sid = new_session(&mut strict).await;
    for (method, params) in terminal_probes(&sid) {
        assert_eq!(
            strict.error_code_of(method, params).await,
            -32601,
            "{method} must be an official method-not-found for a non-declaring client"
        );
    }

    // (c) No authority attached + client declares: declaring an unbacked
    // capability is not a negotiation, and nothing is claimed.
    let (mut unbacked, _task3) = start_server(EchoBackend::new());
    let init = unbacked
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    assert!(
        init["result"].get("extensions").is_none(),
        "an unbacked terminal authority must never be negotiated: {init}"
    );
    let sid = new_session(&mut unbacked).await;
    for (method, params) in terminal_probes(&sid) {
        assert_eq!(
            unbacked.error_code_of(method, params).await,
            -32601,
            "{method} must stay unavailable without a terminal authority"
        );
    }
}

#[tokio::test]
async fn terminal_hostile_params_are_typed_and_connection_survives() {
    let authority = Arc::new(FakeAuthority::new());
    let (mut client, _task) = start_server_with_terminals(
        EchoBackend::new(),
        authority.clone() as Arc<dyn TerminalAuthority>,
    );
    client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    let sid = new_session(&mut client).await;
    let created = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh" }),
        )
        .await;
    let tid = created["result"]["terminalId"]
        .as_str()
        .unwrap()
        .to_string();

    let hostile: Vec<(&str, Value)> = vec![
        (
            "terminal/input",
            json!({
                "sessionId": sid,
                "terminalId": tid,
                "data": "x".repeat(faktor_acp::MAX_TERMINAL_INPUT_BYTES + 1),
            }),
        ),
        (
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid, "data": "a\u{0}b" }),
        ),
        (
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid, "data": 7 }),
        ),
        (
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid }),
        ),
        (
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid, "data": "x", "extra": true }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 0, "cols": 80 }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 65536, "cols": 80 }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": -1, "cols": 80 }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 1.5, "cols": 80 }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 24 }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "" }),
        ),
        (
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "x".repeat(faktor_acp::MAX_TERMINAL_COMMAND_BYTES + 1),
            }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh\u{0}od" }),
        ),
        (
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "sh",
                "args": (0..=faktor_acp::MAX_TERMINAL_ARGS).map(|_| "a").collect::<Vec<_>>(),
            }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "cwd": "/\u{0}" }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "env": ["BAD-NAME"] }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "env": { "PATH": "/bin" } }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "rows": 0 }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "bogus": 1 }),
        ),
        (
            "terminal/create",
            json!({ "sessionId": "foreign-session", "command": "sh" }),
        ),
        (
            "terminal/kill",
            json!({ "sessionId": sid, "terminalId": "t-999" }),
        ),
        (
            "terminal/kill",
            json!({ "sessionId": "foreign-session", "terminalId": tid }),
        ),
        ("terminal/close", json!({ "sessionId": sid })),
        ("terminal/list", json!({ "sessionId": "foreign-session" })),
        ("terminal/list", json!({ "sessionId": sid, "extra": 1 })),
    ];
    for (method, params) in hostile {
        let msg = client.request(method, params.clone()).await;
        assert_eq!(
            msg["error"]["code"], -32602,
            "{method} with hostile params must be a typed invalid-params refusal: {msg}"
        );
    }

    // A refusing authority maps to the internal error (with the message in
    // data), never a silent success.
    let msg = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "refuse-spawn" }),
        )
        .await;
    assert_eq!(msg["error"]["code"], -32603);
    let msg = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "invalid-spec" }),
        )
        .await;
    assert_eq!(msg["error"]["code"], -32602);

    // The connection still serves and the original terminal is intact.
    let msg = client
        .request(
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid, "data": "ok\n" }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    assert_eq!(authority.handle(&tid).input(), b"ok\n");
    let info = client.request("agent_info", json!({})).await;
    assert_eq!(info["result"]["name"], "faktor-test-agent");
}

#[tokio::test]
async fn terminal_output_notifications_are_bounded_coalesced_and_ordered() {
    let authority = Arc::new(FakeAuthority::new());
    let config = AcpConfig {
        terminal_output_frame_max: 1024,
        terminal_output_window_max: 4096,
        terminal_poll_interval: Duration::from_millis(10),
        ..AcpConfig::default()
    };
    let (mut client, _task) = start_server_with_terminals_and_config(
        EchoBackend::new(),
        authority.clone() as Arc<dyn TerminalAuthority>,
        config,
    );
    client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    let sid = new_session(&mut client).await;
    let created = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh" }),
        )
        .await;
    let tid = created["result"]["terminalId"]
        .as_str()
        .unwrap()
        .to_string();

    // 8 KiB in one window: 4 frames of <= 1 KiB are emitted, the remaining
    // 4 KiB hit the per-window cap and are DROPPED — but recorded.
    authority.handle(&tid).feed(&vec![b'a'; 8192]);
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while frames.len() < 4 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, client.expect_message())
            .await
            .expect("bounded output arrives");
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            frames.extend(terminal_updates(std::slice::from_ref(&msg), &tid));
        }
    }
    let mut total = 0usize;
    for (index, update) in frames.iter().enumerate() {
        assert_eq!(update["seq"], json!(index as u64), "per-terminal order");
        assert_eq!(update["kind"], "terminalOutput");
        assert_eq!(update["terminalId"], tid);
        let bytes = update["bytes"].as_u64().unwrap() as usize;
        assert!(bytes <= 1024, "frame exceeds the configured cap: {update}");
        assert_eq!(
            update["data"].as_str().unwrap().len(),
            bytes,
            "lossy data length matches the raw byte count for ASCII"
        );
        total += bytes;
    }
    assert_eq!(total, 4096, "the per-window cap bounds one tick");

    // The dropped bytes are recorded on the ownership row (never silent).
    // Poll: the recording happens in the same pump tick that emitted the
    // frames, so a concurrent list may observe it one beat later.
    let row = wait_for_terminal_row(&mut client, &sid, &tid, "recorded backpressure", |row| {
        row["droppedBytes"] == json!(4096)
    })
    .await;
    assert_eq!(row["backpressureEvents"], json!(1));
    assert_eq!(row["emittedFrames"], json!(4));

    // The next frame carries the previous window's recorded loss, then a
    // fresh seq continues the order.
    authority.handle(&tid).feed(b"tail");
    let update = recv_terminal_output_until(&mut client, &tid, "tail frame", |data| {
        data.contains("tail")
    })
    .await;
    assert_eq!(update["seq"], json!(4));
    assert_eq!(update["bytes"], json!(4));
    assert_eq!(update["droppedBytes"], json!(4096));
    assert_eq!(update["backpressureEvents"], json!(1));
}

async fn wait_for_terminal_row(
    client: &mut WireClient,
    session: &str,
    terminal_id: &str,
    what: &str,
    mut pred: impl FnMut(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let list = client
            .request("terminal/list", json!({ "sessionId": session }))
            .await;
        if let Some(row) = list["result"]["terminals"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["terminalId"] == terminal_id))
        {
            if pred(row) {
                return row.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn terminal_list_is_scoped_to_the_requesting_session_only() {
    let authority = Arc::new(FakeAuthority::new());
    let (mut client, _task) = start_server_with_terminals(
        EchoBackend::new(),
        authority.clone() as Arc<dyn TerminalAuthority>,
    );
    client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    let sid_a = new_session(&mut client).await;
    let sid_b = new_session(&mut client).await;
    assert_ne!(sid_a, sid_b);

    let created_a = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid_a, "command": "sh" }),
        )
        .await;
    let tid_a = created_a["result"]["terminalId"]
        .as_str()
        .unwrap()
        .to_string();
    let created_b = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid_b, "command": "sh" }),
        )
        .await;
    let tid_b = created_b["result"]["terminalId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(tid_a, tid_b);

    // Each session's view holds exactly its own row.
    let list_a = client
        .request("terminal/list", json!({ "sessionId": sid_a }))
        .await;
    assert_eq!(list_a["result"]["sessionId"], sid_a);
    let rows_a = list_a["result"]["terminals"].as_array().unwrap();
    assert_eq!(rows_a.len(), 1);
    assert_eq!(rows_a[0]["terminalId"], tid_a);
    assert_eq!(rows_a[0]["sessionId"], sid_a);

    let list_b = client
        .request("terminal/list", json!({ "sessionId": sid_b }))
        .await;
    let rows_b = list_b["result"]["terminals"].as_array().unwrap();
    assert_eq!(rows_b.len(), 1);
    assert_eq!(rows_b[0]["terminalId"], tid_b);
    assert!(
        rows_b.iter().all(|row| row["terminalId"] != tid_a),
        "session B must never see session A's terminal: {list_b}"
    );

    // Cross-session operations are typed denials, identical to unknown ids.
    for (method, params) in [
        (
            "terminal/input",
            json!({ "sessionId": sid_b, "terminalId": tid_a, "data": "x" }),
        ),
        (
            "terminal/resize",
            json!({ "sessionId": sid_b, "terminalId": tid_a, "rows": 10, "cols": 10 }),
        ),
        (
            "terminal/kill",
            json!({ "sessionId": sid_b, "terminalId": tid_a }),
        ),
        (
            "terminal/close",
            json!({ "sessionId": sid_b, "terminalId": tid_a }),
        ),
    ] {
        let msg = client.request(method, params).await;
        assert_eq!(
            msg["error"]["code"], -32602,
            "a foreign session must never operate another session's terminal: {msg}"
        );
    }

    // Both terminals are still alive for their owners.
    let msg = client
        .request(
            "terminal/input",
            json!({ "sessionId": sid_a, "terminalId": tid_a, "data": "a\n" }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    let msg = client
        .request(
            "terminal/input",
            json!({ "sessionId": sid_b, "terminalId": tid_b, "data": "b\n" }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
}

// ---------------------------------------------------------------------------
// Real PTY lifecycle (the authority is a thin adapter over the existing
// `faktor-pty` crate: no second PTY implementation lives in this crate).
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[derive(Default)]
struct PtyAuthority {
    next: AtomicU64,
}

#[cfg(unix)]
struct PtyHandle {
    id: String,
    ownership: String,
    pid: u32,
    pty: Mutex<faktor_pty::Pty>,
}

#[cfg(unix)]
impl TerminalAuthority for PtyAuthority {
    fn create(
        &self,
        _session_id: &str,
        spec: &TerminalSpec,
    ) -> Result<Arc<dyn TerminalHandle>, TerminalError> {
        let env = if spec.env.is_empty() {
            faktor_pty::EnvSpec::default_baseline()
        } else {
            faktor_pty::EnvSpec::Allowlisted(spec.env.clone())
        };
        let config = faktor_pty::PtyConfig {
            command: spec.command.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            env,
            rows: spec.rows,
            cols: spec.cols,
        };
        let pty = faktor_pty::Pty::spawn(&config).map_err(|e| TerminalError::Refused(e.message))?;
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(PtyHandle {
            id: format!("pty-{n}"),
            ownership: format!("own-pty-{n}"),
            pid: pty.pid(),
            pty: Mutex::new(pty),
        }))
    }
}

#[cfg(unix)]
impl TerminalHandle for PtyHandle {
    fn terminal_id(&self) -> &str {
        &self.id
    }
    fn pid(&self) -> u32 {
        self.pid
    }
    fn is_alive(&self) -> bool {
        self.pty.lock().unwrap().is_alive()
    }
    fn ownership_id(&self) -> &str {
        &self.ownership
    }
    fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.pty
            .lock()
            .unwrap()
            .write_all(bytes)
            .map_err(|e| TerminalError::Refused(e.message))
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        self.pty
            .lock()
            .unwrap()
            .resize(rows, cols)
            .map_err(|e| TerminalError::Refused(e.message))
    }
    fn drain_output(&self) -> Vec<u8> {
        self.pty.lock().unwrap().read_available()
    }
    fn kill(&self) -> Result<(), TerminalError> {
        self.pty.lock().unwrap().kill();
        Ok(())
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid} 2>/dev/null"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Send one request, then read until BOTH its response and a terminal
/// update whose `data` contains `needle` have been seen (the two race, and
/// neither may be dropped).
#[cfg(unix)]
async fn request_until_terminal_output(
    client: &mut WireClient,
    id: u64,
    method: &str,
    params: Value,
    terminal_id: &str,
    needle: &str,
) -> (Value, Vec<Value>) {
    let bytes = frame(method.to_string(), id, params);
    client.write.write_all(&bytes).await.expect("client write");
    client.write.flush().await.expect("client flush");
    let mut updates: Vec<Value> = Vec::new();
    let mut response: Option<Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for terminal output {needle:?}"
        );
        let msg = tokio::time::timeout(remaining, client.expect_message())
            .await
            .expect("pty output arrives");
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            updates.extend(terminal_updates(std::slice::from_ref(&msg), terminal_id));
        } else if msg["id"] == json!(id) {
            response = Some(msg);
        }
        let got = updates
            .iter()
            .any(|update| update["data"].as_str().unwrap_or_default().contains(needle));
        if got {
            if let Some(response) = response {
                return (response, updates);
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_lifecycle_over_a_real_pty_create_input_resize_kill_close() {
    let authority: Arc<dyn TerminalAuthority> = Arc::new(PtyAuthority::default());
    let (mut client, _task) = start_server_with_terminals(EchoBackend::new(), authority);
    client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    let sid = new_session(&mut client).await;

    let created = client
        .request(
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "sh",
                "args": ["-c", "stty -echo; read x; echo got:$x; read y; stty size; sleep 30"],
                "env": ["PATH"],
                "rows": 24,
                "cols": 80,
            }),
        )
        .await;
    assert_eq!(created["error"], Value::Null, "create failed: {created}");
    let result = &created["result"];
    let tid = result["terminalId"]
        .as_str()
        .expect("terminalId")
        .to_string();
    let pid = result["pid"].as_u64().expect("pid") as u32;
    assert!(pid > 0, "a real pty pid");
    assert!(result["ownershipId"]
        .as_str()
        .unwrap()
        .starts_with("own-pty-"));
    assert_eq!(result["sessionId"], sid);
    assert!(pid_alive(pid), "the pty child is alive after create");

    // Input round trip through the real tty.
    let (response, updates) = request_until_terminal_output(
        &mut client,
        100,
        "terminal/input",
        json!({ "sessionId": sid, "terminalId": tid, "data": "hello\n" }),
        &tid,
        "got:hello",
    )
    .await;
    assert_eq!(response["result"], json!({}));
    assert!(
        !updates.is_empty(),
        "the tty output must arrive as terminalOutput frames"
    );

    // Resize reaches the kernel; the child prints the new geometry.
    let msg = client
        .request(
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": tid, "rows": 33, "cols": 121 }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    let (_response, resized_updates) = request_until_terminal_output(
        &mut client,
        101,
        "terminal/input",
        json!({ "sessionId": sid, "terminalId": tid, "data": "go\n" }),
        &tid,
        "33 121",
    )
    .await;
    let mut updates = updates;
    updates.extend(resized_updates);

    // Every frame is bounded, ordered per terminal, and names the ownership.
    let mut last_seq: Option<u64> = None;
    for update in &updates {
        assert_eq!(update["kind"], "terminalOutput");
        assert_eq!(update["terminalId"], tid);
        let seq = update["seq"].as_u64().unwrap();
        if let Some(last) = last_seq {
            assert!(seq > last, "terminal frames must be ordered by seq");
        }
        last_seq = Some(seq);
        let bytes = update["bytes"].as_u64().unwrap() as usize;
        assert!(
            bytes <= faktor_acp::TERMINAL_OUTPUT_FRAME_MAX,
            "frame exceeds the default per-frame bound: {update}"
        );
    }

    // Session-owned row: alive with its ownership id while the child lives.
    let row = wait_for_terminal_row(&mut client, &sid, &tid, "live row", |row| {
        row["alive"] == json!(true)
    })
    .await;
    assert_eq!(row["pid"], json!(pid));
    assert_eq!(row["sessionId"], sid);

    // Kill terminates the whole tree; the row reports the dead child.
    let msg = client
        .request(
            "terminal/kill",
            json!({ "sessionId": sid, "terminalId": tid }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    wait_for_terminal_row(&mut client, &sid, &tid, "killed row", |row| {
        row["alive"] == json!(false)
    })
    .await;
    assert!(!pid_alive(pid), "kill must terminate the pty child");

    // Close releases the row; the id is gone and typed refusals follow.
    let msg = client
        .request(
            "terminal/close",
            json!({ "sessionId": sid, "terminalId": tid }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
    let list = client
        .request("terminal/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(list["result"]["terminals"], json!([]));
    let msg = client
        .request(
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid, "data": "x" }),
        )
        .await;
    assert_eq!(msg["error"]["code"], -32602);
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_connection_close_kills_the_pty_tree() {
    let authority: Arc<dyn TerminalAuthority> = Arc::new(PtyAuthority::default());
    let (mut client, task) = start_server_with_terminals(EchoBackend::new(), authority);
    client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    let sid = new_session(&mut client).await;
    let created = client
        .request(
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "sh",
                "args": ["-c", "sleep 300"],
            }),
        )
        .await;
    let pid = created["result"]["pid"].as_u64().unwrap() as u32;
    assert!(pid > 0 && pid_alive(pid), "child alive before the close");

    // Drop the connection: the serve loop winds down and kills every
    // session-owned terminal before returning (no orphans).
    drop(client);
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("serve loop winds down at EOF")
        .expect("serve task joins");
    assert!(result.is_ok(), "{result:?}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "pty child {pid} outlived its connection"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_env_allowlist_never_carries_daemon_secrets() {
    let authority: Arc<dyn TerminalAuthority> = Arc::new(PtyAuthority::default());
    let (mut client, _task) = start_server_with_terminals(EchoBackend::new(), authority);
    client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    let sid = new_session(&mut client).await;
    let created = client
        .request(
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "sh",
                "args": ["-c", "stty -echo; env; echo ENV_DONE"],
                "env": [
                    "PATH",
                    "FAKTOR_SERVER_PASSWORD",
                    "OPENAI_API_KEY",
                    "TEST_PRIVATE_SECRET",
                    "GITHUB_TOKEN",
                ],
            }),
        )
        .await;
    let tid = created["result"]["terminalId"]
        .as_str()
        .expect("terminalId")
        .to_string();

    // Concatenate every terminalOutput frame until the sentinel closes the
    // environment dump (the dump may be split across coalesced frames).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut text = String::new();
    while !text.contains("ENV_DONE") {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "env dump never completed: {text:?}");
        let msg = tokio::time::timeout(remaining, client.expect_message())
            .await
            .expect("env output");
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            for update in terminal_updates(std::slice::from_ref(&msg), &tid) {
                text.push_str(update["data"].as_str().unwrap_or_default());
            }
        }
    }
    assert!(
        text.lines().any(|line| line.starts_with("PATH=")),
        "the allowlisted PATH must arrive: {text:?}"
    );
    for secret in [
        "FAKTOR_SERVER_PASSWORD",
        "OPENAI_API_KEY",
        "TEST_PRIVATE_SECRET",
        "GITHUB_TOKEN",
    ] {
        assert!(
            !text.contains(secret),
            "the daemon secret {secret} leaked through the ACP terminal: {text:?}"
        );
    }

    let msg = client
        .request(
            "terminal/close",
            json!({ "sessionId": sid, "terminalId": tid }),
        )
        .await;
    assert_eq!(msg["result"], json!({}));
}

// ---------------------------------------------------------------------------
// Durable terminal authority: the ACP adapter over the daemon's ONE
// TerminalService (the same authority the native HTTP surface drives).
// ---------------------------------------------------------------------------

/// The ACP adapter over the real durable server authority — the exact
/// delegation shape the daemon attaches with `with_terminal_authority`.
struct DurableAuthority {
    service: Arc<faktor_server::native::terminal_authority::TerminalService>,
}

fn map_durable_error(
    error: faktor_server::native::terminal_authority::TerminalServiceError,
) -> TerminalError {
    use faktor_server::native::terminal_authority::TerminalServiceError as ServiceError;
    match error {
        ServiceError::Invalid(message) => TerminalError::Invalid(message),
        ServiceError::Unknown { terminal_id, .. }
        | ServiceError::ForeignScope { terminal_id, .. } => {
            TerminalError::Invalid(format!("unknown terminal {terminal_id:?}"))
        }
        ServiceError::Lost {
            terminal_id,
            detail,
        } => TerminalError::Refused(format!("terminal {terminal_id:?} is lost: {detail}")),
        ServiceError::IdentityMismatch { terminal_id, .. } => {
            TerminalError::Refused(format!("terminal {terminal_id:?} identity mismatch"))
        }
        ServiceError::State {
            terminal_id, state, ..
        } => TerminalError::Refused(format!("terminal {terminal_id:?} is {state}")),
        ServiceError::Refused(message) => TerminalError::Refused(message),
        ServiceError::Unavailable(message) => TerminalError::Unavailable(message),
    }
}

impl TerminalAuthority for DurableAuthority {
    fn create(
        &self,
        session_id: &str,
        spec: &TerminalSpec,
    ) -> Result<Arc<dyn TerminalHandle>, TerminalError> {
        let request = faktor_server::native::terminal_authority::TerminalSpawnRequest {
            command: spec.command.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            rows: spec.rows,
            cols: spec.cols,
        };
        let creation = self
            .service
            .spawn(session_id, &request)
            .map_err(map_durable_error)?;
        Ok(Arc::new(DurableHandle {
            handle: creation.handle,
        }))
    }

    fn exposes_durable_terminals(&self) -> bool {
        true
    }

    fn list_terminals(
        &self,
        session_id: &str,
    ) -> Result<Vec<faktor_acp::TerminalAuthorityRow>, TerminalError> {
        self.service
            .list(session_id)
            .map_err(map_durable_error)
            .map(|views| {
                views
                    .into_iter()
                    .map(|view| {
                        let state = view.state_tag().to_string();
                        faktor_acp::TerminalAuthorityRow {
                            terminal_id: view.terminal_id,
                            session_id: view.session_id.to_string(),
                            task_id: view.task_id.to_string(),
                            agent_id: view.agent_id,
                            operation_id: view.operation_id.to_string(),
                            pid: view.pid,
                            start_time_ms: view.start_time_ms,
                            state,
                            alive: view.alive,
                        }
                    })
                    .collect()
            })
    }
}

struct DurableHandle {
    handle: faktor_server::native::terminal_authority::TerminalHandle,
}

impl TerminalHandle for DurableHandle {
    fn terminal_id(&self) -> &str {
        self.handle.terminal_id()
    }
    fn pid(&self) -> u32 {
        self.handle.pid()
    }
    fn is_alive(&self) -> bool {
        self.handle.is_alive()
    }
    fn ownership_id(&self) -> &str {
        self.handle.ownership_id()
    }
    fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.handle.write(bytes).map_err(|e| match e {
            faktor_server::native::terminal_authority::TerminalRegistryError::Invalid(m) => {
                TerminalError::Invalid(m)
            }
            faktor_server::native::terminal_authority::TerminalRegistryError::Refused(m) => {
                TerminalError::Refused(m)
            }
            faktor_server::native::terminal_authority::TerminalRegistryError::Unavailable(m) => {
                TerminalError::Unavailable(m)
            }
        })
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        self.handle.resize(rows, cols).map_err(|e| match e {
            faktor_server::native::terminal_authority::TerminalRegistryError::Invalid(m) => {
                TerminalError::Invalid(m)
            }
            faktor_server::native::terminal_authority::TerminalRegistryError::Refused(m) => {
                TerminalError::Refused(m)
            }
            faktor_server::native::terminal_authority::TerminalRegistryError::Unavailable(m) => {
                TerminalError::Unavailable(m)
            }
        })
    }
    fn drain_output(&self) -> Vec<u8> {
        self.handle.drain_output()
    }
    fn kill(&self) -> Result<(), TerminalError> {
        self.handle.kill().map_err(|e| match e {
            faktor_server::native::terminal_authority::TerminalRegistryError::Invalid(m) => {
                TerminalError::Invalid(m)
            }
            faktor_server::native::terminal_authority::TerminalRegistryError::Refused(m) => {
                TerminalError::Refused(m)
            }
            faktor_server::native::terminal_authority::TerminalRegistryError::Unavailable(m) => {
                TerminalError::Unavailable(m)
            }
        })
    }
}

/// Backend whose one session is the REAL durable session id, so the ACP
/// terminal calls reach the real durable authority.
struct DurableSessionBackend {
    session_id: String,
}

impl AcpBackend for DurableSessionBackend {
    fn agent_info(&self) -> Value {
        json!({ "name": "durable-terminal-agent", "version": "0.0.0" })
    }
    fn create_session(&self, _params: &Value) -> Result<String, String> {
        Ok(self.session_id.clone())
    }
    fn prompt(&self, _session_id: &str, _text: &str) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn abort(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn list_sessions(&self) -> Vec<String> {
        vec![self.session_id.clone()]
    }
}

fn durable_kinds(handle: &faktor_session::SessionHandle) -> Vec<faktor_session::TerminalEventKind> {
    handle
        .ledger_terminal_rows(None)
        .unwrap()
        .into_iter()
        .map(|record| record.kind)
        .collect()
}

#[tokio::test]
async fn durable_terminal_round_trip_through_acp_and_native_adapters_is_identical() {
    use faktor_server::native::terminal_authority::{TerminalService, TerminalSpawnRequest};

    let root = std::env::temp_dir().join(format!(
        "kp-acp-durable-terminal-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let manager =
        faktor_session::SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let ws = manager.create_workspace("/tmp").unwrap();
    let created = manager
        .create_session(ws, "acp-durable-terminal", "fake", "m")
        .unwrap();
    let sid = created.id().to_string();
    let session = manager.get_session(created.id()).unwrap().unwrap();

    let service = TerminalService::with_identity_probe(
        manager.clone(),
        Arc::new(|pid: u32| (pid != 0).then_some(1_700_000_000_000)),
    );
    let authority: Arc<dyn TerminalAuthority> = Arc::new(DurableAuthority {
        service: Arc::clone(&service),
    });
    let (mut client, _task) = start_server_with_terminals(
        DurableSessionBackend {
            session_id: sid.clone(),
        },
        authority,
    );
    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    assert_eq!(init["result"]["extensions"], json!(["faktor.terminal"]));
    assert_eq!(new_session(&mut client).await, sid);

    // Adapter A (ACP): create t1.
    let created = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "/bin/sleep", "args": ["30"] }),
        )
        .await;
    let t1 = match created["result"]["terminalId"].as_str() {
        Some(id) => id.to_string(),
        // Documented platform refusal: nothing to compare on a platform
        // with no PTY backend.
        None => return,
    };
    // Adapter B (the native backend API): create t2.
    let spawned = service.spawn(
        &sid,
        &TerminalSpawnRequest {
            command: "/bin/sleep".into(),
            args: vec!["30".into()],
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        },
    );
    let t2 = match spawned {
        Ok(creation) => creation.handle.terminal_id().to_string(),
        Err(_) => return,
    };

    // Both adapters list the SAME two durable rows (ACP projects the
    // authority's rows, the native API folds the durable ledger).
    let list = client
        .request("terminal/list", json!({ "sessionId": sid }))
        .await;
    let acp_rows = list["result"]["terminals"].as_array().unwrap().clone();
    assert_eq!(acp_rows.len(), 2, "{list}");
    let mut acp_ids: Vec<String> = acp_rows
        .iter()
        .map(|row| row["terminalId"].as_str().unwrap().to_string())
        .collect();
    acp_ids.sort();
    let mut native_ids: Vec<String> = service
        .list(&sid)
        .unwrap()
        .into_iter()
        .map(|view| view.terminal_id)
        .collect();
    native_ids.sort();
    assert_eq!(acp_ids, native_ids, "both adapters see the same rows");
    assert!(acp_rows.iter().all(|row| row["state"] == "running"));
    assert_eq!(
        durable_kinds(&session).len(),
        4,
        "2 terminals x created+running"
    );

    // Input and resize round-trip through BOTH adapters; neither is a
    // journaled lifecycle transition. ACP drives the terminal IT created
    // (its connection table), the native adapter drives both durable rows.
    let msg = client
        .request(
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": t1, "data": "x" }),
        )
        .await;
    assert_eq!(msg["result"], json!({}), "{msg}");
    let msg = client
        .request(
            "terminal/resize",
            json!({ "sessionId": sid, "terminalId": t1, "rows": 33, "cols": 121 }),
        )
        .await;
    assert_eq!(msg["result"], json!({}), "{msg}");
    for terminal in [&t1, &t2] {
        service.input(&sid, terminal, b"y").unwrap();
        service.resize(&sid, terminal, 24, 80).unwrap();
    }
    // ACP's connection ownership is enforced against the durable rows: a
    // terminal this connection did not create is a typed refusal, and the
    // native-created row is untouched.
    let refused = client
        .request(
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": t2, "data": "z" }),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert_eq!(
        durable_kinds(&session).len(),
        4,
        "I/O journals no lifecycle row"
    );

    // Kill t1 through ACP and t2 through the native API: exactly one Killed
    // row each, and BOTH adapters agree on the terminal states.
    let msg = client
        .request(
            "terminal/kill",
            json!({ "sessionId": sid, "terminalId": t1 }),
        )
        .await;
    assert_eq!(msg["result"], json!({}), "{msg}");
    assert!(service.kill(&sid, &t2, "native adapter kill").unwrap());

    // The exact durable sequences, session-scoped and complete.
    let records = session.ledger_terminal_rows(None).unwrap();
    assert_eq!(
        records.len(),
        6,
        "created+running+killed per terminal: {records:?}"
    );
    for terminal in [&t1, &t2] {
        let kinds: Vec<faktor_session::TerminalEventKind> = records
            .iter()
            .filter(|record| &record.row.terminal_id == terminal)
            .map(|record| record.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                faktor_session::TerminalEventKind::Created,
                faktor_session::TerminalEventKind::Running,
                faktor_session::TerminalEventKind::Killed
            ],
            "{terminal} has exactly one terminal state sequence"
        );
    }

    // ACP's projection now reports the durable killed rows.
    let list = client
        .request("terminal/list", json!({ "sessionId": sid }))
        .await;
    let acp_rows = list["result"]["terminals"].as_array().unwrap();
    assert_eq!(acp_rows.len(), 2, "{list}");
    assert!(
        acp_rows
            .iter()
            .all(|row| row["state"] == "killed" && row["alive"] == false),
        "{list}"
    );
    let native_rows = service.list(&sid).unwrap();
    assert_eq!(native_rows.len(), 2);
    assert!(native_rows
        .iter()
        .all(|row| row.state_tag() == "killed" && !row.alive));
    // The two projections are the same rows.
    let mut native_states: Vec<(String, String)> = native_rows
        .iter()
        .map(|row| (row.terminal_id.clone(), row.state_tag().to_string()))
        .collect();
    native_states.sort();
    let mut acp_states: Vec<(String, String)> = acp_rows
        .iter()
        .map(|row| {
            (
                row["terminalId"].as_str().unwrap().to_string(),
                row["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    acp_states.sort();
    assert_eq!(acp_states, native_states);

    // A second kill through either adapter journals nothing further.
    let msg = client
        .request(
            "terminal/kill",
            json!({ "sessionId": sid, "terminalId": t1 }),
        )
        .await;
    assert_eq!(msg["result"], json!({}), "{msg}");
    assert_eq!(session.ledger_terminal_rows(None).unwrap().len(), 6);

    let _ = std::fs::remove_dir_all(&root);
}
