//! Append-only, hash-chained durable approval ledger.
//!
//! One JSON object per line. Each entry commits to its sequence number, the
//! previous entry digest, the recording time and the canonical JSON body, so
//! deletion, reordering, truncation or in-place edits are detected on the
//! next open instead of being read as absence. An OS advisory lock on an open
//! descriptor serializes writers across processes: the kernel releases it
//! automatically when the holder crashes, and it is never a deletable file
//! that another process could mistake for stale.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::{
    content_digest, ApprovalDecision, ApprovalError, ApprovalRequest, MAX_OPEN_APPROVALS,
};
use crate::operator::approval_dir;

/// Ledger filename inside the approval directory.
pub const LEDGER_FILE: &str = "ledger.jsonl";
/// Local head file inside the approval directory (legacy/anti-truncation).
pub const LEDGER_HEAD_FILE: &str = "ledger.head";
/// Monotonic anchor filename outside the approval directory.
pub const LEDGER_ANCHOR_FILE: &str = "ledger.anchor";
/// Maximum wall clock observed so far (monotonic expiry anchor).
pub const CLOCK_ANCHOR_FILE: &str = "clock.anchor";
/// Anchor directory, a sibling of (never inside) the approval directory.
pub const LEDGER_ANCHOR_DIR: &str = "approval-anchor";
const LEDGER_DOMAIN: &[u8] = b"faktor.approval.ledger/v1";
const LEDGER_SCHEMA: &str = "faktor.approval.ledger-entry/v1";
const LEDGER_HEAD_SCHEMA: &str = "faktor.approval.ledger-head/v1";
const LEDGER_ANCHOR_SCHEMA: &str = "faktor.approval.ledger-anchor/v1";
const CLOCK_ANCHOR_SCHEMA: &str = "faktor.approval.clock-anchor/v1";
const MAX_LEDGER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LEDGER_HEAD_BYTES: u64 = 1_024;
const MAX_CLOCK_ANCHOR_BYTES: u64 = 1_024;
const LOCK_FILE: &str = ".ledger.lock";
const CLOCK_LOCK_FILE: &str = "clock.lock";
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(5);

/// The durable ledger path inside a data dir.
pub fn ledger_path(data_dir: &Path) -> PathBuf {
    approval_dir(data_dir).join(LEDGER_FILE)
}

/// The local committed-head path inside a data dir.
pub fn ledger_head_path(data_dir: &Path) -> PathBuf {
    approval_dir(data_dir).join(LEDGER_HEAD_FILE)
}

/// The monotonic anchor path, deliberately outside the approval directory so
/// rolling back every file under `approval/` does not roll back this counter.
pub fn ledger_anchor_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LEDGER_ANCHOR_DIR).join(LEDGER_ANCHOR_FILE)
}

/// The persisted maximum wall clock observed by this data dir. Kept outside
/// `approval/` next to the ledger anchor so resetting the ledger directory
/// cannot reset expiry monotonicity.
pub fn clock_anchor_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LEDGER_ANCHOR_DIR).join(CLOCK_ANCHOR_FILE)
}

/// The last durably committed chain position. The hash chain detects edits
/// and reordering at any position; this anchor additionally detects tail
/// truncation (a prefix of a valid chain is itself a valid chain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerHead {
    schema: String,
    seq: u64,
    digest: String,
}

/// A monotonic head counter stored outside the ledger's own directory. A
/// same-user attacker can rewrite it too (see the crate docs); the honest
/// guarantee is detection of a ledger-only rollback, not prevention of a full
/// same-user rollback of both stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerAnchor {
    schema: String,
    seq: u64,
    digest: String,
}

/// One durable ledger event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerBody {
    /// An agent/requester proposed an action.
    Requested {
        /// The request.
        request: ApprovalRequest,
    },
    /// A human operator decided exactly once.
    Decided {
        /// The signed decision.
        decision: ApprovalDecision,
    },
}

