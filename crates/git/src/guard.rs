//! Repository mutation guard: ONE logical repository operation (commit /
//! exact-tree build / push preparation / worktree mutation) holds BOTH
//!
//! 1. the process-local per-repository write lock, and
//! 2. a durable disk lease under the repository's COMMON git dir — the one
//!    path shared by the main worktree and every linked worktree, so two
//!    runtimes (two daemons, two managers, a manager and an external Faktor
//!    process) serialize on the SAME lease.
//!
//! The lease records a pid-reuse-safe process identity: the pid, a process
//! start marker (`/proc/<pid>/stat` starttime on Linux, `ps -o lstart=` on
//! other unixes) and a random owner token. A lease whose owner is provably
//! dead, or alive but with a DIFFERENT start marker (the pid was reused), is
//! reconciled by an atomic rename-away — exactly one stale generation can be
//! stolen, and a live lease is never taken. Git's own index/ref locks stay
//! untouched: this guard orders Faktor's semantic operations, it does not
//! replace git's locking.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use faktor_core::error::{Error, ErrorKind};

/// The lease file name under the repository's common git dir.
pub const LEASE_FILE: &str = "faktor-mutation.lease";
/// Default budget one caller waits for a busy repository before giving up
/// with a typed conflict (the holder's identity is named in the error).
pub const DEFAULT_LEASE_BUDGET: Duration = Duration::from_secs(30);
/// A malformed/partially-written lease (a crashed writer's residue) is
/// reconciled once it is older than this ceiling.
const MALFORMED_LEASE_TTL: Duration = Duration::from_secs(600);
/// Retry interval while the lease is held by a provably live owner.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// The durable lease record: process identity + purpose + start time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseRecord {
    pub pid: u32,
    /// PID-reuse-safe process start marker; empty when the platform cannot
    /// report one (then only death proves staleness).
    #[serde(default)]
    pub pid_start_marker: String,
    /// Random per-acquisition token (so a released+reacquired lease is
    /// distinguishable, and release never deletes another owner's lease).
    pub owner: String,
    pub started_ms: i64,
    pub purpose: String,
}

fn io_failure(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::internal(format!("{what} {}: {e}", path.display()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The process start marker of `pid`: a stable string that differs when the
/// pid is reused by a different process. Empty when unavailable.
#[cfg(unix)]
pub fn pid_start_marker(pid: u32) -> String {
    // Linux fast path: field 22 (starttime) of /proc/<pid>/stat. The comm
    // field may contain spaces/parentheses, so parse after the last ')'.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) {
            let fields: Vec<&str> = rest.split_whitespace().collect();
            if let Some(starttime) = fields.get(19) {
                return format!("proc:{starttime}");
            }
        }
    }
    // macOS + fallback: `ps -o lstart= -p <pid>` prints the exact start time.
    match std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                String::new()
            } else {
                format!("ps:{text}")
            }
        }
        _ => String::new(),
    }
}

#[cfg(not(unix))]
pub fn pid_start_marker(_pid: u32) -> String {
    String::new()
}

/// Whether `pid` names a live process. Unices use `kill(pid, 0)` (EPERM
/// still means alive: it exists but belongs to someone else). Non-unix
/// platforms cannot prove liveness here and report `true` (the TTL ceiling
/// handles their stale leases).
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the existence/permission
    // check and has no side effect on the target.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn process_alive(pid: u32) -> bool {
    pid == std::process::id()
}

/// True when a lease is stale and may be stolen: its owner is dead, its pid
/// was reused by a different process, or it is an old malformed residue. A
/// live owner with a matching (or unverifiable but reported) marker is NEVER
/// stale.
fn lease_is_stale(record: &LeaseRecord) -> bool {
    if !process_alive(record.pid) {
        return true;
    }
    if record.pid_start_marker.is_empty() {
        return false;
    }
    let current = pid_start_marker(record.pid);
    !current.is_empty() && current != record.pid_start_marker
}

/// A held durable lease. Dropped by releasing it ONLY when the file still
/// carries this acquisition's owner token (a lease already stolen from a
/// dead-looking owner is never deleted by the old owner).
#[derive(Debug)]
pub struct DiskLease {
    path: PathBuf,
    record: LeaseRecord,
}

