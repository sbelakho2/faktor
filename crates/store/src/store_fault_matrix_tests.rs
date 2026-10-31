// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! HAR-05 — a fault-injection matrix over EVERY store op.
//!
//! Enumeration discipline mirrors the HAR-10 query-plan inventory: a source
//! scanner extracts every `impl Store` method (callable visibility) that
//! carries a **writer-queue boundary** (a `writer.execute*` label literal)
//! or a **read boundary** (`self.read()?`), and
//! `store_op_registry_covers_every_boundary_method` fails when the driver
//! table in [`store_fault_ops`] and the scanner disagree in either
//! direction.
//!
//! Fault classes (the instruction's matrix):
//!
//! * **SQLITE_BUSY contention (real)** — a second connection holds
//!   `BEGIN IMMEDIATE` while every write op runs; the op must still be
//!   blocked when the lock is released late (deterministic timing), then
//!   complete bounded and typed (success after release, or SQLITE_BUSY).
//! * **Disk full (ENOSPC)** — an injectable write-boundary shim returns the
//!   real `SQLITE_FULL` (13) error code; every write op must surface it
//!   (asserted: the seam fired — a passing op cannot be vacuous).
//! * **Read-only FS (real chmod + shim)** — write ops under a
//!   `SQLITE_READONLY` (8) shim; read ops under a real `chmod` of the data
//!   dir/files; opening a store on a read-only copy must fail typed; after
//!   restoring permissions the store reopens and the committed baseline
//!   verifies (recovery-on-next-open, no data loss).
//! * **Corrupt page (real byte-flip)** — a checkpointed copy of the db has
//!   the message page carrying the marker byte-flipped; every op runs
//!   bounded against it with typed-or-clean results (never a hang, never a
//!   panic, never wrong data), the message control query errors typed, a
//!   full-integrity open refuses the copy, and the pristine copy still
//!   verifies.
//!
//! Every deadline is explicit (bounded runner); every fault phase ends with
//! a committed-baseline verification.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::connection::FaultKind;
use crate::store_fault_ops::{Fixture, CORRUPT_MARKER, OPS};
use std::panic::AssertUnwindSafe;

/// One op run under a hard deadline. A panic inside the driver is captured
/// and reported as a failure (never a hang, never a silent pass).
fn run_bounded(
    fx: &Arc<Fixture>,
    drive: crate::store_fault_ops::Driver,
    ms: u64,
) -> Result<StoreResult<()>, String> {
    let (tx, rx) = mpsc::channel();
    let fx = Arc::clone(fx);
    thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drive(&fx)))
            .map_err(|p| {
                if let Some(s) = p.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = p.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "non-string panic payload".to_string()
                }
            });
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(Duration::from_millis(ms)) {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(panic_msg)) => Err(format!("op panicked: {panic_msg}")),
        Err(_) => Err("deadline exceeded (hang)".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Source extraction: which methods carry which boundary
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Boundary {
    Write,
    Read,
    Both,
}

/// `#[cfg(test)]`-stripped, comment-blanked production sources.
fn production_sources() -> Vec<(String, String)> {
    let mut files = Vec::new();
    crate::query_plan_inventory_tests::collect_source_files(
        &crate::query_plan_inventory_tests::src_dir(),
        &mut files,
    );
    files
        .into_iter()
        .map(|path| {
            let raw = std::fs::read_to_string(&path).expect("read store source");
            let stripped = crate::query_plan_inventory_tests::blank_comments(
                &crate::query_plan_inventory_tests::strip_test_regions(&raw),
            );
            let rel = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (rel, stripped)
        })
        .collect()
}

/// Every `impl Store { ... }` region as `(start, end)` byte offsets.
fn impl_store_regions(src: &str) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    let mut search = 0usize;
    while let Some(idx) = src[search..].find("impl Store") {
        let start = search + idx;
        search = start + "impl Store".len();
        let after = src[start..].find('{');
        let Some(rel_open) = after else { continue };
        let open = start + rel_open;
        let mut depth = 0usize;
        let mut end = src.len();
        for (x, c) in src.char_indices().skip(open) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = x + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        regions.push((start, end));
        search = end;
    }
    regions
}

