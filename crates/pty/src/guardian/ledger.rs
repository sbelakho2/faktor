//! Durable terminal-identity ledger + restart reconciliation.
//!
//! The daemon cannot keep a PTY ownership row in RAM across a crash. This
//! ledger is the additive durable record behind the guardian: every
//! session-owned terminal spawn appends one bounded row carrying the
//! [`ProcessIdentity`] (pid + platform start-time marker, never a bare pid),
//! the owner label, and a monotonic row id. A normal shutdown marks the row
//! `reaped`; a crash leaves it `live`.
//!
//! On the next start, [`TerminalLedger::reconcile`] settles every `live` row
//! and returns it as a typed [`TerminalLost`]. Reconciliation only CLASSIFIES
//! (still alive / gone / recycled / unverifiable); it never signals
//! anything — the guardian already adjudicated the process group, and a
//! restarted daemon must not blindly kill whatever now carries the pid.
//!
//! Storage is newline-delimited JSON, append-only between compactions:
//! - every row carries `"v": 1`; unknown versions and unparseable lines are
//!   counted as corruption, never fatal (a crash mid-append must not brick
//!   the ledger);
//! - the file, row count, owner label, and every rewritten buffer are
//!   bounded ([`MAX_LEDGER_BYTES`], [`MAX_LEDGER_LINES`], [`MAX_OWNER_BYTES`]);
//! - `reconcile` rewrites atomically through the shared
//!   [`faktor_fs::atomic`] authority (unique temp + fsync + rename), keeping
//!   `live` rows and at most a bounded tail of settled rows as evidence.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use faktor_core::error::Error;

use super::{group_exists, plausible, IdentityVerdict, ProcessIdentity};

/// Hard cap on one ledger file (bytes). An oversize file is compacted; a
/// full one refuses new rows with a typed `Oversized` error.
pub const MAX_LEDGER_BYTES: usize = 1024 * 1024;

/// Hard cap on live ledger lines before compaction/refusal.
pub const MAX_LEDGER_LINES: usize = 4096;

/// Hard cap on the owner label (bytes). The label names the durable owner
/// (session/task/operation); values never contain control characters.
pub const MAX_OWNER_BYTES: usize = 256;

/// Settled (reaped/lost) rows kept after compaction, for forensics.
const MAX_SETTLED_KEPT: usize = 64;

const LEDGER_VERSION: u32 = 1;
const LEDGER_FILE: &str = "terminals.ndjson";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How a recorded terminal ended up after a restart, from the durable
/// identity alone. Classification only: reconciliation never signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDisposition {
    /// A live process still carries the recorded identity (same start-time
    /// marker): the terminal outlived its daemon/guardian. Reported, never
    /// killed by reconciliation.
    StillAlive,
    /// No process and no process group carry the recorded identity.
    Gone,
    /// The pid is live with a DIFFERENT start-time marker: recycled. A
    /// correct restart must never signal it.
    Recycled,
    /// The platform cannot verify start-time markers.
    Unverifiable,
    /// The record is structurally impossible (pgid 0 or pgid != pid).
    Malformed,
}

impl fmt::Display for TerminalDisposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalDisposition::StillAlive => write!(f, "still alive after its daemon died"),
            TerminalDisposition::Gone => write!(f, "gone"),
            TerminalDisposition::Recycled => write!(f, "pid recycled by another process"),
            TerminalDisposition::Unverifiable => write!(f, "identity not verifiable on this host"),
            TerminalDisposition::Malformed => write!(f, "malformed identity record"),
        }
    }
}

/// One terminal the restarted daemon can no longer own: the durable row that
/// named it, and the identity verdict. Typed so a caller can persist it into
/// the session/ownership rows; reconciliation itself never signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLost {
    pub row_id: String,
    pub owner: String,
    pub pid: u32,
    pub pgid: u32,
    pub start_time: Option<u64>,
    pub recorded_ms: i64,
    pub disposition: TerminalDisposition,
}

impl fmt::Display for TerminalLost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "terminal {} (pid {}, pgid {}, owner {:?}) lost: {}",
            self.row_id, self.pid, self.pgid, self.owner, self.disposition
        )
    }
}

