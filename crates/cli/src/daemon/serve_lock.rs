// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! ONE serve owner per data-dir (AGENTS commandment 8, zero orphans).
//!
//! The IDE host generates a per-spawn password that dies with the host
//! process, so a daemon left behind by a reload is an unreachable zombie
//! that still owns the store: every later start contends with it (observed
//! as minutes-long SQLITE_BUSY storms and a permanent "service could not be
//! reached" card) while a fresh spawn makes the pile worse. The lock record
//! makes the pile impossible to grow silently:
//!
//! - `serve.lock` — created `O_EXCL` before the store opens; content
//!   `{"pid":N,"port":P}` with `port` filled in after the bind (atomically
//!   replaced, still an existing file).
//! - A live holder wins: a second `serve` prints the frozen
//!   `faktor already serving pid=<pid> port=<port>` line and exits 3 —
//!   the caller can health-check the winner with `<data-dir>/serve.password`
//!   (0600, written by the winner from `FAKTOR_SERVER_PASSWORD`) and REUSE
//!   it instead of spawning a competitor (this is how a second VS Code
//!   window joins the same daemon).
//! - A dead holder is stale: its lock is removed and the new serve acquires
//!   ownership. Process death releases nothing here (no kernel lock), so the
//!   pid-liveness probe IS the staleness detector; the drop guard removes
//!   both files on every graceful exit.
//!
//! Losing a race between "remove stale" and "someone else recreated it" is
//! safe: the winner's `O_EXCL` create exists and the loser re-reads it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// The ownership record inside the data-dir.
pub(crate) const LOCK_FILE: &str = "serve.lock";
/// The reattach credential, mode 0600, present only while the owner runs.
pub(crate) const PASSWORD_FILE: &str = "serve.password";
/// Exit code for a refused duplicate serve (never the generic 1).
pub(crate) const ALREADY_SERVING_EXIT: i32 = 3;

/// The frozen stdout line a refused duplicate serve prints. The VS Code
/// handshake parses exactly this shape; both strings must stay identical.
pub(crate) fn already_serving_line(pid: u32, port: u16) -> String {
    format!("faktor already serving pid={pid} port={port}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockRecord {
    pub pid: u32,
    pub port: u16,
}

#[derive(Debug)]
pub(crate) enum LockOutcome {
    /// This process now owns the data-dir; the guard releases on drop.
    Acquired(ServeLock),
    /// A live process already owns it; never spawn a competitor.
    AlreadyServing(LockRecord),
}

/// The ownership guard. Dropping it removes the lock and password files
/// (only while they still record THIS process, so a successor is never
/// clobbered).
#[derive(Debug)]
pub(crate) struct ServeLock {
    path: PathBuf,
    password_path: PathBuf,
    pid: u32,
}

impl Drop for ServeLock {
    fn drop(&mut self) {
        if let Some(record) = read_record(&self.path) {
            if record.pid == self.pid {
                let _ = fs::remove_file(&self.path);
                let _ = fs::remove_file(&self.password_path);
            }
        }
    }
}

/// Parse `{"pid":N,"port":P}` (tolerant of unknown extra fields).
pub(crate) fn read_record(path: &Path) -> Option<LockRecord> {
    let mut file = File::open(path).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = value.get("pid")?.as_u64()?;
    let port = value.get("port")?.as_u64()?;
    if pid == 0 || pid > u32::MAX as u64 || port > u16::MAX as u64 {
        return None;
    }
    Some(LockRecord {
        pid: pid as u32,
        port: port as u16,
    })
}

/// True when `pid` names a live process of this user.
///
/// The workspace denies `unsafe_code` outside the static-authority OS
/// seams; this is the process-existence seam for the data-dir singleton
/// (reviewed as `crates/cli/src/daemon/serve_lock.rs` in
/// `UNSAFE_ALLOWED_FILES`). A `kill(pid, 0)` existence probe — the signal
/// number IS the probe: nothing is delivered — beats rolling a `ps` spawn,
/// which the spawn authority forbids outside terminal/pty.
#[allow(unsafe_code)]
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `pid` comes from the on-disk lock record as a plain u32;
        // signal 0 performs permission/existence checking only and never
        // delivers a signal, so no handler, memory, or process state is
        // touched. EPERM proves the process EXISTS (another owner's), ESRCH
        // is the only "dead" verdict.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: OpenProcess receives only the pid from the lock record;
        // the returned handle is used for GetExitCodeProcess and ALWAYS
        // closed on every path; no other process state is read or written
        // and nothing is retained after the call.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        // Unknown platform: treat every holder as live (never steal a lock
        // we cannot prove stale).
        true
    }
}