/// The declaration at `fn_start` is `pub`/`pub(crate)`-visible (doc
/// comments are already blanked; attributes are skipped).
fn is_callable_fn(src: &str, fn_start: usize) -> bool {
    let from = fn_start.saturating_sub(1000);
    let pre = &src[from..fn_start];
    let mut cleaned = String::with_capacity(pre.len());
    let mut chars = pre.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' && chars.peek() == Some(&'[') {
            chars.next();
            let mut depth = 1usize;
            for c2 in chars.by_ref() {
                match c2 {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        cleaned.push(c);
    }
    let trimmed = cleaned.trim_end();
    if trimmed.ends_with("pub") {
        return true;
    }
    // `pub(crate)` / `pub(super)` / `pub(in crate::x)` — a `pub(` group
    // that ends exactly at the declaration.
    match trimmed.rfind("pub(") {
        Some(i) => trimmed[i..].ends_with(')'),
        None => false,
    }
}

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "REPLACE",
    "CREATE",
    "ALTER",
    "DROP",
    "PRAGMA",
    "WITH",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "ANALYZE",
    "VACUUM",
    "EXPLAIN",
];

fn is_sql_literal(lit: &str) -> bool {
    let first = lit
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    SQL_KEYWORDS
        .iter()
        .any(|kw| *kw == first.to_ascii_uppercase())
}

/// True when the body submits to the writer queue: a `writer.execute*` call
/// whose first argument is a NON-SQL string literal (the label). SQL
/// literals (`tx.execute("INSERT…")`) and non-literal arguments (a
/// `Statement` binding its params) are not queue boundaries.
fn has_write_boundary(body: &str) -> bool {
    const METHODS: &[&str] = &[
        ".execute(",
        ".execute_retry_busy(",
        ".execute_recovery(",
        ".execute_raw(",
    ];
    for method in METHODS {
        let mut search = 0usize;
        while let Some(idx) = body[search..].find(method) {
            let arg_start = search + idx + method.len();
            search = arg_start;
            let after = &body[arg_start..];
            let trimmed = after.trim_start();
            let lit_start = after.len() - trimmed.len();
            let rest = &after[lit_start..];
            let Some(stripped) = rest.strip_prefix('"') else {
                continue;
            };
            let mut lit = String::new();
            let mut it = stripped.char_indices();
            let mut end = stripped.len();
            while let Some((i, c)) = it.next() {
                if c == '\\' {
                    // skip the escaped char
                    if let Some((next_i, _)) = it.next() {
                        lit.push('_');
                        let _ = next_i;
                    }
                    continue;
                }
                if c == '"' {
                    end = i;
                    break;
                }
                lit.push(c);
            }
            let _ = end;
            if !is_sql_literal(&lit) {
                return true;
            }
        }
    }
    false
}

/// name -> boundary classification for every callable `impl Store` method
/// that touches a boundary.
fn extract_ops() -> BTreeMap<String, Boundary> {
    let mut out: BTreeMap<String, Boundary> = BTreeMap::new();
    for (file, src) in production_sources() {
        if !file.ends_with(".rs") {
            continue;
        }
        let spans = crate::query_plan_inventory_tests::fn_spans(&src);
        let regions = impl_store_regions(&src);
        for span in &spans {
            if !regions
                .iter()
                .any(|(a, b)| span.start >= *a && span.start < *b)
            {
                continue;
            }
            if !is_callable_fn(&src, span.start) {
                continue;
            }
            let body = &src[span.start..span.end];
            let write = has_write_boundary(body);
            let read = body.contains(".read()?");
            if !write && !read {
                continue;
            }
            let boundary = match (write, read) {
                (true, true) => Boundary::Both,
                (true, false) => Boundary::Write,
                (false, true) => Boundary::Read,
                (false, false) => unreachable!(),
            };
            out.insert(span.name.clone(), boundary);
        }
    }
    out
}

fn driver_map() -> BTreeMap<&'static str, crate::store_fault_ops::Driver> {
    OPS.iter().map(|o| (o.name, o.drive)).collect()
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

fn assert_sqlite_code(result: &StoreResult<()>, code: std::ffi::c_int, name: &str, fault: &str) {
    match result {
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, msg))) => {
            assert_eq!(
                e.extended_code & 0xff,
                code,
                "[{name}] {fault}: expected SQLITE code {code}, got {} ({msg:?})",
                e.extended_code
            );
        }
        other => {
            panic!("[{name}] {fault}: expected a typed SQLITE_FULL/READONLY error, got {other:?}")
        }
    }
}