/// One hash-chained ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerEntry {
    /// Entry schema.
    pub schema: String,
    /// Monotonic sequence starting at 1.
    pub seq: u64,
    /// Recording time (epoch ms).
    pub recorded_at_ms: u64,
    /// Previous entry digest (64 hex zeros for the first).
    pub prev_digest: String,
    /// Event body.
    pub body: LedgerBody,
    /// This entry's digest.
    pub digest: String,
}

/// A fully replayed ledger.
#[derive(Debug, Clone, Default)]
pub struct ApprovalLedger {
    /// Every entry in order (the audit trail).
    pub entries: Vec<LedgerEntry>,
    /// Requests by id.
    pub requests: BTreeMap<String, ApprovalRequest>,
    /// Decisions by request id.
    pub decisions: BTreeMap<String, ApprovalDecision>,
}

impl ApprovalLedger {
    /// Load and verify the whole chain. A missing ledger file is an honest
    /// empty ledger only when the external anchor also records no committed
    /// history; a malformed, truncated or rolled-back file is a typed refusal.
    pub fn load(data_dir: &Path) -> Result<Self, ApprovalError> {
        Self::load_inner(data_dir, false)
    }

    fn load_inner(data_dir: &Path, heal_anchor: bool) -> Result<Self, ApprovalError> {
        let path = ledger_path(data_dir);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let ledger = Self::default();
                verify_anchor(data_dir, &ledger, false)?;
                return Ok(ledger);
            }
            Err(error) => return Err(ApprovalError::Store(error.to_string())),
        };
        if !metadata.is_file() {
            return Err(ApprovalError::Store(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if metadata.len() > MAX_LEDGER_BYTES {
            return Err(ApprovalError::Store(format!(
                "approval ledger exceeds {MAX_LEDGER_BYTES} bytes"
            )));
        }
        let mut raw = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(&path)
            .map_err(|error| ApprovalError::Store(error.to_string()))?
            .take(MAX_LEDGER_BYTES + 1)
            .read_to_end(&mut raw)
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
        if raw.len() as u64 > MAX_LEDGER_BYTES {
            return Err(ApprovalError::Store(format!(
                "approval ledger exceeds {MAX_LEDGER_BYTES} bytes"
            )));
        }
        let text = std::str::from_utf8(&raw)
            .map_err(|error| ApprovalError::Store(format!("ledger is not UTF-8: {error}")))?;
        let mut ledger = Self::default();
        let mut previous = "0".repeat(64);
        let mut expected_seq = 1u64;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: LedgerEntry = serde_json::from_str(line).map_err(|error| {
                ApprovalError::Store(format!("ledger line {}: {error}", index + 1))
            })?;
            if entry.schema != LEDGER_SCHEMA {
                return Err(ApprovalError::Store(format!(
                    "ledger line {} has schema {:?}",
                    index + 1,
                    entry.schema
                )));
            }
            if entry.seq != expected_seq {
                return Err(ApprovalError::Store(format!(
                    "ledger sequence gap at line {}: expected {}, found {}",
                    index + 1,
                    expected_seq,
                    entry.seq
                )));
            }
            if entry.prev_digest != previous {
                return Err(ApprovalError::Store(format!(
                    "ledger chain break at line {}",
                    index + 1
                )));
            }
            let actual = entry_digest(
                &entry.prev_digest,
                entry.seq,
                entry.recorded_at_ms,
                &entry.body,
            )?;
            if actual != entry.digest {
                return Err(ApprovalError::Store(format!(
                    "ledger digest mismatch at line {}",
                    index + 1
                )));
            }
            match &entry.body {
                LedgerBody::Requested { request } => {
                    request.validate()?;
                    if ledger
                        .requests
                        .insert(request.id.clone(), request.clone())
                        .is_some()
                    {
                        return Err(ApprovalError::Store(format!(
                            "duplicate approval request {}",
                            request.id
                        )));
                    }
                }
                LedgerBody::Decided { decision } => {
                    let Some(request) = ledger.requests.get(&decision.request_id) else {
                        return Err(ApprovalError::Store(format!(
                            "decision for unknown request {}",
                            decision.request_id
                        )));
                    };
                    decision.validate()?;
                    if request.digest()? != decision.request_digest {
                        return Err(ApprovalError::Store(
                            "decision request digest does not match the recorded request"
                                .to_string(),
                        ));
                    }
                    if decision.decided_at_ms < request.created_at_ms
                        || decision.decided_at_ms > request.expires_at_ms
                    {
                        return Err(ApprovalError::Store(format!(
                            "decision for {} is timed outside its request lifetime",
                            decision.request_id
                        )));
                    }
                    if decision.principal.id == request.requester.id {
                        return Err(ApprovalError::Store(format!(
                            "request {} records a self-approval",
                            decision.request_id
                        )));
                    }
                    if ledger
                        .decisions
                        .insert(decision.request_id.clone(), decision.clone())
                        .is_some()
                    {
                        return Err(ApprovalError::Store(format!(
                            "request {} has more than one decision",
                            decision.request_id
                        )));
                    }
                }
            }
            previous = entry.digest.clone();
            expected_seq += 1;
            ledger.entries.push(entry);
        }
        verify_anchor(data_dir, &ledger, heal_anchor)?;
        verify_committed_head(data_dir, &ledger)?;
        Ok(ledger)
    }

    /// Begin a locked read/write transaction. The chain is verified exactly
    /// once per operation; the lock is held until the transaction is dropped.
    /// A writer may reconcile an interrupted append (anchor behind a fully
    /// verified ledger).
    pub(crate) fn begin(data_dir: &Path) -> Result<LedgerTransaction, ApprovalError> {
        let lock = LedgerLock::acquire(data_dir)?;
        let ledger = Self::load_inner(data_dir, true)?;
        Ok(LedgerTransaction {
            _lock: lock,
            data_dir: data_dir.to_path_buf(),
            ledger,
        })
    }
}