impl std::error::Error for TerminalLost {}

/// One reconciliation pass over the live ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// Every formerly-live row, settled exactly once (a second pass reports
    /// nothing).
    pub lost: Vec<TerminalLost>,
    /// Unparseable/unsupported lines encountered (truncated appends after a
    /// crash, hostile edits): reported, never fatal.
    pub corrupt_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum RowState {
    Live,
    Reaped,
    Lost,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LedgerRow {
    v: u32,
    id: String,
    state: RowState,
    owner: String,
    pid: u32,
    pgid: u32,
    #[serde(default)]
    start_time: Option<u64>,
    recorded_ms: i64,
}

impl LedgerRow {
    fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            pgid: self.pgid,
            start_time: self.start_time,
        }
    }
}

/// The durable terminal ledger of one daemon data directory.
pub struct TerminalLedger {
    path: PathBuf,
    lock: Mutex<()>,
}

impl fmt::Debug for TerminalLedger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalLedger")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl TerminalLedger {
    /// Open (creating when absent) the ledger under `dir`.
    ///
    /// The directory is created if needed; the file itself is created lazily
    /// on the first append so an unopened/unwritable directory surfaces as a
    /// loud error at open time.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::internal(format!("terminal ledger dir {}: {e}", dir.display())))?;
        let path = dir.join(LEDGER_FILE);
        // Refuse a directory/symlink-to-directory masquerading as the file.
        if path.is_dir() {
            return Err(Error::malformed(format!(
                "terminal ledger path {} is a directory",
                path.display()
            )));
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    /// The ledger file path (diagnostics/tests).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one `live` row for a terminal about to be guarded. Returns the
    /// row id used by [`TerminalLedger::mark_reaped`].
    pub fn record_spawn(&self, owner: &str, identity: &ProcessIdentity) -> Result<String, Error> {
        validate_owner(owner)?;
        if !plausible(identity) {
            return Err(Error::malformed(
                "terminal ledger identity must have pid != 0 and pgid == pid",
            ));
        }
        let _guard = self.lock();
        let mut rows = self.read_rows().0;
        // Bound the store before appending: compact settled rows, then
        // refuse typed if live rows alone exhaust the budget.
        let size = file_len(&self.path);
        if rows.len() >= MAX_LEDGER_LINES || size >= MAX_LEDGER_BYTES {
            rows = compact(rows);
            self.write_rows(&rows)?;
            if rows.len() >= MAX_LEDGER_LINES || file_len(&self.path) >= MAX_LEDGER_BYTES {
                return Err(Error::oversized(format!(
                    "terminal ledger is full ({} live rows, {} bytes)",
                    rows.len(),
                    file_len(&self.path)
                )));
            }
        }
        let id = self.next_row_id();
        let row = LedgerRow {
            v: LEDGER_VERSION,
            id: id.clone(),
            state: RowState::Live,
            owner: owner.to_string(),
            pid: identity.pid,
            pgid: identity.pgid,
            start_time: identity.start_time,
            recorded_ms: now_ms(),
        };
        self.append(&row)?;
        Ok(id)
    }

    /// Mark one row `reaped`: the terminal was terminated and collected by
    /// the daemon on the normal path, so restart reconciliation must not
    /// report it. Unknown ids are a no-op (idempotent).
    pub fn mark_reaped(&self, row_id: &str) -> Result<(), Error> {
        let _guard = self.lock();
        let rows = self.read_rows().0;
        let Some(existing) = rows.into_iter().find(|r| r.id == row_id) else {
            return Ok(());
        };
        if existing.state != RowState::Live {
            return Ok(());
        }
        let row = LedgerRow {
            state: RowState::Reaped,
            recorded_ms: now_ms(),
            ..existing
        };
        self.append(&row)
    }

    /// Settle every `live` row exactly once after a restart. Classification
    /// only: never signals, never trusts a recycled pid. The ledger is
    /// rewritten compacted, so a second call reports nothing new.
    pub fn reconcile(&self) -> Result<Reconciliation, Error> {
        let _guard = self.lock();
        let (rows, corrupt_lines) = self.read_rows();
        let mut lost = Vec::new();
        let mut settled = Vec::with_capacity(rows.len());
        for row in rows {
            if row.state != RowState::Live {
                settled.push(row);
                continue;
            }
            let disposition = classify(&row);
            lost.push(TerminalLost {
                row_id: row.id.clone(),
                owner: row.owner.clone(),
                pid: row.pid,
                pgid: row.pgid,
                start_time: row.start_time,
                recorded_ms: row.recorded_ms,
                disposition,
            });
            settled.push(LedgerRow {
                state: RowState::Lost,
                ..row
            });
        }
        let mut settled = compact(settled);
        // Compaction is also the corruption repair: the rewritten file holds
        // only well-formed rows.
        settled.retain(|r| r.state != RowState::Live);
        self.write_rows(&settled)?;
        Ok(Reconciliation {
            lost,
            corrupt_lines,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read + fold the ledger: the last row for an id wins (append-only
    /// transitions), corruption is counted, never fatal.
    fn read_rows(&self) -> (Vec<LedgerRow>, usize) {
        let mut corrupt = 0usize;
        let mut raw = Vec::new();
        match File::open(&self.path) {
            Ok(f) => {
                let mut take = f.take((MAX_LEDGER_BYTES + 1) as u64);
                if take.read_to_end(&mut raw).is_err() {
                    return (Vec::new(), 1);
                }
            }
            Err(_) => return (Vec::new(), 0),
        }
        if raw.len() > MAX_LEDGER_BYTES {
            corrupt += 1;
            raw.truncate(MAX_LEDGER_BYTES);
        }
        let text = String::from_utf8_lossy(&raw);
        let mut order: Vec<String> = Vec::new();
        let mut latest: std::collections::HashMap<String, LedgerRow> =
            std::collections::HashMap::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LedgerRow>(line) {
                Ok(row) if row.v == LEDGER_VERSION => {
                    if !latest.contains_key(&row.id) {
                        order.push(row.id.clone());
                    }
                    latest.insert(row.id.clone(), row);
                }
                Ok(_) => corrupt += 1,
                Err(_) => corrupt += 1,
            }
        }
        let rows = order
            .into_iter()
            .filter_map(|id| latest.remove(&id))
            .collect();
        (rows, corrupt)
    }

    /// Atomic rewrite through the shared [`faktor_fs::atomic`] authority
    /// (unique temp + fsync + rename): a crash mid-rewrite leaves the old
    /// ledger intact and the sequence is never hand-rolled here.
    fn write_rows(&self, rows: &[LedgerRow]) -> Result<(), Error> {
        let mut bytes = Vec::new();
        for row in rows {
            serde_json::to_writer(&mut bytes, row)
                .map_err(|e| Error::internal(format!("terminal ledger encode: {e}")))?;
            bytes.push(b'\n');
            if bytes.len() > MAX_LEDGER_BYTES {
                return Err(Error::oversized(
                    "terminal ledger rewrite exceeds the file cap",
                ));
            }
        }
        faktor_fs::atomic::atomic_replace(&self.path, &bytes).map_err(|e| {
            Error::internal(format!(
                "terminal ledger rewrite {}: {}",
                self.path.display(),
                e.message
            ))
        })?;
        Ok(())
    }

    fn append(&self, row: &LedgerRow) -> Result<(), Error> {
        let mut line = serde_json::to_string(row)
            .map_err(|e| Error::internal(format!("ledger encode: {e}")))?;
        line.push('\n');
        if line.len() > MAX_LEDGER_BYTES {
            return Err(Error::oversized("terminal ledger row exceeds the file cap"));
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| Error::internal(format!("terminal ledger open: {e}")))?;
        f.write_all(line.as_bytes())
            .map_err(|e| Error::internal(format!("terminal ledger append: {e}")))
    }

    fn next_row_id(&self) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{:x}-{:x}-{:x}", std::process::id(), now_ms(), n)
    }
}

fn file_len(path: &Path) -> usize {
    std::fs::metadata(path)
        .map(|m| m.len().min(usize::MAX as u64) as usize)
        .unwrap_or(0)
}

/// Keep every `live` row plus a bounded tail of settled rows.
fn compact(rows: Vec<LedgerRow>) -> Vec<LedgerRow> {
    let mut live = Vec::new();
    let mut settled: Vec<LedgerRow> = Vec::new();
    for row in rows {
        if row.state == RowState::Live {
            live.push(row);
        } else {
            settled.push(row);
        }
    }
    let keep = settled.len().saturating_sub(MAX_SETTLED_KEPT);
    let settled = settled.split_off(keep.min(settled.len()));
    live.extend(settled);
    live
}

/// Classify a live row without ever signalling: the reconciliation rule.
fn classify(row: &LedgerRow) -> TerminalDisposition {
    let identity = row.identity();
    if !plausible(&identity) {
        return TerminalDisposition::Malformed;
    }
    match identity.verify() {
        IdentityVerdict::Match => TerminalDisposition::StillAlive,
        IdentityVerdict::Mismatch => TerminalDisposition::Recycled,
        IdentityVerdict::Unverifiable => TerminalDisposition::Unverifiable,
        IdentityVerdict::Gone => {
            if group_exists(identity.pgid) {
                // The leader is gone but members survive: still an orphaned
                // terminal, still never killed by reconciliation.
                TerminalDisposition::StillAlive
            } else {
                TerminalDisposition::Gone
            }
        }
    }
}

fn validate_owner(owner: &str) -> Result<(), Error> {
    if owner.is_empty() {
        return Err(Error::malformed("terminal ledger owner must not be empty"));
    }
    if owner.len() > MAX_OWNER_BYTES {
        return Err(Error::oversized(format!(
            "terminal ledger owner exceeds {MAX_OWNER_BYTES} bytes"
        )));
    }
    if owner.chars().any(|c| c.is_control()) {
        return Err(Error::malformed(
            "terminal ledger owner contains control characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{probe_supported, process_start_time, GUARDIAN_EXIT_KILLED};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    fn ledger() -> (tempfile::TempDir, TerminalLedger) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = TerminalLedger::open(dir.path()).expect("ledger open");
        (dir, ledger)
    }

    fn spawn_group_sleeper() -> (Child, ProcessIdentity) {
        let child = Command::new("sh")
            .args(["-c", "sleep 300"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleeper");
        let pid = child.id();
        (child, ProcessIdentity::capture(pid, pid))
    }

    #[test]
    fn reaped_spawn_records_are_never_reported_lost() {
        let (_dir, ledger) = ledger();
        let (mut child, identity) = spawn_group_sleeper();
        let row = ledger.record_spawn("session:1/task:2", &identity).unwrap();
        ledger.mark_reaped(&row).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost, vec![], "a reaped terminal is not lost");
        // Settling is exactly-once: a second pass reports nothing either.
        assert_eq!(ledger.reconcile().unwrap().lost, vec![]);
    }

    #[test]
    fn reconciliation_reports_gone_without_signalling_anything() {
        let (_dir, ledger) = ledger();
        let (mut child, identity) = spawn_group_sleeper();
        ledger.record_spawn("session:7", &identity).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost.len(), 1);
        assert_eq!(report.lost[0].disposition, TerminalDisposition::Gone);
        assert_eq!(report.lost[0].pid, identity.pid);
        assert_eq!(report.corrupt_lines, 0);
        // Never signalled: the group is gone because WE reaped it.
        assert!(!group_exists(identity.pgid));
    }

    #[test]
    fn reconciliation_reports_recycled_pid_and_leaves_the_process_alive() {
        let (_dir, ledger) = ledger();
        let (mut child, identity) = spawn_group_sleeper();
        // Simulate a recycled pid: the recorded start-time marker is wrong.
        let stale = ProcessIdentity {
            pid: identity.pid,
            pgid: identity.pgid,
            start_time: identity.start_time.map(|t| t.wrapping_add(1)),
        };
        ledger.record_spawn("session:9", &stale).unwrap();
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost.len(), 1);
        assert_eq!(report.lost[0].disposition, TerminalDisposition::Recycled);
        assert!(
            group_exists(identity.pgid),
            "reconciliation must never kill a recycled-pid target"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reconciliation_reports_still_alive_and_never_kills() {
        let (_dir, ledger) = ledger();
        let (mut child, identity) = spawn_group_sleeper();
        ledger.record_spawn("session:11", &identity).unwrap();
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost.len(), 1);
        assert_eq!(report.lost[0].disposition, TerminalDisposition::StillAlive);
        assert!(group_exists(identity.pgid), "still alive, untouched");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reconciliation_is_exactly_once_per_live_row() {
        let (_dir, ledger) = ledger();
        let (mut child, identity) = spawn_group_sleeper();
        ledger.record_spawn("session:13", &identity).unwrap();
        let first = ledger.reconcile().unwrap();
        assert_eq!(first.lost.len(), 1);
        let second = ledger.reconcile().unwrap();
        assert_eq!(second.lost.len(), 0, "already settled rows never re-report");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn truncated_and_hostile_lines_are_counted_not_fatal() {
        let (_dir, ledger) = ledger();
        // Two intact records; the second line is then corrupted by a
        // simulated crash-mid-append truncation plus hostile injected text.
        let (mut first, identity_a) = spawn_group_sleeper();
        let (mut second, identity_b) = spawn_group_sleeper();
        ledger.record_spawn("session:17", &identity_a).unwrap();
        ledger.record_spawn("session:18", &identity_b).unwrap();
        let _ = first.kill();
        let _ = first.wait();
        let _ = second.kill();
        let _ = second.wait();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut raw = std::fs::read(ledger.path()).unwrap();
        raw.truncate(raw.len() - 7);
        raw.extend_from_slice(b"{\"v\":1,\"id\":\"injected\"}\nnot json at all\n");
        std::fs::write(ledger.path(), raw).unwrap();
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost.len(), 1, "the intact row survives");
        assert!(
            report.corrupt_lines >= 2,
            "truncated + hostile lines are counted: {report:?}"
        );
        let on_disk = std::fs::read_to_string(ledger.path()).unwrap();
        assert!(!on_disk.contains("injected"));
        assert!(!on_disk.contains("not json"));
    }

    #[test]
    fn hostile_owner_labels_are_rejected() {
        let (_dir, ledger) = ledger();
        let pid = std::process::id();
        let identity = ProcessIdentity::capture(pid, pid);
        assert_eq!(
            ledger.record_spawn("", &identity).unwrap_err().kind,
            faktor_core::error::ErrorKind::Malformed
        );
        assert_eq!(
            ledger
                .record_spawn("session\ninjected", &identity)
                .unwrap_err()
                .kind,
            faktor_core::error::ErrorKind::Malformed
        );
        assert_eq!(
            ledger
                .record_spawn(&"x".repeat(MAX_OWNER_BYTES + 1), &identity)
                .unwrap_err()
                .kind,
            faktor_core::error::ErrorKind::Oversized
        );
    }

    #[test]
    fn malformed_identities_are_refused_before_any_write() {
        let (_dir, ledger) = ledger();
        let bad = ProcessIdentity {
            pid: 0,
            pgid: 0,
            start_time: Some(1),
        };
        assert_eq!(
            ledger.record_spawn("session:1", &bad).unwrap_err().kind,
            faktor_core::error::ErrorKind::Malformed
        );
        let split = ProcessIdentity {
            pid: 42,
            pgid: 43,
            start_time: Some(1),
        };
        assert_eq!(
            ledger.record_spawn("session:1", &split).unwrap_err().kind,
            faktor_core::error::ErrorKind::Malformed
        );
        let huge = ProcessIdentity {
            pid: u32::MAX,
            pgid: u32::MAX,
            start_time: Some(1),
        };
        assert_eq!(
            ledger.record_spawn("session:1", &huge).unwrap_err().kind,
            faktor_core::error::ErrorKind::Malformed
        );
    }

    #[test]
    fn hostile_on_disk_records_are_classified_never_signalled() {
        let (_dir, ledger) = ledger();
        let mut raw = String::new();
        for (id, pid, pgid) in [("huge", u32::MAX, u32::MAX), ("zero", 1, 0)] {
            let row = LedgerRow {
                v: LEDGER_VERSION,
                id: id.into(),
                state: RowState::Live,
                owner: format!("session:{id}"),
                pid,
                pgid,
                start_time: Some(1),
                recorded_ms: now_ms(),
            };
            raw.push_str(&serde_json::to_string(&row).unwrap());
            raw.push('\n');
        }
        std::fs::write(ledger.path(), raw).unwrap();
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost.len(), 2);
        for lost in &report.lost {
            assert_eq!(lost.disposition, TerminalDisposition::Malformed);
        }
    }

    #[test]
    fn the_ledger_is_line_bounded_under_spawn_pressure() {
        let (_dir, ledger) = ledger();
        let pid = std::process::id();
        let identity = ProcessIdentity::capture(pid, pid);
        // Seed a full budget of SETTLED rows directly (cheap): the next
        // record must compact them away instead of growing without bound.
        let mut raw = String::new();
        for i in 0..MAX_LEDGER_LINES {
            let row = LedgerRow {
                v: LEDGER_VERSION,
                id: format!("seed-{i}"),
                state: RowState::Reaped,
                owner: format!("session:{i}"),
                pid,
                pgid: pid,
                start_time: identity.start_time,
                recorded_ms: now_ms(),
            };
            raw.push_str(&serde_json::to_string(&row).unwrap());
            raw.push('\n');
        }
        std::fs::write(ledger.path(), raw).unwrap();
        let row = ledger.record_spawn("session:live", &identity).unwrap();
        let rows = ledger.read_rows().0;
        assert_eq!(
            rows.len(),
            MAX_SETTLED_KEPT + 1,
            "settled rows are compacted to the bounded tail"
        );
        assert!(rows
            .iter()
            .any(|r| r.id == row && r.state == RowState::Live));
        assert!(
            std::fs::metadata(ledger.path()).unwrap().len() <= MAX_LEDGER_BYTES as u64,
            "ledger bytes stay bounded"
        );
        // A budget full of LIVE rows cannot be compacted away: typed refusal.
        let mut raw = String::new();
        for i in 0..MAX_LEDGER_LINES {
            let row = LedgerRow {
                v: LEDGER_VERSION,
                id: format!("live-{i}"),
                state: RowState::Live,
                owner: format!("session:{i}"),
                pid,
                pgid: pid,
                start_time: identity.start_time,
                recorded_ms: now_ms(),
            };
            raw.push_str(&serde_json::to_string(&row).unwrap());
            raw.push('\n');
        }
        std::fs::write(ledger.path(), raw).unwrap();
        assert_eq!(
            ledger
                .record_spawn("session:overflow", &identity)
                .unwrap_err()
                .kind,
            faktor_core::error::ErrorKind::Oversized
        );
    }

    #[test]
    fn unknown_ids_and_double_marks_are_noops() {
        let (_dir, ledger) = ledger();
        ledger.mark_reaped("nope").unwrap();
        let pid = std::process::id();
        let identity = ProcessIdentity::capture(pid, pid);
        let row = ledger.record_spawn("session:1", &identity).unwrap();
        ledger.mark_reaped(&row).unwrap();
        ledger.mark_reaped(&row).unwrap();
        let report = ledger.reconcile().unwrap();
        assert_eq!(report.lost.len(), 0);
    }

    #[test]
    fn direct_guardian_run_leaves_no_lost_row_when_marked_reaped() {
        // The full normal path: spawn a group, record it, run the guardian,
        // kill + reap, mark reaped, then reconcile reports nothing.
        let (_dir, ledger) = ledger();
        let (mut child, identity) = spawn_group_sleeper();
        let row = ledger.record_spawn("session:23", &identity).unwrap();
        let mut guardian = crate::guardian::GuardianHandle::spawn(identity).unwrap();
        assert_eq!(
            guardian.release(),
            Some(GUARDIAN_EXIT_KILLED),
            "guardian killed the group"
        );
        let _ = child.wait();
        ledger.mark_reaped(&row).unwrap();
        assert_eq!(ledger.reconcile().unwrap().lost.len(), 0);
        if probe_supported() {
            assert!(process_start_time(std::process::id()).is_some());
        }
    }
}
