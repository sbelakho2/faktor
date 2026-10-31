//! HAR-01: the REAL `faktor-cli` daemon talks to a provider THROUGH the
//! local chaos proxy; each fault class is injected deterministically and
//! asserted against the provider transport contract.
//!
//! ## Harness design
//!
//! ```text
//! faktor-cli serve (temp data-dir, frozen startup line, Bearer auth)
//!        │ native HTTP (bounded deadlines)
//!        ▼
//!   chaos proxy ── Tcp upstream ──► in-repo mock OpenAI SSE provider
//!        │                          (offline runs; records attempts)
//!        └─ LiveHttps (curl leg) ─► https://api.deepseek.com (live run)
//! ```
//!
//! The live leg exists because the production egress explicitly disables
//! every environment proxy (`.no_proxy()`) and the daemon config carries no
//! per-provider proxy/CA surface: the daemon speaks cleartext HTTP to the
//! loopback proxy, and the proxy bridges the TLS hop with the system
//! `curl` (config on stdin — no credential in argv). The injected fault
//! still governs every byte the DAEMON observes, which is the transport
//! boundary under audit.
//!
//! Turn outcomes are read from the DURABLE TURN RECORD
//! (`GET /native/session/{id}/turns` — "status is the completion
//! authority"), never inferred from a UI label.
//!
//! ## Fault × assertion matrix
//!
//! | fault class       | spec            | contract expectation            | attempts (max=2) | turn status |
//! |-------------------|-----------------|---------------------------------|------------------|-------------|
//! | kill_mid_sse      | `kill_mid_sse:60`      | body ends pre-finish → typed `Malformed`, NOT retryable | 1 | failed |
//! | byte truncation   | `truncate:60`          | clean early EOF → typed `Malformed`, NOT retryable | 1 | failed |
//! | frame reorder     | `reorder:5`            | `data:` line no longer valid JSON → typed `Malformed`, NOT retryable | 1 | failed |
//! | gzip truncate     | `gzip_truncate:40`     | non-UTF-8 body → typed `Malformed`, NOT retryable | 1 | failed |
//! | RST (persistent)  | `rst_after:60`         | transport reset → retryable `Network`, bounded by `max_attempts` | 2 | failed |
//! | RST (one-shot)    | `rst_after:60:0:1`     | retryable `Network`, second attempt recovers → turn completes | 2 | completed |
//! | delay burst       | `delay_burst:80:500`   | delay inside the stream deadlines → clean completion | 1 | completed |
//!
//! EVERY scenario additionally asserts: bounded wall-clock (never a hang),
//! `/native/ready` still `true` after the fault, budget-ledger rows are
//! exactly-once (no attempt with two reservations, no attempt settled
//! twice, settled rows ≤ physical completions, `provider_call` rows ==
//! harness-counted chat attempts — attempt-identified rows only), and zero orphan processes after
//! shutdown (child pids captured before SIGTERM + temp-path token scan).
//!
//! Coverage note (live run): gated on `~/.faktor-live/deepseek.env`; when
//! absent the test records a skip note and costs nothing; when present it
//! performs exactly ONE chat completion request (`retry.max_attempts = 1`)
//! with a one-shot kill-mid-SSE fault.
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process,isolated-data-dir

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::daemon::{
    child_pids, process_table, processes_mentioning, read_env_file, read_ledger, write_config,
    write_live_config, Daemon, HttpClient, LedgerSnapshot,
};
use crate::mock::MockProvider;
use crate::proxy::{ChaosProxy, FaultSpec, ProxyStats, Upstream};

const MODEL: &str = "m";
const PROVIDER: &str = "chaos-p";
const LIVE_MODEL: &str = "deepseek-chat";
const LIVE_PROVIDER: &str = "live-chaos-p";
/// The full contract vocabulary of `ProviderErrorKind` (independent of the
/// SUT observation; see `crates/provider/src/lib.rs`).
const TYPED_KINDS: [&str; 9] = [
    "Network",
    "Timeout",
    "RateLimited",
    "BadRequest",
    "Auth",
    "Server",
    "Cancelled",
    "Malformed",
    "OutputLimitExceeded",
];