/// One locked ledger transaction. Appends update both the durable line and
/// the committed head without re-reading the whole chain a second time.
pub(crate) struct LedgerTransaction {
    _lock: LedgerLock,
    data_dir: PathBuf,
    ledger: ApprovalLedger,
}

impl LedgerTransaction {
    /// The verified chain as observed under the lock.
    pub(crate) fn ledger(&self) -> &ApprovalLedger {
        &self.ledger
    }

    /// Append one body under the held cross-process lock.
    pub(crate) fn append(
        &mut self,
        body: LedgerBody,
        now_ms: u64,
    ) -> Result<LedgerEntry, ApprovalError> {
        if now_ms == 0 {
            return Err(ApprovalError::Invalid(
                "ledger time must be non-zero".to_string(),
            ));
        }
        match &body {
            LedgerBody::Requested { request } => {
                request.validate()?;
                if request.created_at_ms != now_ms {
                    return Err(ApprovalError::Invalid(
                        "request creation time must equal the ledger time".to_string(),
                    ));
                }
                if let Some(existing) = self.ledger.requests.get(&request.id) {
                    if existing == request {
                        return self.recorded_request_entry(&request.id);
                    }
                    return Err(ApprovalError::Conflict(format!(
                        "request {} already records different content",
                        request.id
                    )));
                }
                // Bounded pending queue under the held ledger lock: expired
                // requests free capacity without a mutation, so a long task
                // can never accumulate an unbounded human review backlog.
                let open = self
                    .ledger
                    .requests
                    .values()
                    .filter(|request| {
                        !self.ledger.decisions.contains_key(&request.id)
                            && request.expires_at_ms >= now_ms
                    })
                    .count();
                if open >= MAX_OPEN_APPROVALS {
                    return Err(ApprovalError::PendingLimit {
                        open,
                        max: MAX_OPEN_APPROVALS,
                    });
                }
            }
            LedgerBody::Decided { decision } => {
                let request = self
                    .ledger
                    .requests
                    .get(&decision.request_id)
                    .ok_or_else(|| {
                        ApprovalError::NotFound(format!("approval request {}", decision.request_id))
                    })?;
                decision.validate()?;
                if request.digest()? != decision.request_digest {
                    return Err(ApprovalError::Invalid(
                        "decision request digest does not match the recorded request".to_string(),
                    ));
                }
                if decision.decided_at_ms < request.created_at_ms
                    || decision.decided_at_ms > request.expires_at_ms
                {
                    return Err(ApprovalError::Invalid(
                        "decision is timed outside its request lifetime".to_string(),
                    ));
                }
                if decision.principal.id == request.requester.id {
                    return Err(ApprovalError::SelfApproval {
                        principal: decision.principal.id.clone(),
                    });
                }
                if let Some(existing) = self.ledger.decisions.get(&decision.request_id) {
                    if existing == decision {
                        return self.recorded_decision_entry(&decision.request_id);
                    }
                    return Err(ApprovalError::Conflict(format!(
                        "request {} is already decided",
                        decision.request_id
                    )));
                }
            }
        }
        let seq = self.ledger.entries.len() as u64 + 1;
        let prev_digest = self
            .ledger
            .entries
            .last()
            .map(|entry| entry.digest.clone())
            .unwrap_or_else(|| "0".repeat(64));
        let digest = entry_digest(&prev_digest, seq, now_ms, &body)?;
        let entry = LedgerEntry {
            schema: LEDGER_SCHEMA.to_string(),
            seq,
            recorded_at_ms: now_ms,
            prev_digest,
            body,
            digest,
        };
        let line = serde_json::to_vec(&entry)
            .map_err(|error| ApprovalError::Store(format!("ledger encode: {error}")))?;
        let path = ledger_path(&self.data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApprovalError::Store(error.to_string()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
        file.write_all(&line)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
        sync_directory(
            path.parent()
                .ok_or_else(|| ApprovalError::Store("ledger has no parent".to_string()))?,
        )?;
        write_committed_head(&self.data_dir, entry.seq, &entry.digest)?;
        self.record(entry.clone());
        Ok(entry)
    }

    fn record(&mut self, entry: LedgerEntry) {
        match &entry.body {
            LedgerBody::Requested { request } => {
                self.ledger
                    .requests
                    .insert(request.id.clone(), request.clone());
            }
            LedgerBody::Decided { decision } => {
                self.ledger
                    .decisions
                    .insert(decision.request_id.clone(), decision.clone());
            }
        }
        self.ledger.entries.push(entry);
    }

    fn recorded_request_entry(&self, id: &str) -> Result<LedgerEntry, ApprovalError> {
        self.ledger
            .entries
            .iter()
            .find(|entry| {
                matches!(
                    &entry.body,
                    LedgerBody::Requested { request } if request.id == id
                )
            })
            .cloned()
            .ok_or_else(|| ApprovalError::Store("recorded request has no ledger entry".to_string()))
    }

    fn recorded_decision_entry(&self, request_id: &str) -> Result<LedgerEntry, ApprovalError> {
        self.ledger
            .entries
            .iter()
            .find(|entry| {
                matches!(
                    &entry.body,
                    LedgerBody::Decided { decision } if decision.request_id == request_id
                )
            })
            .cloned()
            .ok_or_else(|| {
                ApprovalError::Store("recorded decision has no ledger entry".to_string())
            })
    }
}

fn write_committed_head(data_dir: &Path, seq: u64, digest: &str) -> Result<(), ApprovalError> {
    let head = LedgerHead {
        schema: LEDGER_HEAD_SCHEMA.to_string(),
        seq,
        digest: digest.to_string(),
    };
    let bytes = serde_json::to_vec(&head)
        .map_err(|error| ApprovalError::Store(format!("ledger head encode: {error}")))?;
    faktor_fs::atomic::atomic_replace(&ledger_head_path(data_dir), &bytes)
        .map_err(|error| ApprovalError::Store(format!("ledger head: {error}")))?;
    write_anchor(data_dir, seq, digest)
}

fn write_anchor(data_dir: &Path, seq: u64, digest: &str) -> Result<(), ApprovalError> {
    let path = ledger_anchor_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ApprovalError::Store(format!("ledger anchor dir: {error}")))?;
    }
    let anchor = LedgerAnchor {
        schema: LEDGER_ANCHOR_SCHEMA.to_string(),
        seq,
        digest: digest.to_string(),
    };
    let bytes = serde_json::to_vec(&anchor)
        .map_err(|error| ApprovalError::Store(format!("ledger anchor encode: {error}")))?;
    faktor_fs::atomic::atomic_replace(&path, &bytes)
        .map_err(|error| ApprovalError::Store(format!("ledger anchor: {error}")))?;
    Ok(())
}