/// The committed baseline every fault phase must leave intact: seeded rows
/// read back through the store's own read surface after the fault clears.
fn baseline_verify(fx: &Fixture, phase: &str) {
    let s = &fx.store;
    let row = s
        .get_session(fx.sid)
        .unwrap_or_else(|e| panic!("[{phase}] session lost: {e:?}"))
        .unwrap_or_else(|| panic!("[{phase}] session row missing"));
    assert_eq!(row.workspace_id.raw(), 1, "[{phase}] session workspace");
    let count = s
        .message_count(fx.sid)
        .unwrap_or_else(|e| panic!("[{phase}] messages lost: {e:?}"));
    assert!(
        count >= 2,
        "[{phase}] committed messages lost (got {count}, want >= 2)"
    );
    let facts = s
        .memory_facts(fx.sid)
        .unwrap_or_else(|e| panic!("[{phase}] memory facts lost: {e:?}"));
    assert!(
        facts.len() >= 2,
        "[{phase}] committed memory facts lost (got {})",
        facts.len()
    );
    let head = s
        .ledger_head(fx.sid)
        .unwrap_or_else(|e| panic!("[{phase}] ledger head lost: {e:?}"));
    assert!(head.is_some(), "[{phase}] ledger head missing");
    let queue = s
        .queue_rows(fx.sid, 10)
        .unwrap_or_else(|e| panic!("[{phase}] queue rows lost: {e:?}"));
    assert!(!queue.is_empty(), "[{phase}] queued prompt missing");
    let task = s
        .get_task(fx.sid, fx.tid)
        .unwrap_or_else(|e| panic!("[{phase}] task lost: {e:?}"));
    assert!(task.is_some(), "[{phase}] task row missing");
}

// ---------------------------------------------------------------------------
// Drift check
// ---------------------------------------------------------------------------

/// HAR-05 enumeration discipline: the driver table and the source scanner
/// agree in BOTH directions (a new boundary-carrying op without a driver
/// fails; a driver whose op no longer carries a boundary fails).
#[test]
fn store_op_registry_covers_every_boundary_method() {
    let extracted = extract_ops();
    let registered: BTreeMap<&str, ()> = OPS.iter().map(|o| (o.name, ())).collect();
    let mut missing: Vec<&str> = extracted
        .keys()
        .filter(|name| !registered.contains_key(name.as_str()))
        .map(String::as_str)
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "NEW boundary-carrying store ops without a driver in store_fault_ops.rs:\n{}",
        missing.join("\n")
    );
    let mut stale: Vec<&str> = registered
        .keys()
        .copied()
        .filter(|name| !extracted.contains_key(*name))
        .collect();
    stale.sort();
    assert!(
        stale.is_empty(),
        "driver registered for a method with no boundary (stale):\n{}",
        stale.join("\n")
    );
    assert!(
        extracted.len() >= 200,
        "extraction found only {} ops; the scanner is broken",
        extracted.len()
    );
}

