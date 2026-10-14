//! The explicit first-run sandbox shell choice (audit item 17).
//!
//! On platforms with no per-process network-isolation backend (macOS,
//! Windows, Linux hosts whose kernel/user-namespace policy refuses the
//! unshare), the secure default REFUSES agent shell and verification spawns
//! typed. That refusal is correct — but it must never be silently worked
//! around. This module owns the ONE explicit operator choice:
//!
//! - **(a) keep fail-closed** (the default): agent shells and verification
//!   stay OS-isolation-required; on backend-less platforms they refuse
//!   typed. Nothing is written and nothing changes.
//! - **(b) run network-capable shells** (an explicit operator grant): the
//!   agent shell-tool class switches to
//!   [`ShellExecutionMode::NetworkCapableUserGranted`] with the
//!   [`SandboxGuarantee::BestEffort`] app-level-only caveat, and the
//!   decision is recorded durably (a decision document plus an append-only
//!   audit line) with the exact trade-off text shown at grant time.
//!
//! The choice is presented (never made) at startup on backend-less
//! platforms; it is made only through `faktor sandbox choose …` (or an
//! explicit `[sandbox] shell` config, which always wins and is reported as
//! the explicit-config state). A corrupt or hostile decision file is a loud
//! error and the effective state stays fail-closed — the fail-closed
//! default is never weakened by a bad record. The decision document is
//! confirmed against the append-only audit trail on every read (the exact
//! choice, timestamp, platform, backend fact and trade-off must match one
//! entry), so editing the JSON alone — a hand-forged or flipped grant —
//! grants nothing. Both files are read through hard byte bounds (the
//! decision document 64 KiB, the scanned trail 4 MiB): a hostile
//! multi-gigabyte file is refused, never pulled into memory.
//!
//! Where a real backend exists (Linux namespaces) nothing is faked: the
//! choice still records exactly what the operator asked for, and the spawn
//! layer continues to isolate children with the real backend.

use std::path::{Path, PathBuf};

use faktor_sandbox::{SandboxGuarantee, ShellExecutionMode};

/// The durable decision document under the data dir.
pub const SANDBOX_FIRST_RUN_FILE: &str = "sandbox-first-run.json";

/// The append-only audit file under the data dir (JSON lines).
pub const SANDBOX_FIRST_RUN_AUDIT_FILE: &str = "sandbox-first-run-audit.jsonl";

/// Rotation bound: the audit file holds at most this many bytes before
/// being compacted in place (the retained complete-line tail plus the new
/// entry), so the trail stays bounded without sidecar files.
const AUDIT_MAX_BYTES: u64 = 1024 * 1024;

/// Hard bound on the decision document bytes: a legitimate record is a few
/// hundred bytes; anything larger is not one and is refused WITHOUT being
/// read into memory (a hostile multi-gigabyte file must never OOM the
/// daemon at startup or in doctor).
const DECISION_MAX_BYTES: u64 = 64 * 1024;

/// Hard bound on the audit bytes scanned to confirm a record. The live
/// trail is compacted at [`AUDIT_MAX_BYTES`], so a much larger file is
/// hostile or corrupt: it is refused unread instead of being pulled
/// wholesale into memory.
const AUDIT_CONFIRM_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Schema version of the decision document; only 1 is accepted.
pub const SANDBOX_FIRST_RUN_VERSION: u32 = 1;

/// The typed first-run choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxShellChoice {
    /// Option (a): keep the fail-closed default.
    FailClosed,
    /// Option (b): the explicit network-capable shell grant.
    NetworkCapableUserGranted,
}

impl SandboxShellChoice {
    /// The stable snake_case tag (also the serde tag).
    pub const fn as_tag(self) -> &'static str {
        match self {
            SandboxShellChoice::FailClosed => "fail_closed",
            SandboxShellChoice::NetworkCapableUserGranted => "network_capable_user_granted",
        }
    }

    /// The shell-execution mode this choice resolves to: `None` keeps the
    /// crate's secure default (fail-closed).
    pub const fn shell_mode(self) -> Option<ShellExecutionMode> {
        match self {
            SandboxShellChoice::FailClosed => None,
            SandboxShellChoice::NetworkCapableUserGranted => {
                Some(ShellExecutionMode::NetworkCapableUserGranted)
            }
        }
    }

    /// The network guarantee this choice resolves to (the app-level-only
    /// `BestEffort` caveat for the grant, the secure default otherwise).
    pub const fn network_guarantee(self) -> SandboxGuarantee {
        match self {
            SandboxShellChoice::FailClosed => SandboxGuarantee::Required,
            SandboxShellChoice::NetworkCapableUserGranted => SandboxGuarantee::BestEffort,
        }
    }
}