fn verify_anchor(
    data_dir: &Path,
    ledger: &ApprovalLedger,
    heal_interrupted_append: bool,
) -> Result<(), ApprovalError> {
    let path = ledger_anchor_path(data_dir);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if ledger.entries.is_empty() {
                return Ok(());
            }
            return Err(ApprovalError::AnchorDegraded {
                path: path.display().to_string(),
                reason: "the monotonic ledger anchor is missing; refusing to trust a ledger whose head is no longer independently anchored"
                    .to_string(),
            });
        }
        Err(error) => {
            return Err(ApprovalError::AnchorDegraded {
                path: path.display().to_string(),
                reason: error.to_string(),
            })
        }
    };
    if raw.len() as u64 > MAX_LEDGER_HEAD_BYTES {
        return Err(ApprovalError::AnchorDegraded {
            path: path.display().to_string(),
            reason: "ledger anchor exceeds its bound".to_string(),
        });
    }
    let anchor: LedgerAnchor =
        serde_json::from_slice(&raw).map_err(|error| ApprovalError::AnchorDegraded {
            path: path.display().to_string(),
            reason: format!("ledger anchor is not valid JSON: {error}"),
        })?;
    if anchor.schema != LEDGER_ANCHOR_SCHEMA {
        return Err(ApprovalError::AnchorDegraded {
            path: path.display().to_string(),
            reason: format!(
                "ledger anchor schema {:?} is not {LEDGER_ANCHOR_SCHEMA:?}",
                anchor.schema
            ),
        });
    }
    if anchor.seq == 0
        || anchor.digest.len() != 64
        || !anchor.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ApprovalError::AnchorDegraded {
            path: path.display().to_string(),
            reason: "ledger anchor is malformed".to_string(),
        });
    }
    let committed = anchor.seq as usize;
    if committed > ledger.entries.len() {
        return Err(ApprovalError::LedgerRollback {
            anchor_seq: anchor.seq,
            ledger_len: ledger.entries.len(),
            path: path.display().to_string(),
        });
    }
    let Some(entry) = ledger.entries.get(committed.saturating_sub(1)) else {
        return Err(ApprovalError::LedgerRollback {
            anchor_seq: anchor.seq,
            ledger_len: ledger.entries.len(),
            path: path.display().to_string(),
        });
    };
    if entry.digest != anchor.digest {
        return Err(ApprovalError::LedgerRollback {
            anchor_seq: anchor.seq,
            ledger_len: ledger.entries.len(),
            path: path.display().to_string(),
        });
    }
    if committed < ledger.entries.len() {
        if heal_interrupted_append && local_head_matches_tail(data_dir, ledger)? {
            if let Some(last) = ledger.entries.last() {
                write_anchor(data_dir, last.seq, &last.digest)?;
                return Ok(());
            }
        }
        return Err(ApprovalError::AnchorDegraded {
            path: path.display().to_string(),
            reason: format!(
                "ledger anchor commits to entry {} while the verified chain has {} entries; an interrupted append or a rolled-back anchor needs explicit reconciliation",
                anchor.seq,
                ledger.entries.len()
            ),
        });
    }
    Ok(())
}

