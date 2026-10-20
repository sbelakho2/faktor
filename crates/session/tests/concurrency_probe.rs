//! New-angle concurrency probes: session resolution tail latency and the
//! journal-wake fan-out. These are deliberately NOT soak tests: one bounded
//! burst, measured percentiles, no background load.
//!
//! The deterministic fan-out test is part of the standing suite (it asserts
//! a cross-session append never wakes another session's live readers).
//! `[perf] session_resolution_p95_under_64_way_concurrency` is an ignored
//! measured probe with generous ceilings; run it manually:
//!
//! `cargo test -p faktor-session --release --test concurrency_probe -- --ignored --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_core::event::EventKind;
use faktor_core::state::AgentState;
use faktor_session::{SessionHandle, SessionManager};

fn open(root: &std::path::Path) -> Arc<SessionManager> {
    SessionManager::open_quick(root.join("store"), root.join("cas")).unwrap()
}

fn session(manager: &Arc<SessionManager>, title: &str) -> SessionHandle {
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, title, "fake", "m").unwrap();
    handle
        .force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
        .unwrap();
    handle
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// A cross-session append must wake only that session's live readers. The
/// historical single global watch woke EVERY stream (32 SSE streams = 32
/// empty journal reads per delta in any session), i.e. O(streams x appends)
/// fan-out under realistic concurrency.
#[test]
fn journal_wake_is_scoped_to_the_appended_session() {
    let dir = tempfile::tempdir().unwrap();
    let manager = open(dir.path());
    let a = session(&manager, "a");
    let b = session(&manager, "b");

    let mut watchers: Vec<_> = (0..32).map(|_| a.subscribe_journal()).collect();

    // Append TEN deltas to the OTHER session: zero watcher on `a` may wake.
    for _ in 0..10 {
        b.force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
            .unwrap();
    }
    let woken: usize = watchers
        .iter_mut()
        .filter(|rx| rx.has_changed().unwrap_or(false))
        .count();
    assert_eq!(
        woken, 0,
        "a cross-session append woke {woken}/32 watchers (global-watch fan-out)"
    );

    // A same-session append wakes every watcher exactly once.
    a.force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
        .unwrap();
    let woken: usize = watchers
        .iter_mut()
        .filter(|rx| rx.has_changed().unwrap_or(false))
        .count();
    assert_eq!(woken, 32, "same-session append must wake every live reader");
}

/// Open verification is paid ONCE per session per typed-write generation:
/// repeated resolutions between typed appends are O(1). A direct-SQL
/// corruption (the test/forensic hatch) advances the store epoch and the
/// next open re-verifies and fails closed, exactly like the uncached path.
#[test]
fn open_verification_is_cached_and_direct_sql_invalidates_it() {
    let dir = tempfile::tempdir().unwrap();
    let manager = open(dir.path());
    let handle = session(&manager, "cached");
    let id = handle.id();

    assert_eq!(manager.open_verification_count(), 0);
    manager.get_session(id).unwrap().unwrap();
    assert_eq!(manager.open_verification_count(), 1);
    for _ in 0..50 {
        manager.get_session(id).unwrap().unwrap();
    }
    assert_eq!(
        manager.open_verification_count(),
        1,
        "steady-state resolution must be a cache hit"
    );
    // A typed append leaves the cache intact.
    handle
        .force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
        .unwrap();
    manager.get_session(id).unwrap().unwrap();
    assert_eq!(manager.open_verification_count(), 1);

    // Direct SQL invalidates: torn creation is refused typed, and the
    // re-verification is counted.
    manager
        .store()
        .sql_execute(&format!(
            "DELETE FROM event WHERE session_id = {}",
            id.raw()
        ))
        .unwrap();
    let err = manager.get_session(id).unwrap_err();
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Malformed,
        "{err:?}"
    );
    assert!(err.message.contains("no events"), "{err:?}");
    assert_eq!(
        manager.open_verification_count(),
        2,
        "direct SQL must force a real re-verification"
    );
}