/// The exact trade-off text recorded with a network-capable grant. Frozen:
/// the doctor line, the CLI notice and the durable record carry this same
/// sentence, so the grant can never be presented as equivalent strength.
pub const NETWORK_CAPABLE_SHELL_TRADE_OFF: &str =
    "network-capable shells (strictly weaker than the fail-closed default): agent shell and \
     verification children inherit the daemon's network namespace — a permitted shell can \
     open its own sockets and direct network access is possible; the app-level destination \
     gate still applies, but no OS-level network isolation backs this grant";

/// The durable decision document.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxFirstRunRecord {
    pub version: u32,
    pub choice: SandboxShellChoice,
    pub decided_at_ms: i64,
    /// The platform the choice was recorded on (`std::env::consts::OS`).
    pub platform: String,
    /// Whether a per-process network-isolation backend existed at decision
    /// time (the reason the choice was presented).
    pub network_backend_available: bool,
    /// The exact trade-off text shown at decision time (the frozen sentence
    /// for a grant; empty for fail-closed).
    pub trade_off: String,
}

/// One append-only audit line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SandboxFirstRunAuditEntry {
    event: String,
    choice: SandboxShellChoice,
    decided_at_ms: i64,
    platform: String,
    network_backend_available: bool,
    trade_off: String,
    pid: u32,
}

/// The effect of the first-run choice on THIS daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxFirstRunDecision {
    /// `[sandbox] shell` is set in the config file: the operator's explicit
    /// config wins and the durable record (if any) is informational only.
    ExplicitConfig,
    /// Fail-closed (the secure default): either explicitly recorded, or no
    /// choice was needed because the platform has a kernel backend.
    FailClosed,
    /// The durable explicit grant is in force.
    NetworkCapableUserGranted,
    /// A backend-less platform with no explicit choice anywhere: the typed
    /// choice must be presented; the EFFECTIVE state is fail-closed.
    Pending,
}

impl SandboxFirstRunDecision {
    pub const fn as_tag(self) -> &'static str {
        match self {
            SandboxFirstRunDecision::ExplicitConfig => "explicit_config",
            SandboxFirstRunDecision::FailClosed => "fail_closed",
            SandboxFirstRunDecision::NetworkCapableUserGranted => "network_capable_user_granted",
            SandboxFirstRunDecision::Pending => "pending",
        }
    }
}

/// The inspected first-run state of one daemon process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxFirstRunState {
    pub decision: SandboxFirstRunDecision,
    /// True when this build/platform has a per-process network-isolation
    /// backend (Linux). On other platforms the choice is presented at
    /// startup.
    pub backend_available: bool,
    /// The durable record when one exists.
    pub record: Option<SandboxFirstRunRecord>,
    /// A corrupt decision document (loud, and the state stays fail-closed).
    pub corrupt_reason: Option<String>,
}

impl SandboxFirstRunState {
    /// True when the startup notice must present the typed choice.
    pub fn pending(&self) -> bool {
        self.decision == SandboxFirstRunDecision::Pending
    }
}

/// True when THIS build/platform has a per-process network-isolation
/// backend (the same compile-time fact the terminal crate reports).
pub fn network_backend_available() -> bool {
    faktor_terminal::broker_only_supported()
}

/// The decision document path under `data_dir`.
pub fn decision_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SANDBOX_FIRST_RUN_FILE)
}

/// The audit file path under `data_dir`.
pub fn audit_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SANDBOX_FIRST_RUN_AUDIT_FILE)
}

/// Read at most `cap` bytes, refusing (never truncating) anything larger.
/// The file's own metadata is only a hint: the READ itself is capped, so a
/// stat-size-zero stream file or a file growing mid-read can never exceed
/// the bound (the bounded-read rule of audit 50e2dca).
fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut reader = file.take(cap + 1);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > cap {
        return Err(format!(
            "{}: larger than the {cap}-byte bound; refusing to read it",
            path.display()
        ));
    }
    Ok(bytes)
}