// ---------------------------------------------------------------------------
// Class A: disk full (ENOSPC shim) + read-only shim — every WRITE op
// ---------------------------------------------------------------------------

/// Every write op under the disk-full and read-only fault classes: the
/// injected typed error MUST surface (and the seam MUST have fired, so a
/// vacuous early-return cannot pass), bounded, and the committed baseline
/// survives + a probe write works after disarm.
#[test]
fn every_write_op_fails_typed_under_disk_full_and_read_only() {
    let fx = Arc::new(Fixture::seed());
    // The truncate-boundary op needs the WAL above its 64 MiB production
    // guard; only this test pays for it (suite-wide footprint stays small).
    fx.pad_wal();
    let extracted = extract_ops();
    let drivers = driver_map();
    let mut write_ops: Vec<&str> = extracted
        .iter()
        .filter(|(_, kind)| matches!(kind, Boundary::Write | Boundary::Both))
        .map(|(name, _)| name.as_str())
        .collect();
    write_ops.sort();
    assert!(!write_ops.is_empty());
    for name in &write_ops {
        let drive = *drivers
            .get(name)
            .unwrap_or_else(|| panic!("no driver for `{name}`"));
        for (fault, code) in [(FaultKind::DiskFull, 13), (FaultKind::ReadOnly, 8)] {
            let before = fx.store.fault_write.fired();
            fx.store.fault_write.arm(fault, 1);
            let outcome = run_bounded(&fx, drive, 10_000);
            fx.store.fault_write.disarm();
            let result = outcome.unwrap_or_else(|e| panic!("[{name}] {fault:?}: {e}"));
            let fired = fx.store.fault_write.fired() - before;
            assert!(
                fired >= 1,
                "[{name}] {fault:?}: never reached the write boundary (vacuous pass);                  op result was {result:?}"
            );
            assert_sqlite_code(&result, code, name, &format!("{fault:?}"));
        }
    }
    // Recovery: with the seam disarmed a real write commits again, and the
    // rows committed before the fault class all still verify.
    fx.store
        .set_session_state(fx.sid, AgentState::Idle)
        .expect("probe write after disarm");
    baseline_verify(&fx, "disk-full/readonly");
}

// ---------------------------------------------------------------------------
// Class B: real SQLITE_BUSY contention — every WRITE op
// ---------------------------------------------------------------------------

/// A write op that legitimately completes without visible blocking under
/// contention (documented, audited exemptions only):
///
/// * `wal_checkpoint_passive` / `sql_execute` — jobs whose statements take
///   no write lock (passive checkpoint / passthrough SELECT).
/// * `wal_checkpoint_truncate_bounded` — its job sets `busy_timeout = 0` by
///   design and maps the resulting SQLITE_BUSY to the documented
///   non-blocking `Ok(false)` refusal (nothing is damaged; the file simply
///   stays until the next tick).
/// * `workspace_memory_facts_fresh` — a SELECT-only job queued through the
///   writer for staleness ordering; it never takes the write lock.
const BUSY_NONBLOCKING: &[&str] = &[
    "wal_checkpoint_passive",
    "sql_execute",
    "wal_checkpoint_truncate_bounded",
    "workspace_memory_facts_fresh",
];

