// TEST-SEPARATION: oracle=property isolation=isolated-data-dir
//! HAR-12 panic-hook test support (ledger row: fuzzing and long runs turn
//! ANY panic into a test failure WITH the input recorded).
//!
//! Three cooperating pieces, all driven by the fuzz tests in
//! [`crate::route_fuzz_tests`]:
//!
//! 1. **The hook** ([`install`]): chains onto the previous panic hook and
//!    records payload + `Backtrace::force_capture()` + the current fuzz
//!    input (set per iteration by [`set_current_input`]) + thread name.
//!    The FIRST record wins (root cause), the previous hook still runs so
//!    the test harness keeps its own failure marking and stderr output.
//! 2. **The catcher** ([`catch`]): runs each iteration inside
//!    `catch_unwind`, so a panicking decision function never aborts the
//!    test binary — it becomes an `Err` carrying payload, backtrace and
//!    the recorded input.
//! 3. **The failure converter** ([`fail_with_minimized_input`]): minimizes
//!    the input with [`minimize`], prints the MINIMIZED input + backtrace,
//!    saves it under `regressions/<name>-<digest>.json` for replay, and
//!    panics with the regression path — the panic converts into an ordinary
//!    TEST FAILURE, never a silent pass.
//!
//! The mutex poisoning hazard is handled explicitly: every lock site uses
//! `unwrap_or_else(PoisonError::into_inner)`, because the hook itself can
//! fire while the state lock is conceptually "hot" from another thread.

use std::any::Any;
use std::backtrace::Backtrace;
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

/// Payload of a caught panic as printable text (the harness contract: the
/// payload is always surfaced, string or not).
pub(crate) fn payload_string(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// One panic, fully attributed: what panicked, where, and under WHICH
/// input the fuzz driver was running when it happened.
#[derive(Debug, Clone)]
pub(crate) struct PanicRecord {
    pub payload: String,
    pub backtrace: String,
    pub input: String,
    pub thread: String,
}

#[derive(Default)]
struct ThreadSlots {
    current_input: String,
    record: Option<PanicRecord>,
}

thread_local! {
    static SLOTS: std::cell::RefCell<ThreadSlots> = std::cell::RefCell::new(ThreadSlots::default());
}

/// Fallback input for panics on threads that never set one (foreign
/// threads inherit the most recently set input — best-effort attribution;
/// the record is tagged with the thread name so a mis-attribution is
/// visible in the failure text).
fn global_input() -> std::sync::MutexGuard<'static, String> {
    static GLOBAL: OnceLock<Mutex<String>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Panics on threads that have NO input slot (tokio workers, sibling test
/// threads): kept in a process-global list so a long run can still see
/// them (belt and braces for "any panic becomes a failure").
fn foreign_records() -> std::sync::MutexGuard<'static, Vec<PanicRecord>> {
    static FOREIGN: OnceLock<Mutex<Vec<PanicRecord>>> = OnceLock::new();
    FOREIGN
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The foreign-panic list length NOW (soaks snapshot it before they start).
pub(crate) fn foreign_record_count() -> usize {
    foreign_records().len()
}

/// Take the foreign records appended since `since` (bounded copy).
pub(crate) fn foreign_records_since(since: usize) -> Vec<PanicRecord> {
    let guard = foreign_records();
    guard.iter().skip(since).cloned().collect()
}

/// Install (once per process) the capturing panic hook.
pub(crate) fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
            let own_input = SLOTS
                .try_with(|slots| slots.borrow().current_input.clone())
                .unwrap_or_default();
            let (input, attributed) = if !own_input.is_empty() {
                (own_input, true)
            } else {
                (global_input().clone(), false)
            };
            let record = PanicRecord {
                payload: payload_string(info.payload()),
                backtrace: Backtrace::force_capture().to_string(),
                input,
                thread: thread_name,
            };
            if attributed {
                SLOTS.with(|slots| {
                    let mut slots = slots.borrow_mut();
                    if slots.record.is_none() {
                        slots.record = Some(record);
                    }
                });
            } else {
                foreign_records().push(record);
            }
            previous(info);
        }));
    });
}

/// Attribute every panic recorded from now on to `input`. Also clears the
/// previous iteration's record so stale panics can never be attributed to
/// a later input. Thread-scoped: parallel tests in one process never
/// overwrite each other's slots.
pub(crate) fn set_current_input(input: &str) {
    install();
    *global_input() = input.to_string();
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        slots.current_input = input.to_string();
        slots.record = None;
    });
}

/// Take the recorded panic (if any) on THIS thread and reset the slot.
pub(crate) fn take_record() -> Option<PanicRecord> {
    SLOTS.with(|slots| slots.borrow_mut().record.take())
}