impl DiskLease {
    /// Acquire the durable lease inside `common_dir`, waiting up to `budget`
    /// for a live owner to release it. `purpose` is bounded diagnostic text.
    pub fn acquire(common_dir: &Path, purpose: &str, budget: Duration) -> Result<Self, Error> {
        if !common_dir.is_dir() {
            return Err(Error::not_found(format!(
                "repository common git dir {} is not a directory",
                common_dir.display()
            )));
        }
        let purpose: String = purpose.chars().take(200).collect();
        let record = LeaseRecord {
            pid: std::process::id(),
            pid_start_marker: pid_start_marker(std::process::id()),
            owner: uuid::Uuid::new_v4().to_string(),
            started_ms: now_ms(),
            purpose,
        };
        let path = common_dir.join(LEASE_FILE);
        let deadline = Instant::now() + budget;
        loop {
            match try_create(&path, &record)? {
                LeaseAttempt::Acquired => {
                    crate::fsync_dir(common_dir);
                    return Ok(DiskLease { path, record });
                }
                LeaseAttempt::Live(holder) => {
                    if Instant::now() >= deadline {
                        return Err(Error::new(
                            ErrorKind::Conflict,
                            format!(
                                "repository mutation lease {} is held by pid {} ({}) and was not released within {:?}",
                                path.display(),
                                holder.pid,
                                holder.purpose,
                                budget
                            ),
                        ));
                    }
                    std::thread::sleep(
                        RETRY_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                LeaseAttempt::Reconcile => {
                    // Atomic claim of a stale generation: exactly one stealer
                    // wins the rename; losers retry the create.
                    let tombstone = common_dir.join(format!(
                        "{LEASE_FILE}.stale.{}.{}",
                        std::process::id(),
                        uuid::Uuid::new_v4()
                    ));
                    match std::fs::rename(&path, &tombstone) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(&tombstone);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(io_failure("reconcile stale lease", &path, e));
                        }
                    }
                }
            }
        }
    }

    /// The lease path (diagnostics/tests).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The recorded owner identity (diagnostics/tests).
    pub fn record(&self) -> &LeaseRecord {
        &self.record
    }
}

impl Drop for DiskLease {
    fn drop(&mut self) {
        // Release only OUR lease: a lease that was reconciled/stolen and
        // re-acquired by someone else must never be deleted from here.
        if let Ok(raw) = std::fs::read(&self.path) {
            if let Ok(current) = serde_json::from_slice::<LeaseRecord>(&raw) {
                if current.owner == self.record.owner && current.pid == self.record.pid {
                    let _ = std::fs::remove_file(&self.path);
                    if let Some(parent) = self.path.parent() {
                        crate::fsync_dir(parent);
                    }
                }
            }
        }
    }
}

enum LeaseAttempt {
    /// `create_new` succeeded: this acquisition owns the lease.
    Acquired,
    /// A provably live owner holds the lease.
    Live(LeaseRecord),
    /// The recorded owner is stale (or the residue is malformed/old).
    Reconcile,
}