fn local_head_matches_tail(
    data_dir: &Path,
    ledger: &ApprovalLedger,
) -> Result<bool, ApprovalError> {
    let Some(last) = ledger.entries.last() else {
        return Ok(false);
    };
    let path = ledger_head_path(data_dir);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(ApprovalError::Store(error.to_string())),
    };
    if raw.len() as u64 > MAX_LEDGER_HEAD_BYTES {
        return Ok(false);
    }
    let Ok(head) = serde_json::from_slice::<LedgerHead>(&raw) else {
        return Ok(false);
    };
    Ok(head.schema == LEDGER_HEAD_SCHEMA && head.seq == last.seq && head.digest == last.digest)
}

fn verify_committed_head(data_dir: &Path, ledger: &ApprovalLedger) -> Result<(), ApprovalError> {
    let path = ledger_head_path(data_dir);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A legacy ledger predates the committed-head anchor. It is
            // still chain-verified; the next successful append writes a head.
            return Ok(());
        }
        Err(error) => return Err(ApprovalError::Store(error.to_string())),
    };
    if raw.len() as u64 > MAX_LEDGER_HEAD_BYTES {
        return Err(ApprovalError::Store(
            "approval ledger head exceeds its bound".to_string(),
        ));
    }
    let head: LedgerHead = serde_json::from_slice(&raw)
        .map_err(|error| ApprovalError::Store(format!("ledger head: {error}")))?;
    if head.schema != LEDGER_HEAD_SCHEMA {
        return Err(ApprovalError::Store(format!(
            "ledger head schema {:?} is not {LEDGER_HEAD_SCHEMA:?}",
            head.schema
        )));
    }
    if head.seq == 0
        || head.digest.len() != 64
        || !head.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ApprovalError::Store("ledger head is malformed".to_string()));
    }
    let committed = head.seq as usize;
    let Some(entry) = ledger.entries.get(committed.saturating_sub(1)) else {
        return Err(ApprovalError::Store(format!(
            "approval ledger was truncated: head commits to entry {} but only {} remain",
            head.seq,
            ledger.entries.len()
        )));
    };
    if entry.seq != head.seq || entry.digest != head.digest {
        return Err(ApprovalError::Store(format!(
            "approval ledger head does not match entry {}",
            head.seq
        )));
    }
    Ok(())
}