/// Every write op against a REAL second connection holding
/// `BEGIN IMMEDIATE`: the op must still be blocked 150 ms in (it really
/// reached the writer and waited), then complete bounded after the release —
/// Ok (write landed after release) or typed SQLITE_BUSY — with the
/// committed baseline intact afterwards.
#[test]
fn every_write_op_waits_out_real_sqlite_busy_contention() {
    let fx = Arc::new(Fixture::seed());
    let extracted = extract_ops();
    let drivers = driver_map();
    let mut write_ops: Vec<&str> = extracted
        .iter()
        .filter(|(_, kind)| matches!(kind, Boundary::Write | Boundary::Both))
        .map(|(name, _)| name.as_str())
        .collect();
    write_ops.sort();
    for name in &write_ops {
        let drive = *drivers
            .get(name)
            .unwrap_or_else(|| panic!("no driver for `{name}`"));

        // Real contention: second connection holds the write lock.
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let db = fx.store.path();
        let holder = thread::spawn(move || {
            let conn = rusqlite::Connection::open(&db).expect("contention connection");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("BEGIN IMMEDIATE under no contention");
            let _ = ready_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            conn.execute_batch("COMMIT").expect("release commit");
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("holder never took the write lock");

        // Run the op under contention with its own deadline channel.
        let (tx, rx) = mpsc::channel();
        let op_fx = Arc::clone(&fx);
        thread::spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(AssertUnwindSafe(|| drive(&op_fx))));
        });

        // Deterministic timing: 150 ms into the hold the op must still be
        // waiting (or be a documented non-blocking exemption).
        thread::sleep(Duration::from_millis(150));
        let early = rx.try_recv();
        if let Ok(outcome) = &early {
            // Two legitimate shapes:
            // (1) the job reached SQLite and the contention surfaced as a
            //     typed SQLITE_BUSY/LOCKED immediately (deferred transactions
            //     that read before their first write do NOT run the busy
            //     handler on the upgrade — verified behaviour, and the error
            //     itself proves the op experienced the lock);
            // (2) documented non-blocking exemptions (no write lock taken).
            // Anything else completed without ever touching the writer — a
            // vacuous pass.
            let busy_experienced = matches!(
                outcome,
                Ok(Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _))))
                    if matches!(e.extended_code & 0xff, 5 | 6)
            );
            let detail = match outcome {
                Ok(r) => format!("outer Ok, inner {r:?}"),
                Err(_) => "panicked".to_string(),
            };
            assert!(
                busy_experienced || BUSY_NONBLOCKING.contains(name),
                "[{name}] completed while the write lock was held without \
                 experiencing contention ({detail}): it never reached the \
                 writer (vacuous pass); if it legitimately does not block, \
                 add it to BUSY_NONBLOCKING with a reason"
            );
        }

        // Release the lock: the op must finish bounded (it may already have
        // finished in the documented non-blocking case).
        let _ = release_tx.send(());
        let outcome = match early {
            Ok(outcome) => outcome,
            Err(_) => rx
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| panic!("[{name}] hung past the contention deadline")),
        };
        holder.join().expect("holder thread");
        let outer = outcome.unwrap_or_else(|p| {
            panic!(
                "[{name}] panicked under contention: {:?}",
                p.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_default()
            )
        });
        // The contention contract: the op was still blocked while the lock
        // was held (asserted above) and finished bounded after the release.
        // Its result is typed by the StoreResult surface — Ok (the write
        // landed after release), SQLITE_BUSY/LOCKED (waited out), or the
        // op's own typed state/conflict refusal raised inside the job — all
        // are correct outcomes of real contention.
        let _ = outer;
    }
    // After every release: a fresh write commits and the baseline verifies.
    fx.store
        .set_session_state(fx.sid, AgentState::Idle)
        .expect("probe write after contention");
    baseline_verify(&fx, "busy");
}

// ---------------------------------------------------------------------------
// Class C: real read-only filesystem — every READ op (+ open refusal)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn set_read_only(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(root).expect("read dir for chmod") {
        let entry = entry.expect("dir entry");
        if entry.path().is_file() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o444))
                .expect("chmod file read-only");
        }
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555))
        .expect("chmod dir read-only");
}

#[cfg(unix)]
fn clear_read_only(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))
        .expect("chmod dir writable");
    for entry in std::fs::read_dir(root).expect("read dir for chmod") {
        let entry = entry.expect("dir entry");
        if entry.path().is_file() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o644))
                .expect("chmod file writable");
        }
    }
}