/// Atomically rewrite `{"pid":pid,"port":port}` through the ONE
/// file-content replacement seam (`faktor_fs::atomic::atomic_replace`:
/// staged temp + fsync + rename + parent fsync).
fn write_record(path: &Path, pid: u32, port: u16) -> io::Result<()> {
    let payload = serde_json::json!({ "pid": pid, "port": port });
    faktor_fs::atomic::atomic_replace(path, payload.to_string().as_bytes())
        .map(|_| ())
        .map_err(|e| io::Error::other(format!("serve.lock write: {e}")))
}

/// Record the ACTUAL bound port for the CURRENT process's lock, called by
/// `serve_impl` the moment the frozen startup line is printed. A caller
/// without this process's record (test harnesses, a foreign owner) is a
/// silent no-op — never a writer.
pub(crate) fn record_bound_port(data_dir: &Path, port: u16) {
    let path = data_dir.join(LOCK_FILE);
    if let Some(record) = read_record(&path) {
        if record.pid == std::process::id() {
            let _ = write_record(&path, record.pid, port);
        }
    }
}

impl ServeLock {
    /// Persist the running password (0600 on unix) so a LATER host process
    /// can health-check and reuse this daemon instead of spawning over it.
    pub(crate) fn write_password(&self, password: &str) -> io::Result<()> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.password_path)?
        };
        #[cfg(not(unix))]
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.password_path)?;
        file.write_all(password.as_bytes())?;
        file.sync_all()
    }
}

