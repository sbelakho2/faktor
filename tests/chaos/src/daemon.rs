//! REAL-daemon harness for the HAR-01 chaos rows: discovers (or builds)
//! the `faktor-cli` binary, starts `faktor-cli serve` into a throwaway
//! data dir, speaks the native HTTP API with bounded deadlines, counts
//! budget-ledger rows in the store database read-only, and audits the
//! process table for orphans.
//!
//! Isolation: every scenario owns a fresh temp dir (never the operator's
//! `~/.faktor*`), an ephemeral loopback port and a unique path token that
//! the process-table scan matches against — so parallel tests cannot see
//! each other's children.
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process,isolated-data-dir

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Wall-clock bound for one full scenario (never a hang).
pub const SCENARIO_DEADLINE: Duration = Duration::from_secs(60);
/// Bound for any single HTTP request.
pub const HTTP_DEADLINE: Duration = Duration::from_secs(10);

/// Locate the REAL `faktor-cli` binary: explicit override, then the
/// workspace target dir, then a patient `cargo build` (cargo waits on its
/// own package locks rather than failing).
pub fn cli_binary() -> PathBuf {
    if let Ok(path) = std::env::var("FAKTOR_CLI_BIN") {
        let path = PathBuf::from(path);
        assert!(path.exists(), "FAKTOR_CLI_BIN points at a missing file");
        return path;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["debug", "release"] {
        let candidate = root.join("target").join(profile).join("faktor-cli");
        if candidate.exists() {
            return candidate;
        }
    }
    // Not built yet: build it. `cargo` waits patiently on the build lock,
    // so this also survives a concurrent workspace build.
    let status = Command::new("cargo")
        .args(["build", "-p", "faktor-cli"])
        .current_dir(&root)
        .status()
        .expect("cargo build -p faktor-cli must be runnable");
    assert!(
        status.success(),
        "cargo build -p faktor-cli failed: {status}"
    );
    let candidate = root.join("target/debug/faktor-cli");
    assert!(
        candidate.exists(),
        "faktor-cli missing after build: run `cargo build -p faktor-cli`"
    );
    candidate
}

/// The frozen serve startup line: `faktor server listening on http://127.0.0.1:<port>`.
pub fn parse_startup_line(line: &str) -> Option<u16> {
    line.trim()
        .strip_prefix("faktor server listening on http://127.0.0.1:")?
        .parse()
        .ok()
}

/// One running `faktor-cli serve` process.
pub struct Daemon {
    child: Child,
    pub port: u16,
    pub password: String,
    pub data_dir: PathBuf,
    stderr_path: PathBuf,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    /// Spawn serve with a fresh temp data dir and wait (bounded) for the
    /// frozen startup line. `extra_env` carries the provider credential
    /// environment (from the operator's KEY=VALUE env file, or the mock's
    /// placeholder) into the daemon process — passed, never read here.
    pub fn start(
        data_dir: &Path,
        config: &Path,
        token: &str,
        extra_env: &[(String, String)],
    ) -> Self {
        let stderr_path = data_dir.join("daemon.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("stderr log");
        let stderr_clone = stderr_file.try_clone().expect("stderr clone");
        // The daemon enforces the frontend password contract: exactly 64
        // ASCII hex characters. Derive it deterministically from the
        // scenario token (unique per temp dir), zero-padded to 64.
        let mut password = String::with_capacity(64);
        for byte in token.bytes() {
            password.push_str(&format!("{byte:02x}"));
        }
        while password.len() < 64 {
            password.push_str("0badf00d");
        }
        password.truncate(64);
        assert!(
            password.len() == 64 && password.chars().all(|c| c.is_ascii_hexdigit()),
            "FAKTOR_SERVER_PASSWORD contract: 64 ASCII hex characters"
        );
        let mut command = Command::new(cli_binary());
        command
            .args(["serve", "--port", "0", "--data-dir"])
            .arg(data_dir)
            .args(["--config"])
            .arg(config)
            .env("FAKTOR_SERVER_PASSWORD", &password);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("spawn faktor-cli serve");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let stdout_thread = std::thread::Builder::new()
            .name("serve-stdout".into())
            .spawn(move || {
                let mut line = String::new();
                let mut byte = [0u8; 1];
                loop {
                    match stdout.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if byte[0] == b'\n' {
                                if tx.send(std::mem::take(&mut line)).is_err() {
                                    break;
                                }
                            } else {
                                line.push(byte[0] as char);
                            }
                        }
                    }
                }
                if !line.is_empty() {
                    let _ = tx.send(line);
                }
            })
            .expect("stdout reader");
        let port = loop {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(line) => {
                    if let Some(port) = parse_startup_line(&line) {
                        break port;
                    }
                    // Any other stdout line (e.g. an attestation) is
                    // tolerated; readiness still comes from the frozen line.
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                    panic!("serve never printed its startup line within 30s; stderr: {stderr}");
                }
            }
        };
        let _ = stderr_clone;
        Self {
            child,
            port,
            password,
            data_dir: data_dir.to_path_buf(),
            stderr_path,
            stdout_thread: Some(stdout_thread),
        }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `GET /native/ready` must answer `{ready: true}`.
    pub fn wait_ready(&self, deadline: Duration) -> Result<(), String> {
        let http = self.http();
        let started = Instant::now();
        loop {
            let (status, body) = http.get("/native/ready");
            let ready = status == 200
                && serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["ready"].as_bool())
                    .unwrap_or(false);
            if ready {
                return Ok(());
            }
            if started.elapsed() > deadline {
                return Err(format!(
                    "daemon not ready within {deadline:?}: status={status} body={body}"
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn http(&self) -> HttpClient {
        HttpClient {
            port: self.port,
            password: self.password.clone(),
        }
    }

    /// The serve process id (for the orphan-child census).
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    pub fn stderr_text(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    /// Graceful SIGTERM via the `kill` utility (bounded), then a hard kill
    /// fallback. Returns the exit status.
    pub fn shutdown(&mut self) -> Option<std::process::ExitStatus> {
        // One signal to exactly OUR child pid — never a process-group
        // broadcast — through the platform's kill binary (no unsafe
        // here), and only while the child is still alive (no `kill: no
        // such process` noise when it already exited).
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-TERM"])
                .arg(self.child.id().to_string())
                .output();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => {
                    self.join_stdout();
                    return Some(status);
                }
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    let status = self.child.wait().expect("kill wait");
                    self.join_stdout();
                    return Some(status);
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn join_stdout(&mut self) {
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Minimal blocking HTTP/1.1 client over std TCP (no new dependency).
#[derive(Clone)]
pub struct HttpClient {
    port: u16,
    password: String,
}

impl HttpClient {
    pub fn get(&self, path: &str) -> (u16, String) {
        self.request("GET", path, None)
    }

    pub fn post_json(&self, path: &str, body: &Value) -> (u16, String) {
        self.request("POST", path, Some(body.to_string()))
    }

    pub fn request(&self, method: &str, path: &str, body: Option<String>) -> (u16, String) {
        let addr = ("127.0.0.1", self.port)
            .to_socket_addrs()
            .expect("addr")
            .next()
            .expect("addr");
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("daemon connect");
        stream
            .set_read_timeout(Some(HTTP_DEADLINE))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(HTTP_DEADLINE))
            .expect("write timeout");
        let body_len = body.as_ref().map(|b| b.len()).unwrap_or(0);
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{}\r\nauthorization: Bearer {}\r\nconnection: close\r\n",
            self.port, self.password
        );
        if body.is_some() {
            request.push_str("content-type: application/json\r\n");
        }
        request.push_str(&format!("content-length: {body_len}\r\n\r\n"));
        if let Some(body) = &body {
            request.push_str(body);
        }
        stream.write_all(request.as_bytes()).expect("request write");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response read");
        parse_response(&response)
    }
}

fn parse_response(raw: &[u8]) -> (u16, String) {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let lower = head.to_ascii_lowercase();
    let body = &raw[head_end..];
    if lower.contains("transfer-encoding: chunked") {
        return (status, decode_chunked(body));
    }
    if let Some(pos) = lower.find("content-length:") {
        let rest = &lower[pos + "content-length:".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(len) = digits.parse::<usize>() {
            let end = len.min(body.len());
            return (status, String::from_utf8_lossy(&body[..end]).to_string());
        }
    }
    (status, String::from_utf8_lossy(body).to_string())
}

fn decode_chunked(mut body: &[u8]) -> String {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| p + 2)
            .unwrap_or(body.len());
        let header = String::from_utf8_lossy(&body[..line_end]).to_string();
        let size_str = header.trim().split(';').next().unwrap_or("");
        let Ok(size) = usize::from_str_radix(size_str, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        body = &body[line_end..];
        let take = size.min(body.len());
        out.extend_from_slice(&body[..take]);
        body = &body[take..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// One line of `ps -ax -o pid=,ppid=,command=`.
#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub pid: i32,
    pub ppid: i32,
    pub command: String,
}

/// The current process table (unix `ps`; bounded, never hangs).
pub fn process_table() -> Vec<ProcessRow> {
    let out = Command::new("ps")
        .args(["-axww", "-o", "pid=", "-o", "ppid=", "-o", "command="])
        .output()
        .expect("ps must run");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim_start().splitn(3, ' ');
            let pid = parts.next()?.trim().parse::<i32>().ok()?;
            let ppid = parts.next()?.trim().parse::<i32>().ok()?;
            let command = parts.next().unwrap_or("").to_string();
            Some(ProcessRow { pid, ppid, command })
        })
        .collect()
}

/// Every live process whose command mentions `token` (our unique temp
/// path fragment): the orphan audit.
pub fn processes_mentioning(token: &str) -> Vec<ProcessRow> {
    process_table()
        .into_iter()
        .filter(|row| row.command.contains(token))
        .collect()
}

/// The direct children of `pid` in the current process table — captured
/// BEFORE shutdown so the post-shutdown scan can prove none survived.
pub fn child_pids(pid: i32) -> Vec<i32> {
    process_table()
        .into_iter()
        .filter(|row| row.ppid == pid)
        .map(|row| row.pid)
        .collect()
}

/// Durable budget-ledger + provider-call accounting, read straight from
/// the store database (read-only, never the SUT's own verdict API).
#[derive(Debug, Default)]
pub struct LedgerSnapshot {
    pub reservations_total: u64,
    pub reservations_by_status: Vec<(String, u64)>,
    pub settled_rows: u64,
    /// Attempts with more than one reservation row (must be empty).
    pub duplicate_reservation_attempts: Vec<i64>,
    /// Attempts with more than one SETTLED row (must be empty).
    pub double_settled_attempts: Vec<i64>,
    pub provider_calls_total: u64,
    /// Rows carrying an `attempt_op_id` — the attempt-aware ledger, one
    /// row per PHYSICAL attempt.
    pub attempt_rows_total: u64,
    /// Rows without attempt identity: op-scoped usage annotations the
    /// session actor writes alongside a COMPLETED call (never a second
    /// charge — settlement lives in `cost_reservation`).
    pub legacy_usage_rows: Vec<String>,
    /// The attempt-identified rows ONLY (status, error) — the physical
    /// attempt ledger, one row per dispatch.
    pub attempt_rows: Vec<(String, Option<String>)>,
    pub provider_call_errors: Vec<(String, Option<String>)>,
    /// Attempts with more than one provider_call row (must be empty).
    pub duplicate_provider_call_attempts: Vec<i64>,
}

pub fn read_ledger(data_dir: &Path) -> Result<LedgerSnapshot, String> {
    let db = data_dir.join("store").join("faktor-plus.db");
    if !db.exists() {
        return Err(format!("store database missing at {}", db.display()));
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open store read-only: {e}"))?;
    let mut snap = LedgerSnapshot::default();
    {
        let mut stmt = conn
            .prepare(
                "SELECT status, COUNT(*) FROM cost_reservation GROUP BY status ORDER BY status",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (status, count) = row.map_err(|e| e.to_string())?;
            snap.reservations_total += count as u64;
            if status == "settled" {
                snap.settled_rows += count as u64;
            }
            snap.reservations_by_status.push((status, count as u64));
        }
    }
    {
        let mut stmt = conn
            .prepare(
                "SELECT attempt_op_id, COUNT(*), SUM(CASE WHEN status = 'settled' THEN 1 ELSE 0 END) \
                 FROM cost_reservation WHERE attempt_op_id IS NOT NULL \
                 GROUP BY attempt_op_id HAVING COUNT(*) > 1 OR SUM(CASE WHEN status = 'settled' THEN 1 ELSE 0 END) > 1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        for row in rows {
            let attempt = row.map_err(|e| e.to_string())?;
            snap.duplicate_reservation_attempts.push(attempt);
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM provider_call")
            .map_err(|e| e.to_string())?;
        snap.provider_calls_total = stmt
            .query_row([], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())? as u64;
    }
    {
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM provider_call WHERE attempt_op_id IS NOT NULL")
            .map_err(|e| e.to_string())?;
        snap.attempt_rows_total = stmt
            .query_row([], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())? as u64;
    }
    {
        let mut stmt = conn
            .prepare("SELECT status FROM provider_call WHERE attempt_op_id IS NULL ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for row in rows {
            snap.legacy_usage_rows.push(row.map_err(|e| e.to_string())?);
        }
    }
    {
        let mut stmt = conn
            .prepare(
                "SELECT status, error FROM provider_call WHERE attempt_op_id IS NOT NULL \
                 ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (status, error) = row.map_err(|e| e.to_string())?;
            snap.attempt_rows.push((status, error));
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT status, error FROM provider_call ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (status, error) = row.map_err(|e| e.to_string())?;
            snap.provider_call_errors.push((status, error));
        }
    }
    {
        let mut stmt = conn
            .prepare(
                "SELECT attempt_op_id FROM provider_call WHERE attempt_op_id IS NOT NULL \
                 GROUP BY attempt_op_id HAVING COUNT(*) > 1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        for row in rows {
            let attempt = row.map_err(|e| e.to_string())?;
            snap.duplicate_provider_call_attempts.push(attempt);
        }
    }
    Ok(snap)
}

/// The daemon config fixture: one OpenAI-compatible loopback provider
/// pointed at the CHAOS PROXY, priced (Economy admits it), with a fast,
/// bounded retry policy so retry assertions stay well under the
/// scenario deadline.
pub fn write_config(path: &Path, proxy_port: u16, model: &str) {
    let config = serde_json::json!({
        "config_version": 1,
        "model": model,
        "providers": [{
            "kind": "open_ai",
            "id": "chaos-p",
            "base_url": format!("http://127.0.0.1:{proxy_port}/v1"),
            "api_key_env": "CHAOS_MOCK_KEY",
            "api": "chat",
            "allow_loopback": true,
            "models": [model],
            "pricing": { "pricing_ceiling_micro_usd_per_million_tokens": 1000000 },
            "quality": {
                "coding_reliability": 90,
                "context_reliability": 90,
                "tool_reliability": 90,
                "reasoning_reliability": 88
            }
        }],
        // The daemon's egress is default-deny: exactly this loopback
        // proxy destination is allowlisted (the fault boundary itself).
        "sandbox": { "network": [format!("http://127.0.0.1:{proxy_port}")] },
        "retry": {
            "max_attempts": 2,
            "base_delay_ms": 50,
            "max_delay_ms": 200,
            "jitter": 0.0,
            "class": "rate_limited"
        }
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&config).expect("config json"),
    )
    .expect("write config");
}

/// The LIVE daemon config fixture: an OpenAI-compatible endpoint pointed at
/// the chaos proxy, the real DeepSeek model admitted through an explicit
/// pricing ceiling (a custom endpoint bills at an Unknown origin until a
/// ceiling is declared — see docs/deepseek-live.md), and retry capped at
/// ONE attempt so the live run can never burn a second paid completion.
pub fn write_live_config(path: &Path, proxy_port: u16, model: &str) {
    let config = serde_json::json!({
        "config_version": 1,
        "model": model,
        "providers": [{
            "kind": "open_ai",
            "id": "live-chaos-p",
            "base_url": format!("http://127.0.0.1:{proxy_port}/v1"),
            "api_key_env": "FAKTOR_DEEPSEEK_API_KEY",
            "api": "chat",
            "allow_loopback": true,
            "models": [model],
            "pricing": { "pricing_ceiling_micro_usd_per_million_tokens": 4_000_000 },
            // A custom endpoint does not inherit the official endpoint's
            // routing prior: the instance-level quality declaration is the
            // documented way to authorize the quality floor here.
            "quality": {
                "coding_reliability": 90,
                "context_reliability": 90,
                "tool_reliability": 90,
                "reasoning_reliability": 88
            }
        }],
        // Default-deny egress: exactly this loopback proxy destination is
        // allowlisted (the fault boundary itself).
        "sandbox": { "network": [format!("http://127.0.0.1:{proxy_port}")] },
        "retry": {
            "max_attempts": 1,
            "base_delay_ms": 50,
            "max_delay_ms": 100,
            "jitter": 0.0,
            "class": "rate_limited"
        }
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&config).expect("config json"),
    )
    .expect("write config");
}

/// Parse an operator env file of plain `KEY=VALUE` lines (optional
/// `export ` prefix, optional matching quotes, full-line `#` comments) —
/// the same documented shape the daemon's `provider_env_file` accepts.
/// Returns Err with the offending line number, never the value.
pub fn read_env_file(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let body = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = body.split_once('=') else {
            return Err(format!("line {}: expected KEY=VALUE", index + 1));
        };
        let key = key.trim();
        if key.is_empty()
            || !key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("line {}: invalid variable name", index + 1));
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    // TEST-SEPARATION: oracle=independent-reference isolation=pure-unit
    use super::*;

    #[test]
    fn startup_line_parses_only_the_frozen_contract() {
        assert_eq!(
            parse_startup_line("faktor server listening on http://127.0.0.1:4242"),
            Some(4242)
        );
        assert_eq!(
            parse_startup_line("faktor server listening on http://[::1]:1"),
            None
        );
        assert_eq!(parse_startup_line("random noise"), None);
    }

    #[test]
    fn http_response_parser_handles_length_chunked_and_close_delimited() {
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n{\"a\":1}");
        assert_eq!(status, 200);
        assert_eq!(body, "{\"a\":1}");
        let (status, body) = parse_response(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(body, "hello");
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nraw-until-eof");
        assert_eq!(status, 200);
        assert_eq!(body, "raw-until-eof");
    }

    #[test]
    fn cli_binary_is_discoverable() {
        let bin = cli_binary();
        assert!(bin.exists(), "faktor-cli must exist at {}", bin.display());
    }
}
