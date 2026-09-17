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
//! creation identity (`/proc/<pid>/stat` starttime on Linux, kernel
//! `proc_pidinfo` on macOS, `OpenProcess` + `GetProcessTimes` creation
//! FILETIME on Windows — never a child process) and a random owner token. A
//! lease whose owner is provably dead, or alive but carrying a DIFFERENT
//! creation identity (the pid was reused), is reconciled by an atomic
//! rename-away — exactly one stale generation can be stolen, and a live
//! lease is never taken. An ACCESS_DENIED or otherwise unobservable identity
//! is UNKNOWN and must be treated as LIVE: only proven death or a proven
//! pid-generation mismatch may reclaim. Git's own index/ref locks stay
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
    /// PID-reuse-safe process creation time in milliseconds since the Unix
    /// epoch (Windows `GetProcessTimes` creation FILETIME); 0 when the
    /// platform cannot report one (unices compare `pid_start_marker`).
    #[serde(default)]
    pub pid_created_ms: i64,
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

/// The platform's answer to "which process generation, if any, owns `pid`?".
/// Produced by [`observe_process`] and consumed by the PURE [`lease_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveResult {
    /// `pid` names a live process; `created_ms` is its creation time in
    /// milliseconds since the Unix epoch when the platform can report one
    /// (`0` = the process exists but its creation time is unobservable).
    Alive { created_ms: i64 },
    /// The process could not be observed (`EPERM` / `ERROR_ACCESS_DENIED` /
    /// any indeterminate query failure). UNKNOWN identity is NEVER proof of
    /// death — it must be treated as live.
    AccessDenied,
    /// The platform proves `pid` names no process (including pid 0, which is
    /// never a real lease owner: on Unix it addresses a process group).
    InvalidPid,
}

/// Why a lease may be reclaimed as stale without a proven death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimReason {
    /// The pid is alive but its creation time differs from the recorded one:
    /// the pid was reused by a different process generation.
    PidReused,
}

/// The liveness verdict for a recorded lease owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The owner is alive, or aliveness cannot be disproven: NEVER steal.
    Alive,
    /// The owner provably exited (`InvalidPid`): the lease is stale.
    Dead,
    /// The pid generation changed ([`ReclaimReason::PidReused`]): stale.
    Reclaim(ReclaimReason),
}

/// PURE lease-liveness decision over a synthetic observation: the whole
/// cross-process safety argument lives here and is unit-testable on every
/// platform.
///
/// - `InvalidPid` is the ONLY observation that proves death.
/// - `AccessDenied` / an unobservable creation time is UNKNOWN => `Alive`.
/// - A live pid whose creation time differs from the recorded one is a reused
///   pid generation => `Reclaim(PidReused)`.
/// - A missing (0) recorded or observed creation time can never prove a
///   mismatch => `Alive` (only death may reclaim such a lease).
pub fn lease_verdict(owner_pid: u32, owner_created_ms: i64, observed: ObserveResult) -> Verdict {
    if owner_pid == 0 {
        return Verdict::Dead;
    }
    match observed {
        ObserveResult::InvalidPid => Verdict::Dead,
        ObserveResult::AccessDenied => Verdict::Alive,
        ObserveResult::Alive { created_ms } => {
            if owner_created_ms != 0 && created_ms != 0 && created_ms != owner_created_ms {
                Verdict::Reclaim(ReclaimReason::PidReused)
            } else {
                Verdict::Alive
            }
        }
    }
}

/// macOS: the process start time from `proc_pidinfo(PROC_PIDTBSDINFO)` —
/// a direct kernel query, never a child process (process identity must not
/// add a spawn site to a production crate; the static spawn authority scan
/// enforces that).
#[cfg(target_os = "macos")]
fn unix_pid_start_marker(pid: u32) -> String {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            size,
        )
    };
    if rc == size {
        format!("bsd:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec)
    } else {
        String::new()
    }
}