/// TRUE when `path` exists as a REGULAR file; `Ok(false)` when nothing is
/// there. Anything else (a symlink, FIFO, socket or device) is hostile
/// input at these artifact paths: refusing it typed keeps a planted FIFO
/// from blocking every daemon start and a planted symlink from redirecting
/// a read or write.
fn regular_file(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(_) => Err(format!(
            "{}: not a regular file (refusing symlinks, FIFOs, sockets and devices)",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Read the durable decision. `Ok(None)` when none exists; a corrupt,
/// oversized, non-regular or schema-mismatched document is a typed error
/// (the caller keeps the fail-closed default and surfaces the refusal).
pub fn load_decision(data_dir: &Path) -> Result<Option<SandboxFirstRunRecord>, String> {
    let path = decision_path(data_dir);
    if !regular_file(&path)? {
        return Ok(None);
    }
    let bytes = read_bounded(&path, DECISION_MAX_BYTES)?;
    let record: SandboxFirstRunRecord =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if record.version != SANDBOX_FIRST_RUN_VERSION {
        return Err(format!(
            "{}: unsupported version {} (this build accepts {SANDBOX_FIRST_RUN_VERSION})",
            path.display(),
            record.version
        ));
    }
    Ok(Some(record))
}

/// The audit event name of one choice: shared by the writer and the
/// confirmation read, so a record must be backed by the event for ITS OWN
/// choice (a flipped `choice` field can never reuse the other event).
fn audit_event(choice: SandboxShellChoice) -> &'static str {
    match choice {
        SandboxShellChoice::FailClosed => "sandbox_choice_fail_closed_recorded",
        SandboxShellChoice::NetworkCapableUserGranted => "sandbox_network_capable_shell_granted",
    }
}

/// Confirm a decision document against the append-only trail: one entry
/// must match its exact event, choice, timestamp, platform, backend fact
/// and trade-off text. A document that matches no entry is UNTRUSTED — the
/// caller keeps the fail-closed default and reports the refusal, so editing
/// the JSON alone (a hand-forged or flipped grant) can never take effect.
/// The scan is byte-bounded ([`AUDIT_CONFIRM_MAX_BYTES`]).
fn audit_confirms(data_dir: &Path, record: &SandboxFirstRunRecord) -> Result<(), String> {
    let path = audit_path(data_dir);
    let trail_problem = |detail: String| {
        format!(
            "the decision document {} has no auditable trail: {detail}; keeping the \
             fail-closed default",
            decision_path(data_dir).display()
        )
    };
    match regular_file(&path) {
        Ok(true) => {}
        Ok(false) => return Err(trail_problem(format!("{} is absent", path.display()))),
        Err(e) => return Err(trail_problem(e)),
    }
    let bytes = read_bounded(&path, AUDIT_CONFIRM_MAX_BYTES).map_err(trail_problem)?;
    let text = String::from_utf8_lossy(&bytes);
    let expected_event = audit_event(record.choice);
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<SandboxFirstRunAuditEntry>(line) else {
            continue;
        };
        if entry.event == expected_event
            && entry.choice == record.choice
            && entry.decided_at_ms == record.decided_at_ms
            && entry.platform == record.platform
            && entry.network_backend_available == record.network_backend_available
            && entry.trade_off == record.trade_off
        {
            return Ok(());
        }
    }
    Err(format!(
        "the decision document {} matches no audit entry in {} (an operator decision is \
         confirmed only by its append-only record); refusing it and keeping the fail-closed \
         default",
        decision_path(data_dir).display(),
        path.display(),
    ))
}

/// Record an explicit operator choice: one append-only audit line FIRST
/// (fsynced), then the durable decision document (atomic replace), so a
/// readable decision is always backed by its trail. Returns the stored
/// record.
pub fn record_decision(
    data_dir: &Path,
    choice: SandboxShellChoice,
    decided_at_ms: i64,
) -> Result<SandboxFirstRunRecord, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let record = SandboxFirstRunRecord {
        version: SANDBOX_FIRST_RUN_VERSION,
        choice,
        decided_at_ms,
        platform: std::env::consts::OS.to_string(),
        network_backend_available: network_backend_available(),
        trade_off: match choice {
            SandboxShellChoice::FailClosed => String::new(),
            SandboxShellChoice::NetworkCapableUserGranted => {
                NETWORK_CAPABLE_SHELL_TRADE_OFF.to_string()
            }
        },
    };
    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    // The audit line lands FIRST: a decision document is only ever read as
    // an operator decision when its append-only entry already exists, so a
    // forged document alone can never grant anything. A crash between the
    // two writes leaves an audit entry with the PREVIOUS (or no) decision
    // document — the effective state stays exactly what it was, never a
    // half grant.
    append_audit(data_dir, &record)?;
    faktor_fs::atomic::atomic_replace(&decision_path(data_dir), &bytes)
        .map_err(|e| format!("{}: {e}", decision_path(data_dir).display()))?;
    Ok(record)
}