static SUBMISSION_SEQ: AtomicU64 = AtomicU64::new(1);

fn receipt_op(receipt: &Value) -> String {
    let raw = &receipt["op_id"];
    raw.as_str()
        .map(String::from)
        .or_else(|| raw.as_u64().map(|n| n.to_string()))
        .unwrap_or_else(|| raw.to_string())
}

fn submission_id() -> String {
    // 1..=64 ASCII [0-9a-f-] — hex segments only.
    let n = SUBMISSION_SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{n:08x}-0000-4000-8000-{n:012x}")
}

/// One offline scenario: mock + chaos proxy + REAL daemon in a temp dir.
struct OfflineScenario {
    _temp: tempfile::TempDir,
    token: String,
    data_dir: PathBuf,
    mock: MockProvider,
    proxy: ChaosProxy,
    daemon: Daemon,
    started: Instant,
}

impl OfflineScenario {
    fn start(spec: FaultSpec) -> Self {
        let temp = tempfile::tempdir().expect("hermetic temp dir");
        let token = temp
            .path()
            .file_name()
            .expect("temp name")
            .to_string_lossy()
            .to_string();
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let mock = MockProvider::start();
        let proxy = ChaosProxy::start(Upstream::Tcp(mock.addr), spec);
        let config = temp.path().join("config.json");
        write_config(&config, proxy.addr.port(), MODEL);
        let daemon = Daemon::start(
            &data_dir,
            &config,
            &token,
            &[(
                "CHAOS_MOCK_KEY".to_string(),
                // High-entropy placeholder: the egress secret scan blocks
                // bodies containing a CONFIGURED SECRET value, so the
                // mock key must never be a dictionary word that could
                // appear in ordinary prompt/context text.
                "c5a1f0e97b3d8462a1c9e05f7b2d3148".to_string(),
            )],
        );
        daemon
            .wait_ready(Duration::from_secs(40))
            .expect("daemon ready before any fault");
        Self {
            _temp: temp,
            token,
            data_dir,
            mock,
            proxy,
            daemon,
            started: Instant::now(),
        }
    }

    fn http(&self) -> HttpClient {
        self.daemon.http()
    }

    fn create_session(&self, provider: &str, model: &str) -> String {
        let (status, body) = self.http().post_json(
            "/native/session",
            &serde_json::json!({ "provider": provider, "model": model }),
        );
        assert_eq!(status, 200, "session create must be admitted: {body}");
        let parsed: Value = serde_json::from_str(&body).expect("session create json");
        let id = parsed["id"]
            .as_u64()
            .map(|n| n.to_string())
            .or_else(|| parsed["id"].as_str().map(String::from))
            .expect("session id in create receipt");
        assert!(
            !id.is_empty() && id != "0",
            "session id must be real: {parsed}"
        );
        id
    }

    /// Submit the prompt; returns the admission receipt (carries the turn
    /// op id — the identity the durable turn record is keyed by).
    fn prompt(&self, session: &str, text: &str) -> Value {
        let (status, body) = self.http().post_json(
            &format!("/native/session/{session}/prompt"),
            &serde_json::json!({
                "session_id": session,
                "submission_id": submission_id(),
                "prompt": text,
                "execution": "task",
            }),
        );
        assert_eq!(status, 200, "prompt admission must succeed: {body}");
        let parsed: Value = serde_json::from_str(&body).expect("prompt receipt json");
        assert!(
            parsed["accepted"].as_bool().unwrap_or(false),
            "prompt must be accepted: {parsed}"
        );
        let op = receipt_op(&parsed);
        assert!(
            !op.is_empty() && op != "null",
            "receipt must name the turn op: {parsed}"
        );
        parsed
    }