/// Other unixes: no start marker is reported (the lease then proves
/// staleness by death only, exactly like the documented unavailable case).
#[cfg(all(unix, not(target_os = "macos")))]
fn unix_pid_start_marker(_pid: u32) -> String {
    String::new()
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
    unix_pid_start_marker(pid)
}

#[cfg(not(unix))]
pub fn pid_start_marker(_pid: u32) -> String {
    String::new()
}

/// Convert a Windows FILETIME (100 ns ticks since 1601-01-01) to Unix epoch
/// milliseconds (0 when the timestamp precedes the epoch).
#[cfg(windows)]
fn filetime_to_unix_ms(ft: windows_sys::Win32::Foundation::FILETIME) -> i64 {
    /// 11644473600 s between 1601-01-01 and 1970-01-01, in 100 ns ticks.
    const EPOCH_DELTA_100NS: u64 = 11_644_473_600_000_000;
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (ticks.saturating_sub(EPOCH_DELTA_100NS) / 10_000) as i64
}

/// Windows identity: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` +
/// `GetProcessTimes` creation FILETIME, compared as a (pid, creation-time)
/// pair. A pid that no longer exists fails `OpenProcess` with
/// `ERROR_INVALID_PARAMETER` (proven death); a protected or otherwise
/// unopenable process yields `ERROR_ACCESS_DENIED` (UNKNOWN identity => live);
/// any other failure is UNKNOWN too, never death.
#[cfg(windows)]
fn platform_observe_process(pid: u32) -> ObserveResult {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, FILETIME, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return ObserveResult::InvalidPid;
    }
    // SAFETY: OpenProcess only asks for query access; the handle is closed
    // exactly once on every path below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        // ERROR_ACCESS_DENIED (and every other failure) is UNKNOWN identity,
        // never proof of death.
        return match unsafe { GetLastError() } {
            ERROR_INVALID_PARAMETER => ObserveResult::InvalidPid,
            _ => ObserveResult::AccessDenied,
        };
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: all four out-params are valid writable FILETIMEs for the
    // duration of the call.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: the handle came from OpenProcess and is closed exactly once.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        // Opened but the query failed (e.g. the process exited between the
        // open and the query): UNKNOWN, never death.
        return ObserveResult::AccessDenied;
    }
    ObserveResult::Alive {
        created_ms: filetime_to_unix_ms(creation),
    }
}

/// Unix identity: `kill(pid, 0)` for existence (`EPERM` = it exists but
/// belongs to someone else => UNKNOWN-alive). The creation channel is the
/// string [`pid_start_marker`], checked by [`lease_check`], so `created_ms`
/// is 0 here.
#[cfg(unix)]
fn platform_observe_process(pid: u32) -> ObserveResult {
    if pid == 0 {
        return ObserveResult::InvalidPid;
    }
    // SAFETY: kill with signal 0 performs only the existence/permission
    // check and has no side effect on the target.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return ObserveResult::Alive { created_ms: 0 };
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ObserveResult::InvalidPid,
        // EPERM and every indeterminate failure are UNKNOWN, never death.
        _ => ObserveResult::AccessDenied,
    }
}

/// Platforms without a process-table query: identity is UNKNOWN => live
/// (pid 0 stays invalid, matching the placeholder-record rule).
#[cfg(all(not(unix), not(windows)))]
fn platform_observe_process(pid: u32) -> ObserveResult {
    if pid == 0 {
        ObserveResult::InvalidPid
    } else {
        ObserveResult::AccessDenied
    }
}