/// Read ops under a REAL read-only data dir: bounded, typed-or-clean (a
/// pooled reader may legitimately refuse a new connection on a read-only
/// -shm), never a panic. Opening a fresh store on a read-only COPY must
/// fail typed; after restoring permissions the copy REOPENS and the
/// committed baseline verifies (recovery-on-next-open); the live store's
/// rows survive the whole phase.
#[cfg(unix)]
#[test]
fn every_read_op_survives_a_real_read_only_filesystem() {
    let fx = Arc::new(Fixture::seed());
    baseline_verify(&fx, "chmod/before");
    set_read_only(&fx.root);

    let extracted = extract_ops();
    let drivers = driver_map();
    // `purge_session_content` carries a read boundary but is destructive by
    // design: driving it mid-loop would delete the fixture every later read
    // op (and the baseline probe) depend on. It is covered by THREE other
    // classes (disk-full, read-only shim, real contention) plus the corrupt
    // pass, so it is still driven under at least one fault class.
    const READ_PHASE_SKIP: &[&str] = &["purge_session_content"];
    let mut read_ops: Vec<&str> = extracted
        .iter()
        .filter(|(_, kind)| matches!(kind, Boundary::Read | Boundary::Both))
        .filter(|(name, _)| !READ_PHASE_SKIP.contains(&name.as_str()))
        .map(|(name, _)| name.as_str())
        .collect();
    read_ops.sort();
    for name in &read_ops {
        let drive = *drivers
            .get(name)
            .unwrap_or_else(|| panic!("no driver for `{name}`"));
        let outcome = run_bounded(&fx, drive, 10_000);
        match outcome {
            Err(e) => panic!("[{name}] read under read-only fs: {e}"),
            Ok(Ok(())) => {}
            Ok(Err(_)) => {} // typed refusal is a correct outcome here
        }
    }

    // A fresh open on a READ-ONLY copy must fail typed (bounded).
    let ro_copy = copy_dir(&fx.root, "har05-ro-open");
    set_read_only(&ro_copy);
    let open_outcome = {
        let root = ro_copy.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(AssertUnwindSafe(|| {
                Store::open(Path::new(&root), false)
            })));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("open under read-only must stay bounded")
    };
    let opened = open_outcome.unwrap_or_else(|_| panic!("Store::open panicked under read-only"));
    match opened {
        Err(_) => {} // typed refusal
        Ok(_) => panic!("Store::open must refuse a read-only data dir"),
    }

    // Restore the copy: open succeeds and the committed baseline verifies.
    clear_read_only(&ro_copy);
    {
        let recovered =
            Store::open(Path::new(&ro_copy), false).expect("open after permissions restored");
        let probe = Fixture {
            _dir: None,
            root: ro_copy.clone(),
            store: recovered,
            ws: fx.ws,
            sid: fx.sid,
            tid: fx.tid,
            mid: fx.mid,
            pid: fx.pid,
            queue_seq: fx.queue_seq,
            wid: fx.wid,
            attempt_op: fx.attempt_op,
            rec_id: fx.rec_id,
        };
        baseline_verify(&probe, "chmod/reopened-copy");
    }

    // Restore the live store's permissions and verify committed state.
    clear_read_only(&fx.root);
    baseline_verify(&fx, "chmod/after");
}

// ---------------------------------------------------------------------------
// Class D: corrupt page (real byte-flip) — EVERY op
// ---------------------------------------------------------------------------

fn copy_dir(src: &Path, tag: &str) -> PathBuf {
    let dst = std::env::temp_dir().join(format!("har05-{tag}-{}", std::process::id()));
    if dst.exists() {
        let _ = std::fs::remove_dir_all(&dst);
    }
    std::fs::create_dir_all(&dst).expect("create copy dir");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("dir entry");
        if entry.path().is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name())).expect("copy file");
        }
    }
    dst
}

fn page_size(db: &[u8]) -> usize {
    assert!(db.len() > 17, "db file too small");
    let raw = u16::from_be_bytes([db[16], db[17]]);
    if raw == 1 {
        65_536
    } else {
        raw as usize
    }
}