/// Take ownership of `data_dir`, or learn who already owns it.
pub(crate) fn acquire(data_dir: &Path) -> io::Result<LockOutcome> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join(LOCK_FILE);
    let my_pid = std::process::id();
    // Bounded: each AlreadyExists-with-stale verdict consumes one attempt;
    // the loop only continues after removing an UNREADABLE/dead record.
    for _ in 0..3 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let payload = serde_json::json!({ "pid": my_pid, "port": 0 });
                file.write_all(payload.to_string().as_bytes())?;
                file.sync_all()?;
                return Ok(LockOutcome::Acquired(ServeLock {
                    path: path.clone(),
                    password_path: data_dir.join(PASSWORD_FILE),
                    pid: my_pid,
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match read_record(&path) {
                    Some(record) if record.pid != my_pid && pid_alive(record.pid) => {
                        return Ok(LockOutcome::AlreadyServing(record));
                    }
                    Some(_) | None => {
                        // Dead pid, our own record (a re-acquire in the same
                        // process, e.g. a test), or an unparsable file from
                        // a crashed writer: stale — take it over.
                        let _ = fs::remove_file(&path);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("could not establish {LOCK_FILE} ownership after bounded retries"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "faktor-serve-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_acquire_wins_and_drop_releases_both_files() {
        let dir = scratch();
        let lock = match acquire(&dir).unwrap() {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::AlreadyServing(record) => panic!("unexpected holder {record:?}"),
        };
        assert_eq!(
            read_record(&dir.join(LOCK_FILE)).unwrap().pid,
            std::process::id()
        );
        lock.write_password("pw-a").unwrap();
        assert_eq!(fs::read_to_string(dir.join(PASSWORD_FILE)).unwrap(), "pw-a");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(PASSWORD_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "password must not be group/world readable"
            );
        }
        drop(lock);
        assert!(!dir.join(LOCK_FILE).exists());
        assert!(!dir.join(PASSWORD_FILE).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Spawned by the live-holder test: a same-user process that stays up
    /// long enough to prove the liveness probe refuses a duplicate acquire.
    #[test]
    fn live_holder_helper() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    fn spawn_helper(test_name: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn helper test binary")
    }

    #[test]
    fn second_acquire_against_a_live_holder_reports_already_serving_with_port() {
        let dir = scratch();
        let holder = match acquire(&dir).unwrap() {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::AlreadyServing(record) => panic!("unexpected holder {record:?}"),
        };
        write_record(&dir.join(LOCK_FILE), std::process::id(), 43210).unwrap();
        let mut helper = spawn_helper("daemon::serve_lock::tests::live_holder_helper");
        let pid = helper.id();
        assert!(pid != 0);
        fs::write(
            dir.join(LOCK_FILE),
            serde_json::json!({ "pid": pid, "port": 43210 }).to_string(),
        )
        .unwrap();
        match acquire(&dir).unwrap() {
            LockOutcome::AlreadyServing(record) => {
                assert_eq!(record.pid, pid);
                assert_eq!(record.port, 43210);
                assert_eq!(
                    already_serving_line(record.pid, record.port),
                    format!("faktor already serving pid={pid} port=43210")
                );
            }
            LockOutcome::Acquired(_) => panic!("a live holder must refuse the duplicate"),
        }
        drop(holder);
        let _ = helper.kill();
        let _ = helper.wait();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dead_holder_is_stale_and_the_lock_is_taken_over() {
        let dir = scratch();
        let mut helper = spawn_helper("daemon::serve_lock::tests::live_holder_helper");
        let pid = helper.id();
        let _ = helper.kill();
        let _ = helper.wait();
        fs::write(
            dir.join(LOCK_FILE),
            serde_json::json!({ "pid": pid, "port": 0 }).to_string(),
        )
        .unwrap();
        match acquire(&dir).unwrap() {
            LockOutcome::Acquired(lock) => drop(lock),
            LockOutcome::AlreadyServing(record) => {
                panic!("dead pid {} reported as serving", record.pid)
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unparsable_record_is_stale_not_a_wedge() {
        let dir = scratch();
        fs::write(dir.join(LOCK_FILE), b"{not json").unwrap();
        match acquire(&dir).unwrap() {
            LockOutcome::Acquired(lock) => drop(lock),
            LockOutcome::AlreadyServing(record) => {
                panic!("garbage record reported as serving: {record:?}")
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_bound_port_replaces_atomically_and_keeps_ownership() {
        let dir = scratch();
        let lock = match acquire(&dir).unwrap() {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::AlreadyServing(_) => panic!("unexpected holder"),
        };
        write_record(&dir.join(LOCK_FILE), std::process::id(), 7).unwrap();
        record_bound_port(&dir, 8);
        assert_eq!(read_record(&dir.join(LOCK_FILE)).unwrap().port, 8);
        // A foreign owner's record is NEVER rewritten by our free function.
        write_record(&dir.join(LOCK_FILE), std::process::id() + 1, 9).unwrap();
        record_bound_port(&dir, 10);
        assert_eq!(read_record(&dir.join(LOCK_FILE)).unwrap().port, 9);
        // Restore our own pid so the drop guard removes the files.
        write_record(&dir.join(LOCK_FILE), std::process::id(), 8).unwrap();
        drop(lock);
        assert!(!dir.join(LOCK_FILE).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn already_serving_line_is_frozen() {
        assert_eq!(
            already_serving_line(4242, 59999),
            "faktor already serving pid=4242 port=59999"
        );
    }
}
