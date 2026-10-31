//! In-repo offline mock provider: a minimal OpenAI-compatible HTTP/SSE
//! server on raw tokio TCP (no HTTP library, no new dependency).
//!
//! It serves exactly three things:
//!
//! - `GET  …/models`            → a fixed, priced model list fixture;
//! - `POST …/chat/completions`  → a deterministic SSE completion stream
//!   (role frame, two content frames, finish+usage frame, `[DONE]`), each
//!   frame flushed separately so a mid-SSE transport fault has a real
//!   stream to cut;
//! - anything else              → typed 404 JSON.
//!
//! Every request is recorded (method, path, body) so tests can count the
//! PHYSICAL attempts the transport made — an oracle that lives in the
//! harness, never in the daemon.
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One recorded upstream request.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// A running mock provider bound to an ephemeral loopback port.
pub struct MockProvider {
    pub addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    chat_hits: Arc<AtomicU64>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    pub fn start() -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let chat_hits = Arc::new(AtomicU64::new(0));
        let requests_task = requests.clone();
        let hits_task = chat_hits.clone();
        let thread = std::thread::Builder::new()
            .name("mock-provider".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("mock runtime");
                rt.block_on(async move {
                    let listener = TcpListener::bind(("127.0.0.1", 0))
                        .await
                        .expect("mock bind");
                    let addr = listener.local_addr().expect("mock addr");
                    let _ = ready_tx.send(addr);
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accepted = listener.accept() => {
                                let Ok((stream, _)) = accepted else { break };
                                let requests = requests_task.clone();
                                let hits = hits_task.clone();
                                tokio::spawn(async move {
                                    let _ = handle(stream, requests, hits).await;
                                });
                            }
                        }
                    }
                });
            })
            .expect("mock thread");
        let addr = ready_rx.recv().expect("mock ready");
        Self {
            addr,
            requests,
            chat_hits,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    /// Physical `POST …/chat/completions` attempts observed upstream.
    pub fn chat_attempts(&self) -> u64 {
        self.chat_hits.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("mock records").clone()
    }

    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn handle(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    chat_hits: Arc<AtomicU64>,
) -> std::io::Result<()> {
    let request = match read_request(&mut stream).await {
        Some(bytes) => bytes,
        None => return Ok(()),
    };
    let head_end = request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(request.len());
    let head = String::from_utf8_lossy(&request[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let body = request[head_end..].to_vec();
    requests
        .lock()
        .expect("mock records")
        .push(RecordedRequest {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
        });

    if method == "POST" && path.ends_with("/chat/completions") {
        chat_hits.fetch_add(1, Ordering::SeqCst);
        return sse_completion(&mut stream).await;
    }
    if method == "GET" && path.ends_with("/models") {
        let payload = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "m", "object": "model", "created": 1, "owned_by": "faktor-mock"}
            ]
        });
        let body = payload.to_string();
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;
        return stream.flush().await;
    }
    let payload = serde_json::json!({"error": {"message": "mock: unhandled route", "type": "invalid_request_error"}});
    let body = payload.to_string();
    let head = format!(
        "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// The deterministic SSE completion. Frame bytes are FIXED (the chaos
/// proxy's byte thresholds are calibrated against them).
async fn sse_completion(stream: &mut TcpStream) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    let frames: [&str; 5] = [
        "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"chaos\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5,\"total_tokens\":16}}\n\n",
        "data: [DONE]\n\n",
    ];
    for frame in frames {
        stream.write_all(frame.as_bytes()).await?;
        stream.flush().await?;
        // A real stream: frames arrive separately, never as one blast.
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    stream.shutdown().await
}

async fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(1024);
    let mut buf = [0u8; 2048];
    let head_end = loop {
        if let Some(pos) = out.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            break pos;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&out[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while out.len() < head_end + content_length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    // TEST-SEPARATION: oracle=independent-reference isolation=separate-process
    use super::*;

    #[test]
    fn mock_serves_a_deterministic_sse_body_with_a_counted_attempt() {
        let mut mock = MockProvider::start();
        let addr = mock.addr;
        // Raw client: one chat completion, byte-exact body inspection.
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let body = serde_json::json!({"model": "m", "messages": []}).to_string();
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, req.as_bytes()).expect("write");
        let mut response = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut response).expect("read");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("text/event-stream"), "{text}");
        assert!(text.contains("data: [DONE]"), "{text}");
        assert_eq!(mock.chat_attempts(), 1, "one recorded attempt");
        assert_eq!(mock.requests().len(), 1, "request recorded");
        mock.shutdown();
        // After shutdown the port no longer accepts: no leaked listener.
        assert!(
            std::net::TcpStream::connect(addr).is_err(),
            "mock must be gone after shutdown"
        );
    }
}