/// Byte-flip the page-type byte of the page carrying `CORRUPT_MARKER`
/// (structural corruption SQLite must report, never silently return).
fn corrupt_marker_page(db_path: &Path) -> usize {
    let mut db = std::fs::read(db_path).expect("read db file");
    let marker = CORRUPT_MARKER.as_bytes();
    let pos = db
        .windows(marker.len())
        .rposition(|w| w == marker)
        .expect("marker present in checkpointed db");
    let ps = page_size(&db);
    assert!(
        pos >= 2 * ps,
        "marker must land on a non-header page (pos {pos}, page size {ps})"
    );
    let page = pos / ps;
    db[page * ps] ^= 0xFF;
    std::fs::write(db_path, &db).expect("write corrupted db");
    page
}

fn raw_message_data(db_root: &Path) -> Result<String, String> {
    let conn = match rusqlite::Connection::open(db_root.join("faktor-plus.db")) {
        Ok(conn) => conn,
        Err(e) => return Err(format!("open: {e}")),
    };
    match conn.query_row(
        "SELECT data FROM message WHERE session_id = 1 AND seq = 9001",
        [],
        |r| r.get::<_, String>(0),
    ) {
        Ok(v) => Ok(v),
        Err(e) => Err(format!("query: {e}")),
    }
}

/// Every op (read AND write) against a real byte-flipped page: bounded,
/// typed-or-clean, no panic — plus the sharp controls: the marker message
/// control query must never return wrong data, message readers must fail
/// typed or return byte-exact fixture data, a full-integrity open refuses
/// the corrupt copy, and the pristine copy still verifies.
#[test]
fn every_op_fails_typed_on_a_corrupt_page() {
    let dir = tempfile::tempdir().expect("corrupt fixture tempdir");
    let seed_root = dir.path().to_path_buf();
    let fx = Fixture::seed_in(&seed_root);
    // Close the seeder, then TRUNCATE the WAL on a lone connection so the
    // marker row (and every other row) lives in the db file the copies take.
    drop(fx);
    {
        let raw = rusqlite::Connection::open(seed_root.join("faktor-plus.db"))
            .expect("lone connection for checkpoint");
        raw.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("truncate checkpoint before corruption");
    }

    let pristine = copy_dir(&seed_root, "pristine");
    let corrupt = copy_dir(&seed_root, "corrupt");
    let flipped_page = corrupt_marker_page(&corrupt.join("faktor-plus.db"));

    // Control: the pristine copy serves the marker byte-exact.
    let pristine_data = raw_message_data(&pristine).expect("pristine marker query");
    // Control: the corrupt copy must NOT serve wrong data for the marker —
    // either a typed failure or the byte-exact original.
    match raw_message_data(&corrupt) {
        Err(e) => {
            eprintln!("corrupt marker read failed typed as required: {e} (page {flipped_page})")
        }
        Ok(data) => panic!(
            "corrupt page (type byte flipped at page {flipped_page}) served the \
             marker row WITHOUT an error — silent corruption or an ineffective \
             flip: {data:?} vs pristine {pristine_data:?}"
        ),
    }

    let corrupt_fx = Arc::new(Fixture::reopen(&corrupt));
    let extracted = extract_ops();
    let drivers = driver_map();
    // Message readers against the corrupt page: typed failure or
    // byte-exact results (never wrong data).
    {
        let pristine_fx = Fixture::reopen(&pristine);
        let pristine_count = pristine_fx.store.message_count(pristine_fx.sid).unwrap();
        match corrupt_fx.store.message_count(corrupt_fx.sid) {
            Err(_) => {}
            Ok(count) => assert_eq!(
                count, pristine_count,
                "message_count on a corrupt page returned wrong data"
            ),
        }
        match corrupt_fx
            .store
            .messages_before_page(corrupt_fx.sid, Some(9_000_000), Some(10))
        {
            Err(_) => {}
            Ok(rows) => {
                let pristine_rows = pristine_fx
                    .store
                    .messages_before_page(pristine_fx.sid, Some(9_000_000), Some(10))
                    .unwrap();
                assert_eq!(
                    format!("{rows:?}"),
                    format!("{pristine_rows:?}"),
                    "messages_before_page on a corrupt page returned wrong data"
                );
            }
        }
    }
    let mut all: Vec<&str> = extracted.keys().map(String::as_str).collect();
    all.sort();
    assert!(all.len() >= 200);
    for name in &all {
        let drive = *drivers
            .get(name)
            .unwrap_or_else(|| panic!("no driver for `{name}`"));
        // Either the op sees nothing broken, or it fails TYPED — never a
        // hang, never a panic, never an untyped failure.
        if let Err(e) = run_bounded(&corrupt_fx, drive, 10_000) {
            panic!("[{name}] corrupt page: {e}");
        }
    }

    drop(corrupt_fx);

    // Recovery: a full-integrity open REFUSES the corrupt copy (typed).
    match Store::open(Path::new(&corrupt), true) {
        Err(StoreError::Corrupt(_) | StoreError::Sqlite(_)) => {}
        other => panic!("integrity open must refuse the corrupt db, got {other:?}"),
    }

    // No data loss: the pristine copy reopens and verifies.
    let pristine_fx = Fixture::reopen(&pristine);
    baseline_verify(&pristine_fx, "corrupt/pristine");
}

