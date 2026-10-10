//! A loopback fake Kiwi server over a `std::net` listener, shared by the
//! integration tests. It speaks real HTTP/1.1 over a loopback socket so the
//! client rides the SAME checked egress transport production uses; every
//! request is recorded for byte-level assertions and every response is
//! scripted per request.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One scripted reply.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A normal HTTP response with a JSON body.
    Http { status: u16, body: String },
    /// Raw bytes written verbatim (hostile framing tests).
    Raw(Vec<u8>),
    /// Accept the request, then hold the connection open without answering
    /// for the given duration (watchdog/deadline tests).
    Hang(Duration),
}

/// One recorded request.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct State {
    replies: VecDeque<Reply>,
    requests: Vec<Recorded>,
}

pub struct FakeKiwi {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeKiwi {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake kiwi bind");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("fake kiwi addr");
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let state = thread_state.clone();
                        // One short-lived handler per connection so a scripted
                        // hang never blocks the accept loop.
                        std::thread::spawn(move || {
                            let _ = handle_connection(socket, &state);
                        });
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}/verify", self.addr)
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Queue one reply (one request consumes one reply).
    pub fn push(&self, reply: Reply) {
        self.state
            .lock()
            .expect("fake kiwi state")
            .replies
            .push_back(reply);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().expect("fake kiwi state").requests.clone()
    }
}

impl Drop for FakeKiwi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(mut socket: TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 256 * 1024 {
            return Ok(());
        }
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let path = target
        .split_once('?')
        .map(|(path, _)| path.to_string())
        .unwrap_or_else(|| target.clone());
    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    while buffer.len() < header_end + content_length {
        let read = socket.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(
        &buffer[header_end..(header_end + content_length).min(buffer.len())],
    )
    .to_string();
    let reply = {
        let mut state = state.lock().expect("fake kiwi state");
        state.requests.push(Recorded {
            method,
            path,
            headers,
            body,
        });
        state.replies.pop_front()
    };
    let reply = reply.unwrap_or(Reply::Http {
        status: 404,
        body: "no scripted kiwi reply for this route".to_string(),
    });
    match reply {
        Reply::Http { status, body } => {
            let response = format!(
                "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                status_text(status),
                body.len()
            );
            socket.write_all(response.as_bytes())?;
            socket.flush()?;
        }
        Reply::Raw(bytes) => {
            socket.write_all(&bytes)?;
            socket.flush()?;
        }
        Reply::Hang(duration) => {
            std::thread::sleep(duration);
            // The client has already given up (watchdog/deadline); answer
            // anyway so the socket closes cleanly.
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
            let _ = socket.flush();
        }
    }
    Ok(())
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