#[cfg(test)]
std::thread_local! {
    /// Test-only observation override, keyed to ONE pid on THIS thread so
    /// synthetic ACCESS_DENIED / creation-time observations never leak into
    /// parallel tests.
    static INJECTED_OBSERVATION: std::cell::RefCell<Option<(u32, ObserveResult)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn injected_observation(pid: u32) -> Option<ObserveResult> {
    INJECTED_OBSERVATION.with(|slot| match &*slot.borrow() {
        Some((injected_pid, result)) if *injected_pid == pid => Some(*result),
        _ => None,
    })
}

/// Install a synthetic observation for `pid` on the current thread (tests
/// only: the OS cannot be asked for ACCESS_DENIED on demand).
#[cfg(test)]
fn inject_observation(pid: u32, result: ObserveResult) {
    INJECTED_OBSERVATION.with(|slot| *slot.borrow_mut() = Some((pid, result)));
}

#[cfg(test)]
fn clear_injected_observation() {
    INJECTED_OBSERVATION.with(|slot| *slot.borrow_mut() = None);
}

/// Observe `pid` through the platform (tests may inject a synthetic result).
fn observe_process(pid: u32) -> ObserveResult {
    #[cfg(test)]
    {
        if let Some(injected) = injected_observation(pid) {
            return injected;
        }
    }
    platform_observe_process(pid)
}

/// Whether `pid` names a live process. Unices use `kill(pid, 0)` (EPERM
/// still means alive: it exists but belongs to someone else). Windows uses
/// `OpenProcess` + `GetProcessTimes`: a pid that no longer exists is dead,
/// while ACCESS_DENIED / unobservable identity is ALIVE (never provably
/// dead).
#[cfg(any(unix, windows))]
pub fn process_alive(pid: u32) -> bool {
    !matches!(platform_observe_process(pid), ObserveResult::InvalidPid)
}

/// No process-table query exists on this platform: unknown is alive.
#[cfg(all(not(unix), not(windows)))]
pub fn process_alive(pid: u32) -> bool {
    pid != 0
}

/// Decide whether `record`'s owner is stale through the pure [`lease_verdict`]
/// over the platform observation, plus the legacy unix string-marker
/// generation check for leases that carry no numeric creation time. An
/// ACCESS_DENIED / unobservable observation short-circuits to `Alive` and is
/// never turned into a reclaim by a secondary probe.
fn lease_check(record: &LeaseRecord) -> Verdict {
    let observed = observe_process(record.pid);
    let verdict = lease_verdict(record.pid, record.pid_created_ms, observed);
    if verdict == Verdict::Alive
        && matches!(observed, ObserveResult::Alive { .. })
        && record.pid_created_ms == 0
        && !record.pid_start_marker.is_empty()
    {
        let current = pid_start_marker(record.pid);
        if !current.is_empty() && current != record.pid_start_marker {
            return Verdict::Reclaim(ReclaimReason::PidReused);
        }
    }
    verdict
}

/// True when a lease is stale and may be stolen: its owner provably died, its
/// pid was reused by a different process generation, or it is an old
/// malformed residue. A live owner — or one whose identity is UNKNOWN
/// (ACCESS_DENIED/unobservable) — is NEVER stale.
fn lease_is_stale(record: &LeaseRecord) -> bool {
    lease_check(record) != Verdict::Alive
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
        let pid = std::process::id();
        let record = LeaseRecord {
            pid,
            pid_start_marker: pid_start_marker(pid),
            pid_created_ms: match observe_process(pid) {
                ObserveResult::Alive { created_ms } => created_ms,
                // The OS cannot describe us: 0 means "no creation identity",
                // never a false identity.
                _ => 0,
            },
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

/// Serialize a lease record for publication. A serialization failure is a
/// typed refusal: the caller must never publish empty bytes (an empty lease
/// would look like a crashed writer's residue and block every other owner
/// for the malformed TTL).
fn lease_payload<T: serde::Serialize>(record: &T) -> Result<Vec<u8>, Error> {
    let mut payload = serde_json::to_vec(record)
        .map_err(|e| Error::internal(format!("serialize lease record: {e}")))?;
    payload.push(b'\n');
    Ok(payload)
}

fn try_create<T: serde::Serialize>(path: &Path, record: &T) -> Result<LeaseAttempt, Error> {
    // Serialize BEFORE creating the file: a record that cannot be serialized
    // refuses to write a lease, and no empty/partial residue is left behind.
    let payload = lease_payload(record)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
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
                            pid_created_ms: 0,
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

    /// Installs a synthetic observation for `pid` and clears it on drop
    /// (panic-safe), so a failed assertion cannot leak it into another test.
    struct Injected;

    impl Injected {
        fn install(pid: u32, observed: ObserveResult) -> Self {
            inject_observation(pid, observed);
            Self
        }
    }

    impl Drop for Injected {
        fn drop(&mut self) {
            clear_injected_observation();
        }
    }

    #[test]
    fn lease_roundtrip_release_only_removes_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DiskLease::acquire(dir.path(), "test", Duration::from_millis(200)).unwrap();
        let path = lease.path().to_path_buf();
        assert!(path.exists());
        assert_eq!(lease.record().pid, std::process::id());
        assert!(
            !lease.record().pid_start_marker.is_empty() || lease.record().pid_created_ms != 0,
            "the writer must persist a pid-reuse-safe process identity: {:?}",
            lease.record()
        );
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
            pid_created_ms: 0,
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

    #[test]
    fn lease_verdict_matrix_unknown_is_live_and_only_proof_reclaims() {
        // Proven death is the ONLY observation that proves death.
        assert_eq!(
            lease_verdict(1234, 100, ObserveResult::InvalidPid),
            Verdict::Dead
        );
        // ACCESS_DENIED / unobservable identity is UNKNOWN: never stale.
        assert_eq!(
            lease_verdict(1234, 100, ObserveResult::AccessDenied),
            Verdict::Alive
        );
        // Same pid + same creation time: the recorded owner is alive.
        assert_eq!(
            lease_verdict(1234, 100, ObserveResult::Alive { created_ms: 100 }),
            Verdict::Alive
        );
        // Same pid + different creation time: the pid generation was reused.
        assert_eq!(
            lease_verdict(1234, 100, ObserveResult::Alive { created_ms: 200 }),
            Verdict::Reclaim(ReclaimReason::PidReused)
        );
        // A missing recorded creation time can never prove a mismatch.
        assert_eq!(
            lease_verdict(1234, 0, ObserveResult::Alive { created_ms: 200 }),
            Verdict::Alive
        );
        // An unobservable observed creation time can never prove a mismatch.
        assert_eq!(
            lease_verdict(1234, 100, ObserveResult::Alive { created_ms: 0 }),
            Verdict::Alive
        );
        // pid 0 is never a real owner (kill(0, ..) addresses a process group).
        assert_eq!(
            lease_verdict(0, 100, ObserveResult::AccessDenied),
            Verdict::Dead
        );
        assert_eq!(
            lease_verdict(0, 0, ObserveResult::InvalidPid),
            Verdict::Dead
        );
    }

    #[test]
    fn lease_writer_refuses_when_the_record_cannot_be_serialized() {
        struct Unserializable;
        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(<S::Error as serde::ser::Error>::custom("synthetic failure"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEASE_FILE);
        let err = match try_create(&path, &Unserializable) {
            Ok(_) => panic!("a record that cannot be serialized must refuse to write a lease"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::Internal, "{err:?}");
        assert!(err.message.contains("serialize lease record"), "{err:?}");
        assert!(
            !path.exists(),
            "no empty/partial lease file may be published"
        );
        // The directory stays usable: a real record then acquires normally.
        let lease =
            DiskLease::acquire(dir.path(), "after-refusal", Duration::from_millis(200)).unwrap();
        assert_eq!(lease.record().purpose, "after-refusal");
    }

    #[test]
    fn injected_access_denied_on_a_dead_pid_is_unknown_not_dead() {
        let dir = tempfile::tempdir().unwrap();
        // A genuinely dead pid: without the injection its lease is stale.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();
        let _injected = Injected::install(dead_pid, ObserveResult::AccessDenied);
        write_lease(
            dir.path(),
            &LeaseRecord {
                pid: dead_pid,
                pid_start_marker: String::new(),
                pid_created_ms: 0,
                owner: "unobservable-owner".into(),
                started_ms: now_ms() - 3_600_000,
                purpose: "unobservable".into(),
            },
        );
        let err = match DiskLease::acquire(dir.path(), "must-not-steal", Duration::from_millis(250))
        {
            Ok(_) => panic!("ACCESS_DENIED must never be treated as proof of death"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::Conflict, "{err:?}");
        assert!(err.message.contains("is held by pid"), "{err:?}");
        assert!(
            dir.path().join(LEASE_FILE).exists(),
            "the unobservable owner's lease is left untouched"
        );
    }

    #[test]
    fn injected_creation_mismatch_reclaims_the_old_pid_generation() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        write_lease(
            dir.path(),
            &LeaseRecord {
                pid: me,
                pid_start_marker: String::new(),
                pid_created_ms: 1_000,
                owner: "old-generation".into(),
                started_ms: now_ms(),
                purpose: "old generation".into(),
            },
        );
        // Same pid, LATER creation time: the old generation is provably gone
        // even though a live process bears the pid now.
        let _injected = Injected::install(me, ObserveResult::Alive { created_ms: 2_000 });
        let lease = DiskLease::acquire(dir.path(), "new-generation", Duration::from_millis(500))
            .expect("a proven pid-generation mismatch is reclaimable");
        assert_eq!(lease.record().pid, me);
        assert_eq!(lease.record().purpose, "new-generation");
        assert_ne!(lease.record().owner, "old-generation");
        assert_eq!(lease.record().pid_created_ms, 2_000);
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
                pid_created_ms: 0,
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
                pid_created_ms: 0,
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

    // ------------------------------------------- Windows cross-process lease
    //
    // The P0 this pins: a second LIVE Faktor process's lease must never look
    // dead. These tests drive real Windows processes; the portable decision
    // logic is covered by the injected-observation tests above (darwin lane).

    #[cfg(windows)]
    const HELPER_DIR_ENV: &str = "FAKTOR_LEASE_HELPER_DIR";
    #[cfg(windows)]
    const READY_FILE: &str = "helper-ready";
    #[cfg(windows)]
    const HELPER_TEST: &str = "guard::tests::windows_lease_holder_helper";

    /// Ignored helper process for the Windows multiprocess tests below: it
    /// acquires the lease, backdates its `started_ms` (so the parent sees an
    /// "old" but live lease), announces readiness, and holds until killed.
    /// Driven by path with `--ignored --exact`, never by the normal run.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper process; driven by the windows multiprocess lease tests"]
    fn windows_lease_holder_helper() {
        let Ok(dir) = std::env::var(HELPER_DIR_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let lease = DiskLease::acquire(&dir, "helper-holder", Duration::from_secs(30))
            .expect("the helper acquires the lease");
        let mut backdated = lease.record().clone();
        backdated.started_ms = now_ms() - 3_600_000;
        std::fs::write(lease.path(), serde_json::to_vec(&backdated).unwrap())
            .expect("the helper backdates its lease");
        std::fs::write(dir.join(READY_FILE), &lease.record().owner)
            .expect("the helper announces readiness");
        // Hold until the parent kills us (bounded so a leaked helper heals).
        std::thread::sleep(Duration::from_secs(120));
    }

    #[cfg(windows)]
    fn read_lease(dir: &Path) -> LeaseRecord {
        serde_json::from_slice(&std::fs::read(dir.join(LEASE_FILE)).unwrap()).unwrap()
    }

    /// A real second process holding the durable lease (A).
    #[cfg(windows)]
    struct LeaseHolder(std::process::Child);

    #[cfg(windows)]
    impl LeaseHolder {
        fn spawn(dir: &Path) -> Self {
            let exe = std::env::current_exe().expect("the test executable path");
            let child = std::process::Command::new(exe)
                .args(["--ignored", "--exact", HELPER_TEST, "--nocapture"])
                .env(HELPER_DIR_ENV, dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn the lease holder helper");
            let mut holder = Self(child);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !dir.join(READY_FILE).exists() {
                if let Some(status) = holder.0.try_wait().expect("poll the helper") {
                    panic!("the lease holder helper exited before acquiring: {status:?}");
                }
                assert!(
                    Instant::now() < deadline,
                    "the lease holder helper never became ready"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            holder
        }

        /// Terminate A WITHOUT its release path (a crashed runtime): the
        /// durable lease file survives. Consumes the holder so the process
        /// handle is closed and the pid provably disappears.
        fn kill(mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(windows)]
    impl Drop for LeaseHolder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A acquires, and B is refused even though the held lease LOOKS old: a
    /// live foreign owner is never stale (the P0 regression test).
    #[cfg(windows)]
    #[test]
    fn windows_live_foreign_holder_lease_is_never_stolen_even_when_old() {
        let dir = tempfile::tempdir().unwrap();
        let mut holder = LeaseHolder::spawn(dir.path());
        let held = read_lease(dir.path());
        assert_ne!(
            held.pid,
            std::process::id(),
            "the holder is a foreign process"
        );
        assert!(
            !held.pid_start_marker.is_empty() || held.pid_created_ms != 0,
            "the holder's lease carries a process identity: {held:?}"
        );
        assert!(
            held.started_ms <= now_ms() - 1_800_000,
            "the held lease looks old: {}",
            held.started_ms
        );
        let err = match DiskLease::acquire(dir.path(), "b-refused", Duration::from_millis(400)) {
            Ok(_) => panic!("a live foreign owner's lease must never be stolen"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::Conflict, "{err:?}");
        assert!(
            err.message.contains(&format!("pid {}", held.pid)),
            "{err:?}"
        );
        let after = read_lease(dir.path());
        assert_eq!(
            after.owner, held.owner,
            "the live holder's lease stays intact"
        );
        assert_eq!(after.pid, held.pid);
        assert!(
            holder.0.try_wait().unwrap().is_none(),
            "the holder is still alive"
        );
    }

    /// A exits (terminated, no release): B acquires the reclaimed lease.
    #[cfg(windows)]
    #[test]
    fn windows_dead_holders_lease_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let holder = LeaseHolder::spawn(dir.path());
        let held = read_lease(dir.path());
        holder.kill();
        let lease = DiskLease::acquire(dir.path(), "b-after-death", Duration::from_secs(10))
            .expect("a provably dead owner's lease is reclaimed");
        assert_eq!(lease.record().pid, std::process::id());
        assert_ne!(lease.record().owner, held.owner);
        assert_eq!(lease.record().purpose, "b-after-death");
    }

    /// The Windows creation FILETIME is compared as Unix epoch milliseconds:
    /// a drifted conversion would silently defeat the pid-generation check.
    #[cfg(windows)]
    #[test]
    fn windows_creation_filetime_converts_to_unix_epoch_ms() {
        use windows_sys::Win32::Foundation::FILETIME;
        const EPOCH_TICKS: u64 = 11_644_473_600_000_000;
        let from_ticks = |ticks: u64| FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        assert_eq!(filetime_to_unix_ms(from_ticks(EPOCH_TICKS)), 0);
        assert_eq!(
            filetime_to_unix_ms(from_ticks(EPOCH_TICKS + 1_234 * 10_000)),
            1_234
        );
    }

    /// Same pid, different creation time observed by the REAL Windows API:
    /// the old generation's lease is reclaimable although the pid is alive.
    #[cfg(windows)]
    #[test]
    fn windows_same_pid_with_a_different_creation_time_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let real = match platform_observe_process(me) {
            ObserveResult::Alive { created_ms } if created_ms != 0 => created_ms,
            other => panic!("this test needs an observable creation time, got {other:?}"),
        };
        write_lease(
            dir.path(),
            &LeaseRecord {
                pid: me,
                pid_start_marker: String::new(),
                pid_created_ms: real - 1,
                owner: "previous-generation".into(),
                started_ms: now_ms(),
                purpose: "previous generation".into(),
            },
        );
        let lease = DiskLease::acquire(dir.path(), "reclaimed", Duration::from_millis(500))
            .expect("same pid + different creation time is a reused pid");
        assert_eq!(lease.record().pid_created_ms, real);
        assert_eq!(lease.record().purpose, "reclaimed");
    }
}