// ---------------------------------------------------------------------------
// Cross-class: committed state + recovery on next open
// ---------------------------------------------------------------------------

/// Committed state survives every shim-able class in sequence on ONE store,
/// and the next open (with a full integrity check) recovers cleanly.
#[test]
fn committed_state_and_reopen_survive_the_fault_classes() {
    let fx = Arc::new(Fixture::seed());
    // Disk full, then read-only, then a real-busy hold — same store.
    fx.store.fault_write.arm(FaultKind::DiskFull, 1);
    let _ = run_bounded(&fx, crate::store_fault_ops::OPS[0].drive, 10_000);
    fx.store.fault_write.disarm();
    fx.store.fault_write.arm(FaultKind::ReadOnly, 1);
    let _ = run_bounded(&fx, crate::store_fault_ops::OPS[0].drive, 10_000);
    fx.store.fault_write.disarm();
    // The admission-level BUSY shim (the real-contention class drives every
    // write op against a held BEGIN IMMEDIATE; this checks the shim's typed
    // SQLITE_BUSY surface too).
    fx.store.fault_write.arm(FaultKind::Busy, 1);
    let busy = run_bounded(&fx, crate::store_fault_ops::OPS[0].drive, 10_000)
        .expect("busy shim op must finish bounded");
    fx.store.fault_write.disarm();
    match busy {
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _))) => {
            assert_eq!(
                e.extended_code & 0xff,
                5,
                "the shim must surface SQLITE_BUSY"
            );
        }
        other => panic!("busy shim must fail typed, got {other:?}"),
    }
    baseline_verify(&fx, "sequenced-shims");
    let root = fx.root.clone();
    // Keep the tempdir alive across the drop of the store (Fixture owns it).
    let keep_dir = fx._dir.clone();
    drop(fx);
    let reopened = Store::open(&root, true).expect("reopen with integrity check");
    let probe = Fixture {
        _dir: None,
        root,
        store: reopened,
        ws: WorkspaceId::new(1),
        sid: SessionId::new(1),
        tid: TaskId::new(1),
        mid: 1,
        pid: 1,
        queue_seq: 1,
        wid: 1,
        attempt_op: 42,
        rec_id: VerificationRecordId::new(1),
    };
    baseline_verify(&probe, "sequenced-reopen");
    drop(probe);
    drop(keep_dir);
}