fn entry_digest(
    prev_digest: &str,
    seq: u64,
    recorded_at_ms: u64,
    body: &LedgerBody,
) -> Result<String, ApprovalError> {
    #[derive(Serialize)]
    struct EntryIdentity<'a> {
        schema: &'static str,
        seq: u64,
        recorded_at_ms: u64,
        prev_digest: &'a str,
        body: &'a LedgerBody,
    }
    content_digest(
        LEDGER_DOMAIN,
        &EntryIdentity {
            schema: LEDGER_SCHEMA,
            seq,
            recorded_at_ms,
            prev_digest,
            body,
        },
    )
}

fn sync_directory(path: &Path) -> Result<(), ApprovalError> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
    }
    Ok(())
}

/// An OS advisory exclusive lock held on an open descriptor. The lock file
/// itself is a stable rendezvous inode and is intentionally never deleted:
/// deleting it would let a third process create a new inode and overlap the
/// still-live holder. The kernel releases the lock on crash, so there is no
/// stale-lock heuristic and no stale-deletion race.
#[derive(Debug)]
struct LedgerLock {
    file: File,
}

impl LedgerLock {
    fn acquire(data_dir: &Path) -> Result<Self, ApprovalError> {
        Self::acquire_with_wait(data_dir, LOCK_WAIT)
    }