/// A panic that the catcher converted into data (never an abort).
#[derive(Debug, Clone)]
pub(crate) struct CaughtPanic {
    pub payload: String,
    pub backtrace: String,
    pub input: String,
    pub thread: String,
}

/// Run `f` under the hook and catch ANY panic it produces — including a
/// panic raised on a foreign thread during the iteration (foreign records
/// surface through [`foreign_records_since`] for the soak; the catcher
/// carries the main-thread payload, backtrace and input). Callers MUST set
/// the input first via [`set_current_input`]; each catch opens a fresh
/// attribution window on THIS thread. Ok(()) means no panic was recorded.
pub(crate) fn catch<F: FnOnce()>(f: F) -> Result<(), CaughtPanic> {
    install();
    SLOTS.with(|slots| slots.borrow_mut().record = None);
    let outcome = panic::catch_unwind(panic::AssertUnwindSafe(f));
    let record = take_record();
    match outcome {
        Ok(()) => {
            if let Some(record) = record {
                Err(CaughtPanic {
                    payload: record.payload,
                    backtrace: record.backtrace,
                    input: record.input,
                    thread: record.thread,
                })
            } else {
                Ok(())
            }
        }
        Err(err) => Err(CaughtPanic {
            payload: payload_string(err.as_ref()),
            backtrace: record
                .as_ref()
                .map(|r| r.backtrace.clone())
                .unwrap_or_else(|| Backtrace::force_capture().to_string()),
            input: record
                .as_ref()
                .map(|r| r.input.clone())
                .unwrap_or_else(|| "(input slot empty at panic time)".to_string()),
            thread: record
                .as_ref()
                .map(|r| r.thread.clone())
                .unwrap_or_else(|| {
                    std::thread::current()
                        .name()
                        .unwrap_or("<unnamed>")
                        .to_string()
                }),
        }),
    }
}

/// Greedy delta-minimization: repeatedly replace the input with the first
/// `shrink` candidate that still panics, until nothing smaller panics.
/// `budget` bounds the total number of still-failing probes so a hostile
/// shrink space cannot turn minimization into the long test.
pub(crate) fn minimize<I: Clone>(
    seed: &I,
    shrink: impl Fn(&I) -> Vec<I>,
    still_fails: impl Fn(&I) -> bool,
    budget: usize,
) -> I {
    let mut current = seed.clone();
    let mut probes = 0usize;
    'shrink: loop {
        for candidate in shrink(&current) {
            if probes >= budget {
                break 'shrink;
            }
            probes += 1;
            if still_fails(&candidate) {
                current = candidate;
                continue 'shrink;
            }
        }
        break;
    }
    current
}

fn digest_of(json: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(json).unwrap_or_default();
    let hash = blake3::hash(&bytes);
    hash.to_hex().to_string()[..12].to_string()
}

/// Persist a minimized input as a replayable regression case. Returns the
/// path. Existing files with the same content are left untouched (stable
/// across runs: the digest is content-addressed).
pub(crate) fn save_regression(
    dir: &Path,
    case_name: &str,
    input: &serde_json::Value,
    panic_payload: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let file_name = format!("{case_name}-{}.json", digest_of(input));
    let path = dir.join(&file_name);
    let document = serde_json::json!({
        "schema": "faktor-route-fuzz-regression/v1",
        "case": case_name,
        "panic_payload": panic_payload,
        "input": input,
    });
    if !path.exists() {
        let body = serde_json::to_string_pretty(&document)
            .map_err(|e| format!("serialize regression: {e}"))?;
        std::fs::write(&path, format!("{body}\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// Load every regression case file in `dir` (name order, deterministic).
pub(crate) fn load_regressions(dir: &Path) -> Vec<(PathBuf, serde_json::Value)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
            out.push((path, value));
        }
    }
    out
}

/// THE conversion: a caught panic becomes a test failure that prints the
/// MINIMIZED input, saves it for replay, and panics with the regression
/// path so the harness reports a failure (never a silent pass).
pub(crate) fn fail_with_minimized_input(
    dir: &Path,
    case_name: &str,
    caught: &CaughtPanic,
    minimized: &serde_json::Value,
) -> ! {
    let saved = save_regression(dir, case_name, minimized, &caught.payload)
        .unwrap_or_else(|e| panic!("regression save failed: {e}"));
    let minimized_pretty = serde_json::to_string_pretty(minimized).unwrap_or_default();
    panic!(
        "PANIC CONVERTED TO TEST FAILURE [{case_name}]\n\
         minimized input: {minimized_pretty}\n\
         regression saved: {}\n\
         payload: {}\n\
         input at panic: {}\n\
         thread: {}\n\
         backtrace:\n{}",
        saved.display(),
        caught.payload,
        caught.input,
        caught.thread,
        caught.backtrace
    );
}