/// Measured probe: 64 sessions resolved concurrently by 64 threads, with an
/// appended journal tail (the realistic mid-flight state). The ceiling is
/// generous (debug builds on shared CI can be slow) but real: a regression
/// back to per-request journal/ledger re-verification shows up immediately
/// at 64-way concurrency.
#[test]
#[ignore = "[perf] measured probe; run manually with --ignored --nocapture"]
fn session_resolution_p95_under_64_way_concurrency() {
    const SESSIONS: usize = 64;
    const ROUNDS: usize = 200;
    const CEILING_P95: Duration = Duration::from_millis(5);

    let dir = tempfile::tempdir().unwrap();
    let manager = open(dir.path());
    let mut handles = Vec::with_capacity(SESSIONS);
    for i in 0..SESSIONS {
        let handle = session(&manager, &format!("s{i}"));
        for _ in 0..8 {
            handle
                .force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
                .unwrap();
        }
        handles.push(handle.id());
    }
    // Warm the resolver once (first-open verification is intentionally paid
    // once per session per process).
    for id in &handles {
        manager.get_session(*id).unwrap().unwrap();
    }

    let start = Instant::now();
    let mut samples: Vec<Duration> = std::thread::scope(|scope| {
        let mut threads = Vec::with_capacity(SESSIONS);
        for id in handles.iter().copied() {
            let manager = Arc::clone(&manager);
            threads.push(scope.spawn(move || {
                let mut local = Vec::with_capacity(ROUNDS);
                for _ in 0..ROUNDS {
                    let t = Instant::now();
                    let handle = manager.get_session(id).unwrap().expect("session exists");
                    debug_assert_eq!(handle.id(), id);
                    local.push(t.elapsed());
                }
                local
            }));
        }
        let mut all = Vec::with_capacity(SESSIONS * ROUNDS);
        for t in threads {
            all.extend(t.join().unwrap());
        }
        all
    });
    let elapsed = start.elapsed();
    samples.sort_unstable();
    let p50 = percentile(&samples, 0.50);
    let p95 = percentile(&samples, 0.95);
    let p99 = percentile(&samples, 0.99);
    let max = *samples.last().unwrap();
    println!(
        "[perf] get_session x{} over {} sessions: p50={:?} p95={:?} p99={:?} max={:?} wall={:?}",
        samples.len(),
        SESSIONS,
        p50,
        p95,
        p99,
        max,
        elapsed
    );
    assert!(
        p95 <= CEILING_P95,
        "get_session p95 {p95:?} exceeds the measured ceiling {CEILING_P95:?} \
         (p50={p50:?} p99={p99:?} max={max:?})"
    );
}

/// Bounded probe (no soak): journal append + tail catch-up at 1k / 10k /
/// 100k events in ONE session. Fails a quadratic append (e.g. a per-append
/// full journal scan) or a tail read that degrades with history: the
/// per-event cost at 100k must stay within 4x of 1k, and the catch-up read
/// over the last 10 events must stay bounded.
#[test]
#[ignore = "[perf] journal append/catch-up at 1k/10k/100k events"]
fn journal_append_and_tail_catch_up_are_linear_at_100k() {
    fn pct(samples: &[Duration], p: f64) -> Duration {
        if samples.is_empty() {
            return Duration::ZERO;
        }
        let index = ((samples.len() as f64 - 1.0) * p).round() as usize;
        samples[index.min(samples.len() - 1)]
    }

    fn measure(events: usize) -> (Duration, Vec<Duration>) {
        let dir = tempfile::tempdir().unwrap();
        let manager = open(dir.path());
        let handle = session(&manager, "scale");
        let started = Instant::now();
        for _ in 0..events {
            handle
                .force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
                .unwrap();
        }
        let append = started.elapsed() / events as u32;
        let last = handle.last_event_seq().unwrap().expect("events").raw();
        let mut catch_ups = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            let tail = handle
                .events_range(last.saturating_sub(9), Some(10))
                .unwrap();
            catch_ups.push(started.elapsed());
            assert_eq!(tail.len(), 10, "bounded tail page");
            assert_eq!(tail.last().unwrap().seq.raw(), last);
        }
        (append, catch_ups)
    }
    let mut results = Vec::new();
    for events in [1_000usize, 10_000, 100_000] {
        let (append, mut catch_ups) = measure(events);
        catch_ups.sort_unstable();
        println!(
            "[perf] events={events}: append/event={append:?} tail10 p50={:?} p95={:?} p99={:?}",
            pct(&catch_ups, 0.50),
            pct(&catch_ups, 0.95),
            pct(&catch_ups, 0.99)
        );
        results.push((append, catch_ups));
    }
    let (one_k, _) = results[0];
    let (hundred_k, catch_ups) = &results[2];
    let catch_up = pct(catch_ups, 0.95);
    assert!(
        *hundred_k <= one_k.saturating_mul(4) + Duration::from_micros(100),
        "per-event append cost grew from {one_k:?} (1k) to {hundred_k:?} (100k): quadratic shape"
    );
    assert!(
        catch_up < Duration::from_millis(50),
        "100k-history tail catch-up {catch_up:?} exceeds 50ms"
    );
}