    fn acquire_with_wait(data_dir: &Path, wait: Duration) -> Result<Self, ApprovalError> {
        let directory = approval_dir(data_dir);
        std::fs::create_dir_all(&directory)
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
        let path = directory.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => {
                    // The descriptor is the lock; the file content is never
                    // read, truncated or trusted, so a live lock cannot be
                    // "cleaned" by age and no arbitrary symlink target is
                    // mutated here.
                    return Ok(Self { file });
                }
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= wait {
                        return Err(ApprovalError::Busy(format!(
                            "approval ledger lock {} held for more than {:?}",
                            path.display(),
                            wait
                        )));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(TryLockError::Error(error)) => {
                    return Err(ApprovalError::Store(error.to_string()))
                }
            }
        }
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        // Unlock only the descriptor this guard owns; the path is left alone.
        let _ = self.file.unlock();
    }
}

/// The persisted high-water mark of observed wall-clock time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockAnchor {
    schema: String,
    max_seen_wall_ms: u64,
}

/// Monotonic wall-clock observation.
///
/// `max_seen_wall_ms` is persisted under the anchor directory and updated
/// under an OS file lock. Effective time is `max(observed, persisted)`, so a
/// backwards wall-clock jump can never move an approval back before its
/// expiry. In-process tokens additionally carry an [`Instant`] deadline, so
/// even a wall-clock change cannot extend a token already minted.
#[derive(Debug, Clone)]
pub(crate) struct MonotonicClock {
    root: PathBuf,
}

impl MonotonicClock {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Observe `observed_ms` and return the monotonic effective time.
    pub(crate) fn effective_now(&self, observed_ms: u64) -> Result<u64, ApprovalError> {
        let anchor_dir = self.root.join(LEDGER_ANCHOR_DIR);
        std::fs::create_dir_all(&anchor_dir)
            .map_err(|error| ApprovalError::Store(format!("clock anchor dir: {error}")))?;
        let lock_path = anchor_dir.join(CLOCK_LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| ApprovalError::Store(format!("clock lock: {error}")))?;
        lock.lock()
            .map_err(|error| ApprovalError::Store(format!("clock lock: {error}")))?;
        let result = self.observe_locked(observed_ms);
        let _ = lock.unlock();
        result
    }

    fn observe_locked(&self, observed_ms: u64) -> Result<u64, ApprovalError> {
        let path = clock_anchor_path(&self.root);
        let persisted = match std::fs::read(&path) {
            Ok(raw) => {
                if raw.len() as u64 > MAX_CLOCK_ANCHOR_BYTES {
                    return Err(ApprovalError::Store(
                        "clock anchor exceeds its bound".to_string(),
                    ));
                }
                let anchor: ClockAnchor = serde_json::from_slice(&raw)
                    .map_err(|error| ApprovalError::Store(format!("clock anchor: {error}")))?;
                if anchor.schema != CLOCK_ANCHOR_SCHEMA {
                    return Err(ApprovalError::Store(format!(
                        "clock anchor schema {:?} is not {CLOCK_ANCHOR_SCHEMA:?}",
                        anchor.schema
                    )));
                }
                anchor.max_seen_wall_ms
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(ApprovalError::Store(format!("clock anchor: {error}"))),
        };
        let effective = observed_ms.max(persisted);
        if effective > persisted {
            let anchor = ClockAnchor {
                schema: CLOCK_ANCHOR_SCHEMA.to_string(),
                max_seen_wall_ms: effective,
            };
            let bytes = serde_json::to_vec(&anchor)
                .map_err(|error| ApprovalError::Store(format!("clock anchor encode: {error}")))?;
            faktor_fs::atomic::atomic_replace(&path, &bytes)
                .map_err(|error| ApprovalError::Store(format!("clock anchor: {error}")))?;
        }
        Ok(effective)
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};
    use std::process::{Child, ChildStdout, Command, Stdio};

    const CHILD_ENV: &str = "FAKTOR_APPROVAL_LOCK_CHILD";
    const CHILD_DIR_ENV: &str = "FAKTOR_APPROVAL_LOCK_DIR";
    const CHILD_TEST: &str = "store::lock_tests::child_holds_the_lock";

    /// Child process entry point: acquire the real ledger lock and hold it
    /// until the parent closes stdin or kills this process. The parent is the
    /// regular test harness re-invoked with `--exact`.
    #[test]
    fn child_holds_the_lock() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let dir = std::env::var_os(CHILD_DIR_ENV).expect("child data dir");
        let _lock = LedgerLock::acquire(Path::new(&dir)).unwrap();
        println!("LOCKED");
        std::io::stdout().flush().unwrap();
        let mut ignored = String::new();
        let _ = std::io::stdin().read_line(&mut ignored);
    }