/// Append one audit line. Past the byte bound the trail is compacted
/// through the fs-atomic authority (the retained tail of complete lines
/// plus the new entry replace the file — never a hand-rolled rename), so
/// the audit stays bounded on disk while recent history survives.
fn append_audit(data_dir: &Path, record: &SandboxFirstRunRecord) -> Result<(), String> {
    let path = audit_path(data_dir);
    let entry = SandboxFirstRunAuditEntry {
        event: audit_event(record.choice).to_string(),
        choice: record.choice,
        decided_at_ms: record.decided_at_ms,
        platform: record.platform.clone(),
        network_backend_available: record.network_backend_available,
        trade_off: record.trade_off.clone(),
        pid: std::process::id(),
    };
    let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
    regular_file(&path)?;
    let existing_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if existing_len > AUDIT_MAX_BYTES {
        // Compact through the fs-atomic authority (never a hand-rolled
        // rename): keep the most recent half-bound of complete lines plus
        // the new entry, so the trail stays bounded while recent history
        // survives. Only a BOUNDED TAIL of the file is ever read: the
        // retention window is at most half the bound plus one byte, and
        // trimming to the first newline inside it drops the possibly
        // partial leading line — a hostile oversized audit file is never
        // pulled into memory.
        use std::io::{Read as _, Seek as _, SeekFrom};
        let window = (AUDIT_MAX_BYTES / 2) as usize + 1;
        let start = existing_len.saturating_sub(window as u64);
        let mut file =
            std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut content = Vec::new();
        file.take(window as u64)
            .read_to_end(&mut content)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let cut = content
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| offset + 1)
            .unwrap_or(content.len());
        let retained = String::from_utf8_lossy(&content[cut..]);
        let mut compacted = String::with_capacity(retained.len() + line.len() + 1);
        compacted.push_str(&retained);
        compacted.push_str(&line);
        compacted.push('\n');
        faktor_fs::atomic::atomic_replace(&path, compacted.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(());
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("{}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// Inspect the first-run state WITHOUT mutating the config: reads the
/// durable record, CONFIRMS it against the append-only audit trail, and
/// combines it with the config's explicit `shell` value. A corrupt,
/// oversized or unconfirmed record yields a `Pending`/`FailClosed` state
/// with `corrupt_reason` set — never a grant, and never a record presented
/// as recorded.
pub fn inspect(data_dir: &Path, config: &crate::config::Config) -> SandboxFirstRunState {
    let backend_available = network_backend_available();
    let (record, corrupt_reason) = match load_decision(data_dir) {
        Ok(Some(record)) => match audit_confirms(data_dir, &record) {
            Ok(()) => (Some(record), None),
            Err(reason) => (None, Some(reason)),
        },
        Ok(None) => (None, None),
        Err(reason) => (None, Some(reason)),
    };
    let decision = if config.sandbox.shell.is_some() {
        // The operator's explicit config is a declaration and wins over any
        // durable record — even an unreadable one (which is still a loud
        // issue, reported alongside).
        SandboxFirstRunDecision::ExplicitConfig
    } else if corrupt_reason.is_some() {
        // Corruption never weakens the default: fail closed, loudly.
        SandboxFirstRunDecision::FailClosed
    } else {
        match (&record, backend_available) {
            (Some(record), _) => match record.choice {
                SandboxShellChoice::FailClosed => SandboxFirstRunDecision::FailClosed,
                SandboxShellChoice::NetworkCapableUserGranted => {
                    SandboxFirstRunDecision::NetworkCapableUserGranted
                }
            },
            (None, true) => SandboxFirstRunDecision::FailClosed,
            (None, false) => SandboxFirstRunDecision::Pending,
        }
    };
    SandboxFirstRunState {
        decision,
        backend_available,
        record,
        corrupt_reason,
    }
}

/// Apply the first-run choice to a config loaded by a daemon entry point:
/// an explicit `[sandbox] shell` always wins; otherwise a durable grant
/// switches the AGENT shell class to `network_capable_user_granted` with
/// the app-level-only `BestEffort` guarantee. Returns the inspected state
/// (after mutation) so callers can present it.
pub fn apply_effective_choice(
    config: &mut crate::config::Config,
    data_dir: &Path,
) -> SandboxFirstRunState {
    let state = inspect(data_dir, config);
    if state.corrupt_reason.is_some() {
        tracing::error!(
            "sandbox first-run decision is unreadable; keeping the fail-closed default: {}",
            state.corrupt_reason.as_deref().unwrap_or_default()
        );
    }
    if state.decision == SandboxFirstRunDecision::NetworkCapableUserGranted {
        // The record is only reachable when the config left `shell` unset
        // (otherwise `inspect` reports ExplicitConfig), so this mutation
        // cannot override an operator's config.
        config.sandbox.shell = SandboxShellChoice::NetworkCapableUserGranted.shell_mode();
        config.sandbox.network_guarantee =
            SandboxShellChoice::NetworkCapableUserGranted.network_guarantee();
    }
    state
}

/// The startup notice text for a state: `None` when nothing must be
/// presented. This is the typed presentation (the choice itself is only
/// made through `faktor sandbox choose …` or the config file).
pub fn startup_notice(state: &SandboxFirstRunState, data_dir: &Path) -> Option<String> {
    let data_dir_display = data_dir.display();
    if state.pending() {
        return Some(format!(
            "sandbox first-run choice REQUIRED on platform '{}' (no per-process \
             network-isolation backend): agent shell and verification commands stay \
             FAIL-CLOSED (the secure default) until an operator chooses explicitly.\n\
             \x20 (a) keep fail-closed (default, nothing to do): run `faktor sandbox choose \
             --fail-closed --data-dir {data_dir_display}` to record that decision.\n\
             \x20 (b) grant network-capable shells: run `faktor sandbox choose \
             --network-capable-shell --data-dir {data_dir_display}` after reading the trade-off:\n\
             \x20     {NETWORK_CAPABLE_SHELL_TRADE_OFF}",
            std::env::consts::OS,
        ));
    }
    match state.decision {
        SandboxFirstRunDecision::NetworkCapableUserGranted => Some(format!(
            "sandbox first-run choice: the operator GRANTED network-capable shells \
             (recorded at {}); {NETWORK_CAPABLE_SHELL_TRADE_OFF}",
            decision_path(data_dir).display(),
        )),
        SandboxFirstRunDecision::ExplicitConfig => Some(
            "sandbox shell contract comes from the explicit [sandbox] shell config key \
             (config wins over any durable first-run record)"
                .to_string(),
        ),
        SandboxFirstRunDecision::FailClosed => state.corrupt_reason.as_ref().map(|reason| {
            format!(
                "sandbox first-run decision is UNREADABLE ({reason}); the fail-closed default \
                 remains in force — fix or remove the decision file"
            )
        }),
        // Unreachable: `pending()` returned above; kept total for the type.
        SandboxFirstRunDecision::Pending => None,
    }
}

/// Present the startup notice on stderr and in the log (never stdout: the
/// daemon's stdout carries the frozen startup line). A no-op when there is
/// nothing to present.
pub fn announce_startup(data_dir: &Path, config: &crate::config::Config) {
    let state = inspect(data_dir, config);
    if let Some(text) = startup_notice(&state, data_dir) {
        match state.decision {
            SandboxFirstRunDecision::NetworkCapableUserGranted => tracing::info!("{text}"),
            SandboxFirstRunDecision::ExplicitConfig => tracing::debug!("{text}"),
            _ => tracing::warn!("{text}"),
        }
        eprintln!("faktor sandbox: {text}");
    }
}
