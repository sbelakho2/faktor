//! Test-only loopback HTTP mock shared by the daemon-wiring tests (SSO
//! adapter, GitHub App sync, billing report schedule). It speaks real HTTP
//! over a loopback socket so every request rides the SAME checked transport
//! production uses; responses are scripted per `METHOD path` and every
//! request is recorded for byte-level assertions.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Reply {
    pub(crate) fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }

    pub(crate) fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Recorded {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Recorded {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct MockState {
    routes: HashMap<(String, String), VecDeque<Reply>>,
    requests: Vec<Recorded>,
}

pub(crate) struct MockServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    pub(crate) async fn start() -> Self {
        let state = Arc::new(Mutex::new(MockState::default()));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock bind");
        let addr = listener.local_addr().expect("mock addr");
        let serve_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let state = serve_state.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(&mut socket, &state).await;
                });
            }
        });
        Self { addr, state, task }
    }

    pub(crate) fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queue one reply for `METHOD path` (query strings are not part of the
    /// route key; the exact query is asserted from the recording).
    pub(crate) fn push(&self, method: &str, path: &str, reply: Reply) {
        self.state
            .lock()
            .expect("mock state")
            .routes
            .entry((method.to_string(), path.to_string()))
            .or_default()
            .push_back(reply);
    }

    pub(crate) fn requests(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.state
            .lock()
            .expect("mock state")
            .requests
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .cloned()
            .collect()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.state.lock().expect("mock state").requests.len()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_conn(
    socket: &mut tokio::net::TcpStream,
    state: &Arc<Mutex<MockState>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 128 * 1024 {
            return Ok(());
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, _query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let mut content_length = 0usize;
    let mut request_headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            request_headers.push((name, value));
        }
    }
    while buf.len() < header_end + content_length {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body =
        String::from_utf8_lossy(&buf[header_end..(header_end + content_length).min(buf.len())])
            .to_string();
    let reply = {
        let mut state = state.lock().expect("mock state");
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            headers: request_headers,
            body,
        });
        state
            .routes
            .get_mut(&(method, path))
            .and_then(|queue| queue.pop_front())
    };
    let reply = reply.unwrap_or_else(|| Reply::text(404, "no scripted reply for this route"));
    let mut response = format!(
        "HTTP/1.1 {} {}\r\n",
        reply.status,
        status_text(reply.status)
    );
    for (name, value) in &reply.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    // This mock serves exactly ONE request per connection and then drops
    // the socket; `Connection: close` keeps the pooled client from reusing
    // a socket the server is about to close (the pooled-reuse race was a
    // real flake: a reused half-closed socket surfaces as "error sending
    // request" on the NEXT request).
    response.push_str("connection: close\r\n");
    response.push_str(&format!("content-length: {}\r\n\r\n", reply.body.len()));
    response.push_str(&reply.body);
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