    fn spawn_holder(dir: &Path) -> (Child, BufReader<ChildStdout>) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env(CHILD_DIR_ENV, dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn lock holder");
        let stdout = child.stdout.take().expect("child stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).expect("read child output");
            assert!(
                read != 0,
                "the lock-holder child exited before acquiring the lock"
            );
            if line.contains("LOCKED") {
                break;
            }
        }
        (child, reader)
    }

    fn reap(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn concurrent_processes_serialize_and_never_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let (mut child, _reader) = spawn_holder(temp.path());
        let error = LedgerLock::acquire_with_wait(temp.path(), Duration::from_millis(150))
            .expect_err("a second process must not acquire a held ledger lock");
        assert!(
            matches!(error, ApprovalError::Busy(_)),
            "expected a typed busy refusal, got {error:?}"
        );
        reap(&mut child);
        let lock = LedgerLock::acquire_with_wait(temp.path(), Duration::from_secs(2))
            .expect("the released lock must be acquirable");
        drop(lock);
    }

    #[test]
    fn a_crashed_holder_releases_the_os_lock_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let (mut child, _reader) = spawn_holder(temp.path());
        reap(&mut child);
        let started = Instant::now();
        let lock = LedgerLock::acquire_with_wait(temp.path(), Duration::from_secs(2))
            .expect("a dead holder's lock is released by the kernel");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "crash release must not wait for a stale timeout"
        );
        drop(lock);
    }

    #[test]
    fn a_live_holders_lock_file_is_never_deleted_or_bypassed() {
        let temp = tempfile::tempdir().unwrap();
        let (mut child, _reader) = spawn_holder(temp.path());
        let path = approval_dir(temp.path()).join(LOCK_FILE);
        assert!(path.exists(), "the rendezvous file exists while held");
        // Forge the mtime far into the past: the old 30s stale heuristic
        // would delete this live lock and let a second process overlap.
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3_600))
            .unwrap();
        let error = LedgerLock::acquire_with_wait(temp.path(), Duration::from_millis(150))
            .expect_err("an old mtime must not make a live lock acquirable");
        assert!(matches!(error, ApprovalError::Busy(_)), "{error:?}");
        assert!(path.exists(), "the live lock file must not be removed");
        reap(&mut child);
    }

    #[test]
    fn repeated_lock_unlock_cycles_keep_the_rendezvous_stable() {
        let temp = tempfile::tempdir().unwrap();
        let path = approval_dir(temp.path()).join(LOCK_FILE);
        for _ in 0..64 {
            let lock = LedgerLock::acquire_with_wait(temp.path(), Duration::from_millis(500))
                .expect("an uncontended lock must acquire");
            drop(lock);
        }
        assert!(path.exists(), "unlock must not delete the lock file");
        let (mut child, _reader) = spawn_holder(temp.path());
        reap(&mut child);
    }
}