    fn turn_records(&self, session: &str) -> Value {
        let (status, body) = self.http().get(&format!("/native/session/{session}/turns"));
        assert_eq!(status, 200, "turn records must be readable: {body}");
        serde_json::from_str(&body).expect("turn records json")
    }

    /// Poll the durable turn record until its status leaves `active` —
    /// the completion authority, with a hard deadline (never a hang).
    fn await_turn_terminal(&self, session: &str, op_id: &str, deadline: Duration) -> Value {
        let started = Instant::now();
        loop {
            let records = self.turn_records(session);
            if let Some(turn) = records
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["opId"].as_str() == Some(op_id)))
            {
                let status = turn["status"].as_str().unwrap_or("");
                if status != "active" && !status.is_empty() {
                    return turn.clone();
                }
            }
            if started.elapsed() > deadline {
                panic!(
                    "turn {op_id} never left the active record within {deadline:?} \
                     (bounded, never a hang); turns={records}; stderr tail={}",
                    tail(&self.daemon.stderr_text(), 4000),
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn assert_ready_after_fault(&self) {
        let (status, body) = self.http().get("/native/ready");
        assert_eq!(status, 200, "daemon must answer after the fault: {body}");
        let ready = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["ready"].as_bool())
            .unwrap_or(false);
        assert!(ready, "daemon must be READY after the fault: {body}");
    }

    fn ledger(&self) -> LedgerSnapshot {
        read_ledger(&self.data_dir).expect("ledger snapshot")
    }

    /// SIGTERM the daemon, then prove BOTH orphan checks: no pre-shutdown
    /// child survived, and no process anywhere still references our unique
    /// temp path token.
    fn audit_shutdown(&mut self, watched_children: &[i32]) {
        self.daemon.shutdown();
        let table = process_table();
        let survivors: Vec<&crate::daemon::ProcessRow> = table
            .iter()
            .filter(|row| watched_children.contains(&row.pid))
            .collect();
        assert!(
            survivors.is_empty(),
            "orphan child processes survived daemon shutdown: {survivors:?}"
        );
        let stray = processes_mentioning(&self.token);
        assert!(
            stray.is_empty(),
            "processes still reference the scenario temp path {}: {stray:?}",
            self.token
        );
        self.mock.shutdown();
        self.proxy.shutdown();
    }

    fn assert_bounded(&self) {
        assert!(
            self.started.elapsed() < crate::daemon::SCENARIO_DEADLINE,
            "scenario exceeded the bounded wall-clock deadline: {:?}",
            self.started.elapsed()
        );
    }
}

fn tail(text: &str, bytes: usize) -> String {
    let len = text.len();
    if len <= bytes {
        text.to_string()
    } else {
        text[len - bytes..].to_string()
    }
}

/// The durable typed-error oracle: `provider_call` rows carry the
/// per-attempt `ProviderError` display (`<Kind>: message`). Every FAILED
/// row must name a contract kind; at least one must name `expected`.
fn assert_typed_provider_failure(ledger: &LedgerSnapshot, expected: &[&str]) {
    let failures: Vec<&(String, Option<String>)> = ledger
        .provider_call_errors
        .iter()
        .filter(|(status, _)| status == "failed")
        .collect();
    assert!(
        !failures.is_empty(),
        "a faulted run must leave a durable FAILED provider_call row; rows={:?}",
        ledger.provider_call_errors
    );
    for (_, error) in &failures {
        let text = error.as_deref().unwrap_or("");
        assert!(
            TYPED_KINDS.iter().any(|kind| text.contains(kind)),
            "failed provider_call must name a contract kind, got: {text:?}"
        );
    }
    assert!(
        failures.iter().any(|(_, error)| expected
            .iter()
            .any(|kind| error.as_deref().is_some_and(|text| text.contains(kind)))),
        "expected one of {expected:?} among failed rows: {:?}",
        failures
    );
}

/// Exactly-once charge accounting: the harness's own attempt count (mock
/// or proxy) against the durable ledger.
fn assert_no_duplicate_charge(ledger: &LedgerSnapshot, chat_attempts: u64) {
    assert!(
        ledger.duplicate_reservation_attempts.is_empty(),
        "an attempt must own exactly one reservation row: {:?}",
        ledger.duplicate_reservation_attempts
    );
    assert!(
        ledger.double_settled_attempts.is_empty(),
        "an attempt must never settle twice: {:?}",
        ledger.double_settled_attempts
    );
    assert!(
        ledger.duplicate_provider_call_attempts.is_empty(),
        "an attempt must own exactly one provider_call row: {:?}",
        ledger.duplicate_provider_call_attempts
    );
    assert_eq!(
        ledger.attempt_rows_total, chat_attempts,
        "physical attempts ({chat_attempts}) and ATTEMPT-IDENTIFIED provider_call rows must \
         match 1:1 (op-scoped usage annotations carry no attempt identity); rows={:?}",
        ledger.provider_call_errors
    );
    assert!(
        ledger
            .legacy_usage_rows
            .iter()
            .all(|status| status == "completed"),
        "a usage annotation may only ever sit beside a COMPLETED call: {:?}",
        ledger.legacy_usage_rows
    );
    assert!(
        ledger.legacy_usage_rows.len() as u64 <= chat_attempts,
        "usage annotations can never outnumber physical attempts"
    );
    assert!(
        ledger.settled_rows <= chat_attempts,
        "settled charges ({}) can never exceed dispatched completions ({chat_attempts})",
        ledger.settled_rows
    );
}

fn turn_status(turn: &Value) -> &str {
    turn["status"].as_str().unwrap_or("<missing>")
}

// ---------------------------------------------------------------------------
// Offline fault classes
// ---------------------------------------------------------------------------

#[test]
fn har01_kill_mid_sse_fails_typed_without_burning_retries() {
    // Spec `kill_mid_sse:60`: drop the connection 60 BODY bytes into the
    // SSE stream — mid first frame, before any accepted text. The adapter
    // observes a body that ended before finish_reason/[DONE]: a typed
    // Malformed error, which is NOT retryable, so exactly ONE physical
    // attempt may reach the mock.
    let spec = FaultSpec::parse("kill_mid_sse:60").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(
        attempts, 1,
        "Malformed is not retryable: the retry contract forbids a second attempt"
    );
    assert_eq!(
        turn_status(&turn),
        "failed",
        "the truncated stream must fail the turn: {turn}"
    );
    let ledger = scenario.ledger();
    assert_typed_provider_failure(&ledger, &["Malformed"]);
    assert_no_duplicate_charge(&ledger, attempts);
    assert_eq!(
        scenario.proxy.stats.faults_fired(),
        1,
        "the armed kill must have fired exactly once"
    );
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

#[test]
fn har01_byte_truncation_fails_typed_without_burning_retries() {
    // Spec `truncate:60`: clean write-side FIN after 60 body bytes — the
    // HTTP layer sees a complete (close-delimited) body whose SSE frame is
    // cut: typed Malformed, not retryable, one attempt.
    let spec = FaultSpec::parse("truncate:60").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(attempts, 1, "truncation is typed Malformed: no retry");
    assert_eq!(turn_status(&turn), "failed", "turn must fail: {turn}");
    let ledger = scenario.ledger();
    assert_typed_provider_failure(&ledger, &["Malformed"]);
    assert_no_duplicate_charge(&ledger, attempts);
    assert_eq!(scenario.proxy.stats.faults_fired(), 1);
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

#[test]
fn har01_frame_reorder_fails_typed_without_burning_retries() {
    // Spec `reorder:5`: the body's last 5 bytes (`NE]\n\n` — a cut INSIDE
    // the terminal `data: [DONE]` frame) swap to the front. The stream's
    // trailing `data:` line is no longer valid JSON: typed Malformed, not
    // retryable, one attempt.
    let spec = FaultSpec::parse("reorder:5").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(
        attempts, 1,
        "a reordered stream is typed Malformed: no retry"
    );
    assert_eq!(turn_status(&turn), "failed", "turn must fail: {turn}");
    let ledger = scenario.ledger();
    assert_typed_provider_failure(&ledger, &["Malformed"]);
    assert_no_duplicate_charge(&ledger, attempts);
    assert_eq!(scenario.proxy.stats.faults_fired(), 1);
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

#[test]
fn har01_gzip_truncated_body_fails_typed_without_burning_retries() {
    // Spec `gzip_truncate:40`: the body is re-encoded as gzip and cut at
    // 40 bytes — non-UTF-8 bytes reach the SSE line decoder: typed
    // Malformed, not retryable, one attempt.
    let spec = FaultSpec::parse("gzip_truncate:40").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(attempts, 1, "gzip garbage is typed Malformed: no retry");
    assert_eq!(turn_status(&turn), "failed", "turn must fail: {turn}");
    let ledger = scenario.ledger();
    assert_typed_provider_failure(&ledger, &["Malformed"]);
    assert_no_duplicate_charge(&ledger, attempts);
    assert_eq!(scenario.proxy.stats.faults_fired(), 1);
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

#[test]
fn har01_rst_persistent_fault_respects_the_retry_budget() {
    // Spec `rst_after:60` (fires on EVERY connection): the transport sees
    // a reset — a retryable `Network` error — and retries within the
    // configured budget (max_attempts = 2), then fails typed. The attempt
    // count must equal the budget exactly: no infinite loop, no stall.
    let spec = FaultSpec::parse("rst_after:60").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(
        attempts, 2,
        "Network is retryable: exactly max_attempts (2) physical attempts, bounded"
    );
    assert_eq!(
        turn_status(&turn),
        "failed",
        "persistent resets must fail the turn: {turn}"
    );
    let ledger = scenario.ledger();
    assert_typed_provider_failure(&ledger, &["Network"]);
    assert_no_duplicate_charge(&ledger, attempts);
    assert!(
        scenario.proxy.stats.faults_fired() >= 1,
        "the armed RST must have fired"
    );
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

#[test]
fn har01_rst_one_shot_fault_recovers_on_retry() {
    // Spec `rst_after:60:0:1` (fires ONCE): attempt 1 resets → retryable
    // Network → attempt 2 passes through healthy → the turn completes.
    // Recovery per the transport contract, with every charge accounted.
    let spec = FaultSpec::parse("rst_after:60:0:1").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(
        attempts, 2,
        "one-shot reset: retry exactly once, then recover"
    );
    assert_eq!(
        turn_status(&turn),
        "completed",
        "the recovered attempt must complete the turn: {turn}"
    );
    let ledger = scenario.ledger();
    // Attempt 1 failed typed (Network) AND attempt 2 completed — both
    // ATTEMPT rows durable, neither duplicated (the success also writes an
    // op-scoped usage annotation, which carries no attempt identity).
    let (failed, completed) =
        ledger
            .attempt_rows
            .iter()
            .fold(
                (0u64, 0u64),
                |(failed, completed), (status, _)| match status.as_str() {
                    "failed" => (failed + 1, completed),
                    "completed" => (failed, completed + 1),
                    _ => (failed, completed),
                },
            );
    assert_eq!(
        failed, 1,
        "the reset attempt fails exactly once: {:?}",
        ledger.provider_call_errors
    );
    assert_eq!(
        completed, 1,
        "the recovered attempt completes exactly once: {:?}",
        ledger.attempt_rows
    );
    assert_typed_provider_failure(&ledger, &["Network"]);
    assert_no_duplicate_charge(&ledger, attempts);
    assert_eq!(
        scenario.proxy.stats.faults_fired(),
        1,
        "exactly one fire (one-shot budget consumed)"
    );
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

#[test]
fn har01_delay_burst_within_deadlines_completes_clean() {
    // Spec `delay_burst:80:500`: one 500 ms stall after 80 body bytes —
    // inside every stream deadline. The contract outcome is a CLEAN
    // completion (no error, one attempt), proving transient latency alone
    // never becomes a failure or a retry storm.
    let spec = FaultSpec::parse("delay_burst:80:500").expect("spec");
    let mut scenario = OfflineScenario::start(spec);
    let session = scenario.create_session(PROVIDER, MODEL);
    let children = child_pids(scenario.daemon.pid());
    let receipt = scenario.prompt(&session, "Reply with exactly: pong");
    let op = receipt_op(&receipt);
    let turn = scenario.await_turn_terminal(&session, &op, Duration::from_secs(45));
    scenario.assert_ready_after_fault();

    let attempts = scenario.mock.chat_attempts();
    assert_eq!(attempts, 1, "an in-deadline delay is not retried");
    assert_eq!(
        turn_status(&turn),
        "completed",
        "the delayed stream must complete the turn: {turn}"
    );
    let ledger = scenario.ledger();
    let failures = ledger
        .provider_call_errors
        .iter()
        .filter(|(status, _)| status == "failed")
        .count();
    assert_eq!(
        failures, 0,
        "a delay within deadlines must leave no failed provider_call: {:?}",
        ledger.provider_call_errors
    );
    assert!(
        ledger
            .provider_call_errors
            .iter()
            .any(|(status, _)| status == "completed"),
        "the completion must be durably recorded: {:?}",
        ledger.provider_call_errors
    );
    assert_no_duplicate_charge(&ledger, attempts);
    assert_eq!(
        scenario.proxy.stats.faults_fired(),
        1,
        "the delay burst fired once"
    );
    scenario.assert_bounded();
    scenario.audit_shutdown(&children);
}

// ---------------------------------------------------------------------------
// Live run (gated, exactly ONE paid completion)
// ---------------------------------------------------------------------------

/// The operator's live env file — expanded from `$HOME`, never the daemon's
/// own data dir, never written.
fn live_env_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".faktor-live/deepseek.env");
    path.exists().then_some(path)
}

/// LIVE: one short completion against the REAL DeepSeek endpoint through
/// the chaos proxy with a one-shot kill-mid-SSE fault. Gated on the
/// operator env file; absent → recorded skip note (never a failure).
/// Present → `retry.max_attempts = 1` plus the proxy's one-shot fire
/// budget guarantee a SINGLE provider request.
#[ignore = "[live] real DeepSeek completion through the chaos proxy (needs ~/.faktor-live/deepseek.env); run explicitly with `cargo test -p faktor-tests-chaos -- --ignored`"]
#[test]
fn har01_real_deepseek_kill_mid_stream_is_typed_and_single_charge() {
    let Some(env_path) = live_env_path() else {
        println!(
            "SKIPPED (recorded note): ~/.faktor-live/deepseek.env absent — \
             no live run, no network, zero cost"
        );
        return;
    };
    let entries = read_env_file(&env_path).expect("live env file parses as KEY=VALUE");
    assert!(
        entries
            .iter()
            .any(|(key, value)| key == "FAKTOR_DEEPSEEK_API_KEY" && !value.is_empty()),
        "the live env file must carry FAKTOR_DEEPSEEK_API_KEY"
    );

    let temp = tempfile::tempdir().expect("hermetic temp dir");
    let token = temp
        .path()
        .file_name()
        .expect("temp name")
        .to_string_lossy()
        .to_string();
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    // No mock: the upstream is the REAL endpoint through the TLS bridge.
    let spec = FaultSpec::parse("kill_mid_sse:60:0:1").expect("spec");
    let proxy = ChaosProxy::start(
        Upstream::LiveHttps {
            base: "https://api.deepseek.com".to_string(),
            work_dir: temp.path().join("curl-work"),
        },
        spec,
    );
    let config = temp.path().join("config.json");
    write_live_config(&config, proxy.addr.port(), LIVE_MODEL);
    let daemon = Daemon::start(&data_dir, &config, &token, &entries);
    daemon
        .wait_ready(Duration::from_secs(40))
        .expect("daemon ready before the live run");
    let http = daemon.http();
    let started = Instant::now();

    let (status, body) = http.post_json(
        "/native/session",
        &serde_json::json!({ "provider": LIVE_PROVIDER, "model": LIVE_MODEL }),
    );
    assert_eq!(status, 200, "live session create must be admitted: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("session json");
    let session = parsed["id"]
        .as_u64()
        .map(|n| n.to_string())
        .or_else(|| parsed["id"].as_str().map(String::from))
        .expect("session id");

    let children = child_pids(daemon.pid());
    let (status, body) = http.post_json(
        &format!("/native/session/{session}/prompt"),
        &serde_json::json!({
            "session_id": session,
            "submission_id": submission_id(),
            "prompt": "Reply with exactly: pong",
            "execution": "task",
        }),
    );
    assert_eq!(status, 200, "live prompt admission must succeed: {body}");
    let receipt: Value = serde_json::from_str(&body).expect("receipt json");
    let op = receipt_op(&receipt);

    // Bounded terminal wait on the durable turn record — a live network
    // stall fails, never hangs.
    let terminal_deadline = Instant::now() + Duration::from_secs(90);
    let mut turn = Value::Null;
    loop {
        let (status, text) = http.get(&format!("/native/session/{session}/turns"));
        if status == 200 {
            let records: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if let Some(row) = records.as_array().and_then(|rows| {
                rows.iter()
                    .find(|r| r["opId"].as_str() == Some(op.as_str()))
            }) {
                if row["status"].as_str() != Some("active") {
                    turn = row.clone();
                    break;
                }
            }
        }
        assert!(
            Instant::now() < terminal_deadline,
            "live turn never terminated within the bounded deadline; turn={turn}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // The daemon must still be healthy after the fault.
    let (status, ready_body) = http.get("/native/ready");
    assert_eq!(status, 200, "daemon must answer after the live fault");
    let live_ready = serde_json::from_str::<Value>(&ready_body)
        .ok()
        .and_then(|v| v["ready"].as_bool())
        .unwrap_or(false);
    assert!(live_ready, "ready after fault: {ready_body}");

    let stats: &ProxyStats = &proxy.stats;
    let attempts = stats.chat_requests();
    assert_eq!(
        attempts,
        1,
        "ONE paid completion: exactly one chat request may reach the real endpoint \
         (fire budget + max_attempts=1); connections={}",
        stats.connections()
    );
    assert!(
        stats.faults_fired() >= 1,
        "the kill-mid-SSE fault must fire on the real stream"
    );
    let ledger = read_ledger(&data_dir).expect("ledger snapshot");
    assert_typed_provider_failure(&ledger, &["Malformed", "Network"]);
    assert_no_duplicate_charge(&ledger, attempts);
    println!(
        "live transcript: chat_requests={attempts} faults_fired={} turn_status={} \
         elapsed={:?} ledger rows={:?}",
        stats.faults_fired(),
        turn_status(&turn),
        started.elapsed(),
        ledger.provider_call_errors
    );
    assert!(
        started.elapsed() < crate::daemon::SCENARIO_DEADLINE,
        "live scenario exceeded the bounded wall-clock"
    );

    // Orphan audit: same two checks as the offline runs.
    let mut daemon = daemon;
    daemon.shutdown();
    let table = process_table();
    let survivors: Vec<&crate::daemon::ProcessRow> = table
        .iter()
        .filter(|row| children.contains(&row.pid))
        .collect();
    assert!(survivors.is_empty(), "orphan children: {survivors:?}");
    let stray = processes_mentioning(&token);
    assert!(stray.is_empty(), "temp-path processes remain: {stray:?}");
    drop(proxy);
}