fn try_create(path: &Path, record: &LeaseRecord) -> Result<LeaseAttempt, Error> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            let mut payload = serde_json::to_vec(record).unwrap_or_default();
            payload.push(b'\n');
            let write = file
                .write_all(&payload)
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all());
            match write {
                Ok(()) => Ok(LeaseAttempt::Acquired),
                Err(e) => {
                    let _ = std::fs::remove_file(path);
                    Err(io_failure("write lease", path, e))
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let raw = std::fs::read(path).ok();
            let parsed: Option<LeaseRecord> =
                raw.as_deref().and_then(|b| serde_json::from_slice(b).ok());
            match &parsed {
                Some(held) if !lease_is_stale(held) => Ok(LeaseAttempt::Live(held.clone())),
                Some(_) => Ok(LeaseAttempt::Reconcile),
                None => {
                    // Unreadable/malformed residue: reconcile only after the
                    // TTL ceiling, so a writer mid-create is never stolen.
                    let old = raw.is_none()
                        || std::fs::metadata(path)
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.elapsed().ok())
                            .is_some_and(|age| age > MALFORMED_LEASE_TTL);
                    if old {
                        Ok(LeaseAttempt::Reconcile)
                    } else {
                        Ok(LeaseAttempt::Live(LeaseRecord {
                            pid: 0,
                            pid_start_marker: String::new(),
                            owner: "unknown".into(),
                            started_ms: now_ms(),
                            purpose: "a malformed lease residue".into(),
                        }))
                    }
                }
            }
        }
        Err(e) => Err(io_failure("create lease", path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lease(dir: &Path, record: &LeaseRecord) {
        std::fs::write(dir.join(LEASE_FILE), serde_json::to_vec(record).unwrap()).unwrap();
    }

    #[test]
    fn lease_roundtrip_release_only_removes_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DiskLease::acquire(dir.path(), "test", Duration::from_millis(200)).unwrap();
        let path = lease.path().to_path_buf();
        assert!(path.exists());
        // A second acquisition with a small budget conflicts (live owner).
        let err = match DiskLease::acquire(dir.path(), "other", Duration::from_millis(150)) {
            Ok(_) => panic!("a live lease must not be stolen"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(err.message.contains("is held by pid"), "{err:?}");
        assert!(
            path.exists(),
            "a failed acquisition never deletes the lease"
        );
        // The owner can re-enter after release.
        drop(lease);
        assert!(!path.exists(), "the owner releases its own lease");
        let again = DiskLease::acquire(dir.path(), "again", Duration::from_millis(200)).unwrap();
        // A foreign writer replacing the file behind its back must not be
        // deleted by the old owner's drop.
        let foreign = LeaseRecord {
            pid: std::process::id(),
            pid_start_marker: pid_start_marker(std::process::id()),
            owner: "foreign-token".into(),
            started_ms: now_ms(),
            purpose: "someone else".into(),
        };
        std::fs::write(again.path(), serde_json::to_vec(&foreign).unwrap()).unwrap();
        drop(again);
        assert!(
            dir.path().join(LEASE_FILE).exists(),
            "the old owner must not delete a lease carrying another token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dead_owner_lease_is_reconciled_and_a_reused_pid_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        // A genuinely dead process (spawned + exited) cannot hold the lease.
        let child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        let mut child = child;
        let _ = child.wait();
        assert!(!process_alive(dead_pid));
        write_lease(
            dir.path(),
            &LeaseRecord {
                pid: dead_pid,
                pid_start_marker: "proc:whatever".into(),
                owner: "dead-owner".into(),
                started_ms: now_ms() - 60_000,
                purpose: "crashed runtime".into(),
            },
        );
        let lease = DiskLease::acquire(dir.path(), "reconcile-dead", Duration::from_millis(500))
            .expect("a dead owner's lease is reconciled");
        assert_eq!(lease.record().purpose, "reconcile-dead");
        drop(lease);
        // A LIVE pid with a start marker that does not match (pid reuse) is
        // stale too — but a live pid with the CORRECT marker is never stolen.
        write_lease(
            dir.path(),
            &LeaseRecord {
                pid: std::process::id(),
                pid_start_marker: "ps:not-the-real-start".into(),
                owner: "reused-pid".into(),
                started_ms: now_ms() - 5_000,
                purpose: "reused".into(),
            },
        );
        let lease = DiskLease::acquire(dir.path(), "reconcile-reused", Duration::from_millis(500))
            .expect("a mismatched start marker is a reused pid and is stale");
        drop(lease);
        let live = DiskLease::acquire(dir.path(), "live", Duration::from_millis(200)).unwrap();
        let held: LeaseRecord =
            serde_json::from_slice(&std::fs::read(dir.path().join(LEASE_FILE)).unwrap()).unwrap();
        assert_eq!(held.pid_start_marker, pid_start_marker(std::process::id()));
        let err = match DiskLease::acquire(dir.path(), "nope", Duration::from_millis(120)) {
            Ok(_) => panic!("a live matching lease must never be stolen"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::Conflict);
        drop(live);
    }
}
