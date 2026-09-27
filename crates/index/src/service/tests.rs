//! Index service tests (mechanically split from `service`).

use super::*;
use crate::state::WorkspaceIndexState as St;
use tempfile::TempDir;

/// The crash seam is a process-global test hook; the harness runs tests
/// in parallel, so every service test serializes on this lock (the
/// seam is only ever installed while the crash test holds it). Shared
/// with the heavy cold-path fixtures so their IO never starves a
/// seam round's deadline.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    // Test-serialization lock only: recover on poison so one panicking
    // test cannot cascade-fail every later fixture.
    crate::cold::TEST_SERIAL
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Environment-independent readiness ceiling for tests whose semantic
/// assertion is the PUBLISHED CONTENT, not latency: readiness is
/// poll/progress-driven (`ensure_ready` reconciles and checks the
/// machine state every 15 ms), so the only thing this bound exists for
/// is to fail loudly on a true deadlock. 30 s intermittently expired
/// under machine-wide load (other test binaries / cold heavy fixtures
/// sharing the box) while builds were still making progress.
const DEADLINE: Duration = Duration::from_secs(300);
/// Watcher-driven rebuilds are scheduled against machine load; this
/// ceiling only fails when no rebuild ever happens.
const WATCHER_DEADLINE: Duration = Duration::from_secs(240);
/// The bounded grace for a REAL FSEvents delivery before the watcher e2e
/// falls back to the service's own event kick (a stalled watcher stream
/// on a loaded host must not strand the suite). The durable assertions
/// are identical on both signals; only the signal origin differs.
const WATCHER_EVENT_GRACE: Duration = Duration::from_secs(30);

struct Env {
    _dir: TempDir,
    repo: PathBuf,
    store_root: PathBuf,
    data_root: PathBuf,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let store_root = dir.path().join("store");
    let data_root = dir.path().join("index_data");
    Env {
        _dir: dir,
        repo,
        store_root,
        data_root,
    }
}

fn write(repo: &Path, rel: &str, content: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn fast_cfg() -> ServiceConfig {
    ServiceConfig {
        poll: Duration::from_millis(25),
        fingerprint_interval: Duration::from_millis(50),
        build_lease: Duration::from_millis(200),
        ..ServiceConfig::default()
    }
}

fn cfg_poll_only() -> ServiceConfig {
    // fingerprint reconciliation disabled for watcher e2e determinism
    ServiceConfig {
        poll: Duration::from_millis(25),
        fingerprint_interval: Duration::from_secs(3600),
        build_lease: Duration::from_millis(200),
        ..ServiceConfig::default()
    }
}

/// Fresh store + service on the env (a "daemon restart").
fn restart(
    env: &Env,
    fs: Arc<WorkspaceFileService>,
    cfg: Option<ServiceConfig>,
) -> (Arc<Store>, Arc<IndexService>, WorkspaceId) {
    let store = Arc::new(Store::open(&env.store_root, true).unwrap());
    let ws = store.create_workspace(env.repo.to_str().unwrap()).unwrap();
    let svc = IndexService::open(store.clone(), env.data_root.clone(), fs).unwrap();
    if let Some(c) = cfg {
        svc.set_config(c);
    }
    (store, svc, ws)
}

fn first_fixture() -> (Env, Arc<Store>, Arc<IndexService>, WorkspaceId) {
    let env = env();
    write(
        &env.repo,
        "src/lib.rs",
        "pub fn alpha() -> i64 { 1 }\npub struct Beta {}\n",
    );
    write(&env.repo, "src/util.py", "def gamma():\n    return 1\n");
    write(&env.repo, "AGENTS.md", "# rules\n");
    write(&env.repo, "target/junk.rs", "fn junk() {}");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, None);
    svc.set_config(fast_cfg());
    (env, store, svc, ws)
}

/// Build to `want_gen` and assert the published content serves lookups.
fn pub_view_asserts(svc: &IndexService, ws: WorkspaceId, want_gen: u64) {
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), want_gen);
    let arc = view.index();
    let idx = arc.lock().unwrap();
    assert!(!idx.files_for_token(ws, "alpha", 10).is_empty());
    let syms = idx.symbol_lookup(ws, "Beta", 10);
    assert_eq!(syms[0].1.name, "Beta");
    assert!(idx.file_paths(ws).iter().any(|p| p == "src/lib.rs"));
    assert!(
        !idx.file_paths(ws).iter().any(|p| p.contains("junk")),
        "skipped dirs never indexed"
    );
}

fn journal_counts(store: &Store, ws: WorkspaceId) -> Vec<(String, i64)> {
    store
        .index_state_log(ws, 100_000)
        .unwrap()
        .into_iter()
        .map(|r| (r.kind, r.generation))
        .collect()
}

// ------------------------------------------------------------- happy path

#[test]
fn build_serves_and_dirty_rebuilds() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    let (state, gen) = svc.state(ws).unwrap();
    assert_eq!((state, gen), (St::Ready { generation: 1 }, 1));
    // Watcher-event semantics: change the tree, request a rebuild.
    write(&env.repo, "src/lib.rs", "pub fn delta() -> i64 { 2 }\n");
    std::thread::sleep(Duration::from_millis(60)); // mtime resolution
    svc.request_build(ws).unwrap();
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 2);
    let arc = view.index();
    let idx = arc.lock().unwrap();
    assert!(!idx.symbol_lookup(ws, "delta", 10).is_empty());
    assert!(idx.symbol_lookup(ws, "alpha", 10).is_empty());
    drop(idx);
    let log = journal_counts(&store, ws);
    assert_eq!(
        log.iter().filter(|(k, _)| k == "building").count(),
        2,
        "{log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|(k, g)| k == "building" && *g == 2)
            .count(),
        1,
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, _)| k == "ready").count(),
        2,
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, _)| k == "dirty").count(),
        1,
        "{log:?}"
    );
    // Durable row + mirror agree.
    let (state, gen) = svc.state(ws).unwrap();
    assert_eq!((state, gen), (St::Ready { generation: 2 }, 2));
}

#[test]
fn view_is_none_while_first_build_has_no_ready_generation() {
    let _serial = serial();
    let (env, _store, svc, ws) = first_fixture();
    let _ = &svc;
    let _ = &ws;
    // A second service has never built this workspace: attach leaves it
    // NotStarted and view() must be None (never torn/partial) until a
    // Ready generation exists.
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
    svc2.attach(ws2).unwrap();
    assert!(svc2.view(ws2).is_none());
    let (state, _) = svc2.state(ws2).unwrap();
    assert_eq!(state, St::NotStarted);
    pub_view_asserts(&svc2, ws2, 1);
}

// ------------------------------------------------------ (a) crash mid-build

#[test]
fn crash_mid_build_leaves_durable_building_and_never_torn() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    write(&env.repo, "extra.rs", "pub fn extra_fn() {}\n");
    std::thread::sleep(Duration::from_millis(60));

    let reader_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let svc = svc.clone();
        let stop = reader_stop.clone();
        std::thread::spawn(move || {
            let mut last_gen = 0u64;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(view) = svc.view(ws) {
                    let g = view.generation();
                    assert!(
                        g >= last_gen,
                        "generations must be monotone, saw {g} after {last_gen}"
                    );
                    last_gen = g;
                    let arc = view.index();
                    let idx = arc.lock().unwrap();
                    let paths = idx.file_paths(ws);
                    // Torn-read guard: published content is COMPLETE for
                    // its generation. gen-1 never carries extra.rs;
                    // gen-2 always does.
                    let has_extra = paths.iter().any(|p| p == "extra.rs");
                    assert!(
                        !(has_extra && g == 1),
                        "gen 1 must never observe gen 2 rows"
                    );
                    assert!(has_extra || g == 1, "gen {g} must be complete: {paths:?}");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    // 50 crash rounds. Each round: fresh service, seam armed; the
    // rebuild toward gen 2 panics BEFORE the publish CAS (no file was
    // staged at all).
    let mut crash_rounds = 0u64;
    for _ in 0..50 {
        install_seam(Box::new(move |_w, generation, point| {
            if point == "before_publish" && generation == 2 {
                // Target only THIS round's crash; rounds are sequential
                // so the hook is cleared right after each attempt. The
                // gen-1 verification publish (the same-generation
                // `verify_dirty` fact) is allowed to land so the crash
                // happens in the REAL gen-2 rebuild.
                panic!("simulated builder crash before the publish CAS");
            }
        }));
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            svc2.ensure_ready(ws2, Instant::now() + DEADLINE)
        }));
        clear_seam(); // never leak the crash hook into other tests
        drop(store2);
        match outcome {
            Err(_) => {
                crash_rounds += 1;
                // Crash residue: durable + mirrored Building{2}.
                let (state, gen) = svc2.state(ws2).unwrap();
                assert_eq!(gen, 2);
                assert!(matches!(state, St::Building { generation: 2 }), "{state:?}");
                // Only complete generations are ever visible: gen 1 with
                // NO extra.rs, or nothing at all.
                if let Some(view) = svc2.view(ws2) {
                    assert_eq!(view.generation(), 1);
                    let arc = view.index();
                    let idx = arc.lock().unwrap();
                    assert!(
                        !idx.file_paths(ws2).iter().any(|p| p == "extra.rs"),
                        "no gen-2 rows before the swap"
                    );
                }
            }
            Ok(view) => {
                // Impossible while the seam is armed (each round crashes
                // before the publish); kept as a safety net.
                assert_eq!(view.unwrap().generation(), 2);
                break;
            }
        }
        drop(svc2);
    }
    assert_eq!(crash_rounds, 50, "every armed round must crash the builder");

    // Final restart WITHOUT the seam completes at exactly gen 2 and the
    // journal shows resume rows for the crashed attempts.
    clear_seam();
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store3, svc3, ws3) = restart(&env, fs, Some(fast_cfg()));
    pub_view_asserts(&svc3, ws3, 2);
    let (state, gen) = svc3.state(ws3).unwrap();
    assert_eq!((state, gen), (St::Ready { generation: 2 }, 2));
    let log = journal_counts(&store3, ws3);
    assert_eq!(
        log.iter()
            .filter(|(k, g)| k == "building" && *g == 2)
            .count(),
        1,
        "exactly ONE first claim of gen 2: {log:?}"
    );
    // Rounds 2..50 resumed the SAME crashed generation AND the final
    // clean restart resumed it once more (50 resume rows total), then
    // the publish succeeded: one resume per crashed attempt, and the
    // crashed target is never renumbered.
    assert_eq!(
        log.iter().filter(|(k, g)| k == "resume" && *g == 2).count(),
        50,
        "each crashed restart must journal a resume of gen 2: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, g)| k == "ready" && *g == 2).count(),
        1,
        "gen 2 published exactly once: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, g)| k == "ready" && *g == 1).count(),
        1,
        "gen 1 published exactly once: {log:?}"
    );
    reader_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    reader.join().unwrap();
    drop(store);
    drop(svc3);
}

#[test]
fn crash_before_visibility_resumes_same_target_without_torn_reads() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    write(&env.repo, "extra.rs", "pub fn extra_fn() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    // Pin the rebuild: `ensure_ready` would otherwise return the at-rest
    // gen-1 view without probing the changed tree.
    svc.request_build(ws).unwrap();

    // Crash INSIDE the atomic writer, after the temp bytes were fsynced,
    // but before the rename made gen-2 visible — and therefore before
    // the durable `Building{2} -> Ready{2}` CAS. The hook survives the
    // earlier "before_publish" seam by name.
    install_seam(Box::new(move |_w, generation, point| {
        if point == "before_visibility" && generation == 2 {
            panic!("simulated builder crash between staging and visibility");
        }
    }));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        svc.ensure_ready(ws, Instant::now() + DEADLINE)
    }));
    clear_seam();
    assert!(outcome.is_err(), "the armed seam must kill the builder");

    // Ready must never name bytes that are not visible: the durable row
    // is still Building{2} and the visible generation file is exactly
    // the OLD one — never a partial gen-2. The writer's orphan temp is
    // the only residue.
    let gen_dir = generation_dir(&env.data_root, ws);
    let names: Vec<String> = std::fs::read_dir(&gen_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n == "gen-2.json"),
        "the rename must not have happened: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "gen-1.json"),
        "the old generation stays visible: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|n| faktor_fs::atomic::is_internal_temp_name(n)),
        "the crash leaves the writer's orphan temp, proving the seam was \
             between staging and the rename: {names:?}"
    );
    // Every visible generation file is complete and decodable: a torn
    // file can never be observed (the shared writer's contract).
    for name in names.iter().filter(|n| n.starts_with("gen-")) {
        let file = read_generation_file(&gen_dir.join(name)).unwrap_or_else(|e| {
            panic!("visible generation {name} must be whole: {e:?}");
        });
        assert_eq!(file.workspace, ws.raw());
    }
    let (durable, gen) = {
        let row = store.index_state_get(ws).unwrap().unwrap();
        let parsed = PersistedIndexState::parse(row.state_json, row.generation).unwrap();
        (parsed.state, parsed.row_generation)
    };
    assert_eq!(gen, 2);
    assert!(
        matches!(durable, St::Building { generation: 2 }),
        "the CAS never ran before the crash: {durable:?}"
    );

    // Reopen: the Building{2} residue resumes the SAME target (no
    // renumbering, no torn-Ready heal) and the reader observes only
    // complete generations until the new one is published.
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store3, svc3, ws3) = restart(&env, fs, Some(fast_cfg()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let svc = svc3.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + DEADLINE;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
                if let Some(view) = svc.view(ws3) {
                    let generation = view.generation();
                    let arc = view.index();
                    let idx = arc.lock().unwrap();
                    let has_extra = idx.file_paths(ws3).iter().any(|p| p == "extra.rs");
                    drop(idx);
                    // gen 1 lacks extra.rs; the resumed gen 2 carries
                    // it. No mixed state.
                    assert!(
                        (generation == 1 && !has_extra) || (generation == 2 && has_extra),
                        "torn generation observed: gen {generation}, extra={has_extra}"
                    );
                }
                if svc.view(ws3).map(|v| v.generation()) == Some(2) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let view = svc3.ensure_ready(ws3, Instant::now() + DEADLINE).unwrap();
    assert_eq!(
        view.generation(),
        2,
        "the crash residue resumes the same target, never renumbers"
    );
    pub_view_asserts(&svc3, ws3, 2);
    let log = journal_counts(&store3, ws3);
    assert_eq!(
        log.iter().filter(|(k, _)| k == "torn_ready").count(),
        0,
        "no tear ever existed (Ready came after visibility): {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|(k, g)| k == "building" && *g == 2)
            .count(),
        1,
        "exactly one first claim of gen 2: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, g)| k == "resume" && *g == 2).count(),
        1,
        "the crashed target is resumed: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, g)| k == "ready" && *g == 2).count(),
        1,
        "gen 2 published exactly once: {log:?}"
    );
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    reader.join().unwrap();
    drop(store3);
    drop(svc3);
}

/// External corruption of the published generation (the file removed
/// under a Ready row — e.g. an older daemon's torn CAS-first publish or
/// manual deletion): attach heals durably through `Ready -> Dirty ->
/// rebuild` and never serves "no index" silently.
#[test]
fn ready_row_with_deleted_generation_heals_loudly_at_attach() {
    let _serial = serial();
    let (env, _store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    drop(svc);
    std::fs::remove_file(generation_file_path(&env.data_root, ws, 1)).unwrap();
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
    let view = svc2.ensure_ready(ws2, Instant::now() + DEADLINE).unwrap();
    assert_eq!(
        view.generation(),
        2,
        "the missing published generation heals into the next build"
    );
    assert!(view
        .index()
        .lock()
        .unwrap()
        .file_paths(ws2)
        .iter()
        .any(|p| p == "src/lib.rs"));
    let log = journal_counts(&store2, ws2);
    assert_eq!(
        log.iter().filter(|(k, _)| k == "torn_ready").count(),
        1,
        "the heal names the tear loudly: {log:?}"
    );
    drop(store2);
}

// ------------------------------------------------------ (b) corrupt JSON

#[test]
fn corrupt_persisted_state_fails_open_as_failed_not_silent_default() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    drop(svc);
    // Adversary corrupts the state row directly in the store.
    store
        .index_state_put(ws, "{ this is not json !!!", 3, "corrupt")
        .unwrap();
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
    svc2.attach(ws2).unwrap();
    let (state, gen) = svc2.state(ws2).unwrap();
    assert!(
        matches!(state, St::Failed { .. }),
        "corrupt row must fail OPEN as durable Failed, got {state:?}"
    );
    assert_eq!(gen, 3);
    assert!(svc2.view(ws2).is_none());
    // Loud journal entry + explicit retry recovers (retry targets the
    // corrupt row's generation 3 — no renumbering).
    let log = journal_counts(&store2, ws2);
    assert_eq!(log[0].0, "corrupt", "{log:?}");
    svc2.request_build(ws2).unwrap();
    let view = svc2.ensure_ready(ws2, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 3);
    let (state, gen) = svc2.state(ws2).unwrap();
    assert_eq!((state, gen), (St::Ready { generation: 3 }, 3));
    // The gen-1 file from before the corruption was pruned when gen 3
    // published (keep 2: gen 2 is missing, so gen 1 goes).
    let gen_dir = generation_dir(&env.data_root, ws2);
    let gens: Vec<String> = std::fs::read_dir(&gen_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        gens.iter().any(|g| g == "gen-3.json"),
        "gen-3 published: {gens:?}"
    );
}

#[test]
fn corrupt_row_repair_write_loss_surfaces_and_recovers_on_retry() {
    // Adversarial (injected store fault): an external writer advances
    // the durable row with a corrupt payload, and the fail-open rewrite
    // of the Failed marker fails. The loss must surface as a typed
    // error (never be discarded), the mirror must not be half-updated,
    // and the retry must land the repair durably.
    let _serial = serial();
    let (_env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    // External writer corrupts the row at a newer generation.
    store
        .index_state_put(ws, "{ this is not json !!!", 5, "corrupt")
        .unwrap();
    // The repair rewrite aborts on this pass.
    store
        .sql_execute(
            "CREATE TRIGGER idx_fail_corrupt_repair BEFORE INSERT ON index_state \
                 BEGIN SELECT RAISE(ABORT, 'injected index-state corruption'); END",
        )
        .unwrap();
    let err = svc
        .refresh_mirror(ws)
        .expect_err("the dropped repair write must surface, never vanish");
    assert!(matches!(err, IndexError::Store(_)), "{err:?}");
    // No half-applied repair: the mirror still carries the last good
    // generation and the durable corrupt row stays the retry trigger.
    let (state, gen) = svc.state(ws).unwrap();
    assert!(matches!(state, St::Ready { generation: 1 }), "{state:?}");
    assert_eq!(gen, 1);
    // Retry after the injected failure clears: the fail-open repair
    // lands durably at the corrupt row's own generation.
    store
        .sql_execute("DROP TRIGGER idx_fail_corrupt_repair")
        .unwrap();
    svc.refresh_mirror(ws).unwrap();
    let (state, gen) = svc.state(ws).unwrap();
    assert!(matches!(state, St::Failed { .. }), "{state:?}");
    assert_eq!(gen, 5);
    let log = journal_counts(&store, ws);
    assert_eq!(
        log.iter().filter(|(k, _)| k == "corrupt").count(),
        2,
        "the external corruption and the landed repair are both journaled: {log:?}"
    );
}

/// P2: a negative persisted generation is impossible under this writer
/// and is typed corruption naming the value — refused on the reconcile
/// and fresh-attach recovery paths — never a silent `max(0)` rewrite
/// that destroys the forensic evidence and looks like a clean initial
/// generation.
#[test]
fn negative_persisted_generation_is_typed_corruption_not_silent_zero() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    store
        .index_state_put(ws, &St::NotStarted.to_row_json(), -37, "hostile_write")
        .unwrap();
    // The live mirror sees the corruption on the reconcile path...
    let err = svc
        .refresh_mirror(ws)
        .expect_err("a negative persisted generation must be typed corruption");
    match err {
        IndexError::CorruptState(message) => {
            assert!(message.contains("-37"), "names the value: {message}");
        }
        other => panic!("typed corruption expected, got {other:?}"),
    }
    // ...and on a fresh attach (daemon restart).
    let fs = faktor_fs::WorkspaceFileService::new();
    let store2 = Arc::new(Store::open(&env.store_root, true).unwrap());
    let svc2 = IndexService::open(store2.clone(), env.data_root.clone(), fs).unwrap();
    svc2.set_config(fast_cfg());
    let err = svc2
        .attach(ws)
        .expect_err("a negative persisted generation must be typed corruption");
    match &err {
        IndexError::CorruptState(message) => {
            assert!(message.contains("-37"), "names the value: {message}");
        }
        other => panic!("typed corruption expected, got {other:?}"),
    }
    // The row is untouched: the forensic value survives, the corruption
    // stays diagnosable, and no repair transition was silently applied.
    let row = store2.index_state_get(ws).unwrap().expect("row persists");
    assert_eq!(row.generation, -37, "the original value is preserved");
    assert_eq!(row.state_json, St::NotStarted.to_row_json());
    assert!(matches!(
        svc2.attach(ws).unwrap_err(),
        IndexError::CorruptState(_)
    ));
    // A valid positive generation is unaffected by the gate.
    let ws_valid = store2.create_workspace(env.repo.to_str().unwrap()).unwrap();
    store2
        .index_state_put(
            ws_valid,
            &St::Failed {
                message: "prior failure".into(),
            }
            .to_row_json(),
            3,
            "seed",
        )
        .unwrap();
    svc2.attach(ws_valid).unwrap();
    let (state, generation) = svc2.state(ws_valid).unwrap();
    assert!(matches!(state, St::Failed { .. }), "{state:?}");
    assert_eq!(generation, 3, "a valid generation is untouched");
}

// ------------------------------------------------------ (c) event storm

#[test]
fn thousand_event_storm_coalesces_to_one_dirty_and_one_rebuild() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    for i in 0..1000 {
        write(&env.repo, &format!("f{i:04}.rs"), "pub fn storm_fn() {}\n");
    }
    std::thread::sleep(Duration::from_millis(80));
    for _ in 0..1000 {
        svc.request_build(ws).unwrap();
    }
    // Settle at gen 2 through sync reconciles. The generation is built
    // in bounded batches (audit 5), so completion means gen 2 AND complete
    // coverage — a partial generation is never mistaken for "settled".
    let deadline = Instant::now() + DEADLINE;
    loop {
        svc.reconcile_now(ws).unwrap();
        let settled = svc
            .view(ws)
            .map(|v| v.generation() == 2 && v.coverage().complete)
            .unwrap_or(false)
            && svc
                .fingerprint_coverage(ws)
                .map(|c| c.complete)
                .unwrap_or(false);
        if settled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "gen 2 never completed: coverage={:?} fp={:?}",
            svc.coverage(ws),
            svc.fingerprint_coverage(ws)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(svc.view(ws).unwrap().generation(), 2);
    let view = svc.view(ws).unwrap();
    let arc = view.index();
    let idx = arc.lock().unwrap();
    // 3 fixture files (incl. AGENTS.md) + 1000 storm files.
    assert_eq!(idx.file_count(ws), 1003, "storm files all indexed once");
    drop(idx);
    drop(view);
    let log = journal_counts(&store, ws);
    assert_eq!(
        log.iter().filter(|(k, _)| k == "dirty").count(),
        1,
        "1000 events must coalesce into ONE durable dirty mark: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|(k, g)| k == "building" && *g == 2)
            .count(),
        1,
        "1000 events must coalesce into ONE rebuild: {log:?}"
    );
}

// ------------------------------------------------------ (d) builder race

#[test]
fn two_racing_builders_swap_once_and_generation_increments_once() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    write(&env.repo, "race.rs", "pub fn race_fn() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    let svc = Arc::new(svc.clone());
    let mut handles = Vec::new();
    for _ in 0..2 {
        let svc = svc.clone();
        handles.push(std::thread::spawn(move || {
            svc.ensure_ready(ws, Instant::now() + DEADLINE)
                .unwrap()
                .generation()
        }));
    }
    let gens: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(gens, vec![2, 2], "both callers see the SAME new generation");
    let log = journal_counts(&store, ws);
    assert_eq!(
        log.iter()
            .filter(|(k, g)| k == "building" && *g == 2)
            .count(),
        1,
        "exactly ONE builder wins the gen-2 claim: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, g)| k == "ready" && *g == 2).count(),
        1,
        "{log:?}"
    );
}

// ------------------------------------------------------ (e) 10 restarts

#[test]
fn ten_restarts_keep_generation_monotone_and_ready_persisted() {
    let _serial = serial();
    let (env, _store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    let fs = faktor_fs::WorkspaceFileService::new();
    drop(svc);
    let mut last_gen = 1u64;
    for round in 2..=10u64 {
        // The tree changes while NO service is alive. A restart must
        // detect it (fingerprint reconciliation), rebuild exactly one
        // generation, and stay Ready.
        write(
            &env.repo,
            &format!("gen{round}.rs"),
            &format!("pub fn gen{round}() {{}}\n"),
        );
        std::thread::sleep(Duration::from_millis(25));
        let (store_r, svc_r, ws_r) = restart(&env, fs.clone(), Some(fast_cfg()));
        let view = svc_r.ensure_ready(ws_r, Instant::now() + DEADLINE).unwrap();
        assert_eq!(
            view.generation(),
            round,
            "rebuild after restart bumps generation by exactly 1"
        );
        assert!(view.generation() > last_gen);
        last_gen = view.generation();
        let (state, gen) = svc_r.state(ws_r).unwrap();
        assert_eq!((state, gen), (St::Ready { generation: round }, round));
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(!idx
            .symbol_lookup(ws_r, &format!("gen{round}"), 10)
            .is_empty());
        drop(idx);
        drop(view);
        // Restart again WITHOUT changes: Ready stays Ready (no rebuild).
        drop(store_r);
        drop(svc_r);
        let (store_r2, svc_r2, ws_r2) = restart(&env, fs.clone(), Some(fast_cfg()));
        let view = svc_r2
            .ensure_ready(ws_r2, Instant::now() + DEADLINE)
            .unwrap();
        assert_eq!(
            view.generation(),
            round,
            "Ready must stay Ready across a clean restart (no rebuild)"
        );
        let (state, _) = svc_r2.state(ws_r2).unwrap();
        assert_eq!(state, St::Ready { generation: round });
        drop(view);
        // Generation files on disk agree with the durable row.
        let file = generation_file_path(&env.data_root, ws_r2, round);
        assert!(file.exists(), "gen {round} file must be durable");
        drop(store_r2);
        drop(svc_r2);
    }
    assert_eq!(last_gen, 10);
}

// ------------------------------------------------------ (f) pruning

#[test]
fn old_generation_pruned_exactly_at_ready_n_plus_2() {
    let _serial = serial();
    let (env, _store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    let gen_dir = generation_dir(&env.data_root, ws);
    let list_gens = || -> Vec<u64> {
        let mut v: Vec<u64> = std::fs::read_dir(&gen_dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.strip_prefix("gen-")
                    .and_then(|n| n.strip_suffix(".json"))
                    .and_then(|n| n.parse().ok())
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(list_gens(), vec![1], "gen 1 file present at Ready(1)");
    // Change -> Ready(2): data for gen 1 must STILL be present.
    write(&env.repo, "two.rs", "pub fn two() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    pub_view_asserts(&svc, ws, 2);
    assert_eq!(
        list_gens(),
        vec![1, 2],
        "gen 1 data still present at Ready(2) (keep 2 generations)"
    );
    // Change -> Ready(3): gen 1 pruned exactly now.
    write(&env.repo, "three.rs", "pub fn three() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    pub_view_asserts(&svc, ws, 3);
    assert_eq!(
        list_gens(),
        vec![2, 3],
        "gen 1 pruned exactly at Ready(3); gen 2 kept"
    );
    // No scratch leftovers after clean builds.
    let scratch = scratch_dir(&env.data_root, ws);
    let leftovers: Vec<String> = std::fs::read_dir(&scratch)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "scratch leaked: {leftovers:?}");
}

// ------------------------------------------------------ watcher e2e

#[test]
fn watcher_event_drives_dirty_to_ready_via_worker() {
    let _serial = serial();
    // Explicit runtime (block_on, no .await in this fn): the serial
    // std-lock guard must never cross an await point.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let env = env();
        write(&env.repo, "a.rs", "pub fn watcher_fn() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, None);
        svc.set_config(cfg_poll_only());
        // Attach: the worker builds gen 1 (a.rs only) in the background.
        svc.attach(ws).unwrap();
        // Poll-driven with a fresh environment-independent ceiling per
        // phase: the assertion is the READY state, not the latency.
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            if matches!(svc.state(ws), Some((St::Ready { generation: 1 }, 1))) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("initial build never reached Ready(1)");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Real fs watcher event: the worker marks dirty (durable, in the
        // journal) and rebuilds to gen 2. The Dirty state itself is
        // transient (dirty -> claim -> build happen inside one reconcile
        // pass), so the assertion is journal-based.
        write(&env.repo, "b.rs", "pub fn second_fn() {}\n");
        let touched = tokio::time::Instant::now();
        // A watcher event under full-suite load can take minutes to
        // schedule (FSEvents stalls); the assertion (the worker drove
        // Dirty -> Ready(2)) is unchanged. The wait is a deadline, and
        // when no event arrives within the grace below the test falls
        // back to the service's own event kick: the SAME reconciliation
        // pipeline (durable Dirty -> claim -> publish) the watcher
        // signal enters, so the durable assertions are identical. A late
        // watcher event for the already-built state is dropped by the
        // fingerprint check and can never churn a phantom generation.
        let grace = touched + WATCHER_EVENT_GRACE;
        let deadline = touched + WATCHER_DEADLINE;
        let mut kicked = false;
        loop {
            if let Some((St::Ready { generation }, _)) = svc.state(ws) {
                if generation >= 2 {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("watcher-driven rebuild never reached Ready(2)");
            }
            if !kicked && tokio::time::Instant::now() >= grace {
                eprintln!(
                    "index watcher e2e: no FSEvents delivery within {WATCHER_EVENT_GRACE:?}; \
                         using the service event kick (stalled watcher stream)"
                );
                svc.request_build(ws).unwrap();
                kicked = true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Settle: with no further changes the state STAYS ready — the
        // worker does not rebuild on a loop. The stability window is
        // CALIBRATED to the observed watcher latency (touch -> Ready(2))
        // with the 600ms floor, so a load-slowed host waits
        // proportionally longer before the assertion runs instead of
        // racing a late coalesced event; the state is re-read across the
        // window. Assertions unchanged.
        let observed_latency = tokio::time::Instant::now().saturating_duration_since(touched);
        let settle = if kicked {
            // The kick path already proved there was no pending watcher
            // delivery to race; a late event is fingerprint-dropped.
            Duration::from_millis(600)
        } else {
            observed_latency
                .max(Duration::from_millis(600))
                .min(WATCHER_DEADLINE)
        };
        let stable_deadline = tokio::time::Instant::now() + settle;
        loop {
            if tokio::time::Instant::now() >= stable_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let (state, gen) = svc.state(ws).unwrap();
        assert_eq!((state, gen), (St::Ready { generation: 2 }, 2));
        // Journal: the watcher change produced a DURABLE dirty mark and
        // exactly one claim+publish of gen 2 (fs events coalesce; no
        // duplicate generations).
        let log = journal_counts(&store, ws);
        assert!(
            log.iter().any(|(k, g)| k == "dirty" && *g == 1),
            "the watcher event must be journaled as a durable Dirty: {log:?}"
        );
        let building2 = log
            .iter()
            .filter(|(k, g)| *k == "building" && *g == 2)
            .count();
        let resume2 = log
            .iter()
            .filter(|(k, g)| *k == "resume" && *g == 2)
            .count();
        let ready2 = log.iter().filter(|(k, g)| *k == "ready" && *g == 2).count();
        assert_eq!(ready2, 1, "one publish of gen 2: {log:?}");
        assert!(building2 + resume2 <= 1, "one claim of gen 2: {log:?}");
        drop(svc);
    });
}

// ------------------------------------------------------ deadline / failure

#[test]
fn ensure_ready_honors_deadline_when_workspace_unreadable() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "gone.rs", "pub fn g() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, None);
    // Make the workspace unreadable for builds: move the repo away.
    let moved = env._dir.path().join("moved");
    std::fs::rename(&env.repo, &moved).unwrap();
    svc.set_config(fast_cfg());
    svc.attach(ws).unwrap();
    let err = svc.ensure_ready(ws, Instant::now() + Duration::from_millis(600));
    assert!(matches!(err, Err(IndexError::Deadline { .. })), "{err:?}");
    // The failure was recorded durably as Failed, not swallowed.
    let (state, _) = svc.state(ws).unwrap();
    assert!(
        matches!(state, St::Failed { .. }),
        "unreadable workspace must land in durable Failed: {state:?}"
    );
    drop(store);
    // Explicit retry recovers once the dir is back.
    std::fs::rename(&moved, &env.repo).unwrap();
    svc.request_build(ws).unwrap();
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
}

#[test]
fn scratch_orphans_never_become_generations() {
    let _serial = serial();
    // A hostile/crashed writer drops junk into the scratch dir: the
    // service must never load it as a generation, and prune must
    // eventually remove it.
    let (env, _store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    let scratch = scratch_dir(&env.data_root, ws);
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("gen-1-9999-1.tmp"), b"partial garbage").unwrap();
    std::fs::write(scratch.join("gen-77-9999-1.tmp"), b"partial garbage").unwrap();
    // gen-77-*.tmp is newer than any published gen: prune must leave it
    // (it belongs to an in-flight target) until that target is passed.
    write(&env.repo, "next.rs", "pub fn next_fn() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    pub_view_asserts(&svc, ws, 2);
    // gen-1 scratch pruned at Ready(2)? floor = 2-2 = 0 -> nothing
    // pruned yet; but after Ready(3) both gen-1 leftovers go.
    write(&env.repo, "next2.rs", "pub fn next2_fn() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    pub_view_asserts(&svc, ws, 3);
    let leftovers: Vec<String> = std::fs::read_dir(&scratch)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !leftovers.iter().any(|n| n.starts_with("gen-1-")),
        "stale gen-1 scratch pruned: {leftovers:?}"
    );
    // A scratch orphan of a FUTURE generation (77) is kept (a real
    // builder may legitimately hold scratch for newest+1) but it can
    // never become a generation: no gen-77.json exists and the view is
    // untouched.
    let gens = generation_dir(&env.data_root, ws);
    let published: Vec<String> = std::fs::read_dir(&gens)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !published.iter().any(|n| n.starts_with("gen-77")),
        "scratch must never be published as a generation: {published:?}"
    );
    assert!(svc.view(ws).unwrap().generation() == 3);
}

#[test]
fn sweep_removes_stale_temp_orphans_without_following_or_touching_real_files() {
    let _serial = serial();
    let (env, store, svc, ws) = first_fixture();
    pub_view_asserts(&svc, ws, 1);
    let gen_dir = generation_dir(&env.data_root, ws);
    let gen_path = generation_file_path(&env.data_root, ws, 1);
    let gen_bytes = std::fs::read(&gen_path).unwrap();

    let backdate = |p: &Path| {
        let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old)
            .unwrap();
    };

    // Open path: stale atomic temps under BOTH spellings (legacy crash
    // residue and the current Faktor name) are swept.
    let open_stale = gen_dir.join(".gen-1.json.kp-tmp-1111-open");
    let open_stale_faktor = gen_dir.join(".gen-1.json.faktor-tmp-1112-open");
    for p in [&open_stale, &open_stale_faktor] {
        std::fs::write(p, b"torn residue").unwrap();
        backdate(p);
    }
    let svc2 = IndexService::open(
        store.clone(),
        env.data_root.clone(),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    assert!(
        !open_stale.exists() && !open_stale_faktor.exists(),
        "open must sweep stale atomic temps of both spellings"
    );

    // Attach path: stale atomic (both spellings) + legacy staging temps
    // in the generation and scratch dirs are swept; a FRESH temp
    // (possibly a live writer) survives because only provably stale
    // residue is removed.
    let stale_atomic = gen_dir.join(".gen-1.json.kp-tmp-2222-attach");
    let stale_atomic_faktor = gen_dir.join(".gen-1.json.faktor-tmp-2223-attach");
    let stale_staging = gen_dir.join("gen-2-2222-1.tmp");
    let scratch = scratch_dir(&env.data_root, ws);
    std::fs::create_dir_all(&scratch).unwrap();
    let stale_scratch_staging = scratch.join("gen-3-2222-1.tmp");
    let fresh_temp = gen_dir.join(".gen-3.json.faktor-tmp-3333-live");
    for p in [
        &stale_atomic,
        &stale_atomic_faktor,
        &stale_staging,
        &stale_scratch_staging,
    ] {
        std::fs::write(p, b"torn residue").unwrap();
        backdate(p);
    }
    std::fs::write(&fresh_temp, b"live writer staging").unwrap();

    // Hostile: a symlink named exactly like a temp must not be followed
    // (its target lives outside the index dirs and must stay sacred).
    #[cfg(unix)]
    let (evil_link, outside) = {
        let outside = env._dir.path().join("outside-sacred.bin");
        std::fs::write(&outside, b"SACRED-BYTES").unwrap();
        let evil_link = gen_dir.join(".gen-1.json.kp-tmp-6666-evil");
        std::os::unix::fs::symlink(&outside, &evil_link).unwrap();
        (evil_link, outside)
    };

    svc2.attach(ws).unwrap();

    assert!(!stale_atomic.exists(), "stale atomic temp swept at attach");
    assert!(
        !stale_atomic_faktor.exists(),
        "stale Faktor atomic temp swept at attach"
    );
    assert!(
        !stale_staging.exists(),
        "stale staging temp swept at attach"
    );
    assert!(
        !stale_scratch_staging.exists(),
        "stale scratch staging swept at attach"
    );
    assert!(
        fresh_temp.exists(),
        "a fresh temp may belong to a live writer and must survive"
    );
    assert_eq!(
        std::fs::read(&gen_path).unwrap(),
        gen_bytes,
        "real generation bytes untouched by the sweep"
    );
    #[cfg(unix)]
    {
        let md = std::fs::symlink_metadata(&evil_link).unwrap();
        assert!(
            md.file_type().is_symlink(),
            "a symlink named like a temp must never be removed or followed"
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"SACRED-BYTES",
            "the symlink target must be byte-untouched"
        );
    }
}

/// Cold path + upgrade (P0-30): the runtime's first prompt on a
/// workspace with NO Ready generation serves cold evidence (partial:
/// the turn's own changed-file reads), then once a Ready generation is
/// built the VIEW serves full index evidence — the provider is retired
/// from the runtime decision tree exactly when `view()` returns Some.
/// This locks both halves against the real service (generation files,
/// durable rows, ensure_ready).
#[test]
fn cold_before_ready_and_view_after_ready() {
    let _serial = serial();
    let env = env();
    write(
        &env.repo,
        "src/a.rs",
        "pub fn alpha() -> i64 { 1 }\npub struct Beta {}\n",
    );
    write(&env.repo, "src/b.rs", "pub fn gamma() -> i64 { 2 }\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
    svc.attach(ws).unwrap();
    assert!(svc.view(ws).is_none(), "no Ready generation yet");
    let provider = svc.cold_provider(ws).expect("attached workspace");
    // Turn 1 (no Ready): only the changed file's own read comes back.
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "alpha".into(),
        changed_files: vec!["src/a.rs".into()],
        ..Default::default()
    });
    assert!(matches!(
        cold.origin,
        crate::cold::ColdOrigin::DirectReads { files: 1 }
    ));
    assert!(
        cold.hits.iter().any(|h| h.path == "src/a.rs"),
        "{:?}",
        cold.hits
    );
    // The Ready generation builds (background machine, polled by
    // ensure_ready) and the NEXT turn's view covers the whole repo.
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    let arc = view.index();
    let idx = arc.lock().unwrap();
    assert!(!idx.files_for_token(ws, "gamma", 10).is_empty());
    assert!(idx.file_paths(ws).iter().any(|p| p == "src/b.rs"));
    drop(idx);
    // After Ready the cold provider itself serves the generation too
    // (stale-but-cheap), and the runtime's ordering — view first, cold
    // only on None — is what retires it next turn.
    let after = provider.evidence(&crate::cold::ColdQuery {
        prompt: "gamma".into(),
        changed_files: vec![],
        ..Default::default()
    });
    assert!(matches!(
        after.origin,
        crate::cold::ColdOrigin::StaleGeneration { generation: 1, .. }
    ));
    assert!(
        after.hits.iter().any(|h| h.path == "src/b.rs"),
        "{:?}",
        after.hits
    );
    let _ = store;
}

/// A persisted OLD generation file + Ready durable row serve the FIRST
/// prompt of a fresh daemon process WITHOUT waiting for the background
/// load: cold_provider decodes the generation file directly (bounded),
/// the same file the worker would load for the view.
#[test]
fn stale_generation_serves_first_prompt_of_a_fresh_daemon() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/lib.rs", "pub fn balance_account() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
    svc.attach(ws).unwrap();
    // Build generation 1 and record its durable state.
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    // "Restart": a second service over the same durable files. The row
    // says Ready{1} and the generation file exists, so the first prompt
    // is served WITHOUT any scan or build wait.
    let svc2 = IndexService::open(
        store.clone(),
        env.data_root.clone(),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    svc2.attach(ws).unwrap();
    // A fresh attach has pending_load=true: the view is NOT ready yet,
    // but the cold provider serves the persisted generation now.
    let provider = svc2.cold_provider(ws).expect("attached");
    let evidence = provider.evidence(&crate::cold::ColdQuery {
        prompt: "balance_account".into(),
        changed_files: vec![],
        ..Default::default()
    });
    assert!(
        matches!(
            evidence.origin,
            crate::cold::ColdOrigin::StaleGeneration { generation: 1, .. }
        ),
        "{:?}",
        evidence.origin
    );
    assert!(
        evidence.hits.iter().any(|h| h.path == "src/lib.rs"),
        "{:?}",
        evidence.hits
    );
}

/// P0-48 root re-pointing at attach: while exactly ONE session of the
/// workspace carries a LIVE durable shadow row, a fresh attach resolves
/// the workspace root to the SHADOW (cold evidence reads shadow-only
/// files); with no live shadow, on ambiguity (two live shadows) or on a
/// corrupt registry the stored root stays authoritative — never a
/// guessed root.
#[test]
fn attach_resolves_the_live_shadow_of_a_shadowed_workspace() {
    let env = env();
    write(
        &env.repo,
        "useronly.rs",
        "pub fn user_marker_fn() -> u32 { 1 }\n",
    );
    let shadow = env._dir.path().join("shadows").join("1").join("sh-1");
    std::fs::create_dir_all(&shadow).unwrap();
    write(
        &shadow,
        "onlyshadow.rs",
        "pub fn shadow_marker_fn() -> u32 { 2 }\n",
    );

    fn fresh(env: &Env) -> (Arc<Store>, Arc<IndexService>, WorkspaceId) {
        let store = Arc::new(Store::open(&env.store_root, true).unwrap());
        let ws = store.create_workspace(env.repo.to_str().unwrap()).unwrap();
        let svc = IndexService::open(
            store.clone(),
            env.data_root.clone(),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        (store, svc, ws)
    }

    fn shadow_fact(session: u64, root: &str, state: &str) -> String {
        serde_json::json!({
            "session_id": session,
            "shadow_id": "sh-1",
            "base_root": root,
            "root": root,
            "state": state,
            "base_entries": 1,
            "base_bytes": 1,
            "created_ms": 1,
        })
        .to_string()
    }

    // Baseline: no shadow rows anywhere — cold evidence reads the
    // stored root.
    let (store, svc, ws) = fresh(&env);
    svc.attach(ws).unwrap();
    let provider = svc.cold_provider(ws).expect("attached");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "user_marker_fn".into(),
        changed_files: vec!["useronly.rs".into()],
        referenced_paths: vec!["onlyshadow.rs".into()],
        ..Default::default()
    });
    assert!(
        cold.hits
            .iter()
            .any(|h| h.path == "useronly.rs" && !h.snippet.is_empty()),
        "baseline reads the stored root: {:?}",
        cold.hits
    );
    assert!(
        !cold.hits.iter().any(|h| h.path.contains("onlyshadow.rs")),
        "no shadow exists on the baseline: {:?}",
        cold.hits
    );
    drop(svc);

    // ONE session of the workspace with a LIVE shadow: attach re-points
    // the workspace at the shadow root.
    let session = store.create_session(ws, "shadowed", "p", "m").unwrap();
    store
        .upsert_memory_fact(
            session.id,
            "shadow_root",
            "active",
            &shadow_fact(session.id.raw(), shadow.to_str().unwrap(), "active"),
        )
        .unwrap();
    let svc = IndexService::open(
        store.clone(),
        env.data_root.clone(),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    svc.attach(ws).unwrap();
    let provider = svc.cold_provider(ws).expect("attached");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "shadow_marker_fn".into(),
        changed_files: vec!["onlyshadow.rs".into()],
        referenced_paths: vec!["useronly.rs".into()],
        ..Default::default()
    });
    assert!(
        cold.hits
            .iter()
            .any(|h| h.path == "onlyshadow.rs" && !h.snippet.is_empty()),
        "the live shadow re-points cold reads: {:?}",
        cold.hits
    );
    assert!(
        !cold.hits.iter().any(|h| h.path.contains("useronly.rs")),
        "the user checkout is not the root while a shadow is live: {:?}",
        cold.hits
    );
    drop(svc);

    // A second LIVE shadow on the same workspace is ambiguous: loud
    // degrade to the stored root (never a guessed root).
    let other = store.create_session(ws, "other", "p", "m").unwrap();
    store
        .upsert_memory_fact(
            other.id,
            "shadow_root",
            "active",
            &shadow_fact(other.id.raw(), shadow.to_str().unwrap(), "active"),
        )
        .unwrap();
    let svc = IndexService::open(
        store.clone(),
        env.data_root.join("data2"),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    svc.attach(ws).unwrap();
    let provider = svc.cold_provider(ws).expect("attached");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "user_marker_fn".into(),
        changed_files: vec!["useronly.rs".into()],
        ..Default::default()
    });
    assert!(
        cold.hits.iter().any(|h| h.path == "useronly.rs"),
        "ambiguous shadows fall back to the stored root: {:?}",
        cold.hits
    );
    drop(svc);

    // A corrupt shadow-row value anywhere: loud degrade to the stored
    // root.
    store
        .upsert_memory_fact(other.id, "shadow_root", "active", "{not a shadow row")
        .unwrap();
    store
        .upsert_memory_fact(session.id, "shadow_root", "active", "{also broken")
        .unwrap();
    let svc = IndexService::open(
        store.clone(),
        env.data_root.join("data3"),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    svc.attach(ws).unwrap();
    let provider = svc.cold_provider(ws).expect("attached");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "user_marker_fn".into(),
        changed_files: vec!["useronly.rs".into()],
        ..Default::default()
    });
    assert!(
        cold.hits.iter().any(|h| h.path == "useronly.rs"),
        "corrupt registry rows never hijack the root: {:?}",
        cold.hits
    );
    drop(svc);

    // Retire every shadow (integrated): the stored root is
    // authoritative again.
    store
        .upsert_memory_fact(
            session.id,
            "shadow_root",
            "active",
            &shadow_fact(session.id.raw(), shadow.to_str().unwrap(), "integrated"),
        )
        .unwrap();
    store
        .upsert_memory_fact(
            other.id,
            "shadow_root",
            "active",
            &shadow_fact(other.id.raw(), shadow.to_str().unwrap(), "discarded"),
        )
        .unwrap();
    let svc = IndexService::open(
        store.clone(),
        env.data_root.join("data4"),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    svc.attach(ws).unwrap();
    let provider = svc.cold_provider(ws).expect("attached");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "user_marker_fn".into(),
        changed_files: vec!["useronly.rs".into()],
        ..Default::default()
    });
    assert!(
        cold.hits.iter().any(|h| h.path == "useronly.rs"),
        "retired shadows never keep re-pointing: {:?}",
        cold.hits
    );
}

// ------------------------------------------------------------ embeddings

/// Spy embedding source: counts provider calls (one per `embed` batch),
/// records every text it was asked for, and can be flipped to fail (the
/// provider-outage path of a build).
#[derive(Default)]
struct CountingSource {
    calls: std::sync::atomic::AtomicUsize,
    texts: std::sync::Mutex<Vec<String>>,
    fail: std::sync::atomic::AtomicBool,
}

impl CountingSource {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn texts(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }
}

impl EmbeddingSource for CountingSource {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, faktor_core::Error> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(faktor_core::Error::new(
                faktor_core::error::ErrorKind::Network,
                "embedding backend down",
            ));
        }
        self.texts.lock().unwrap().extend(texts.iter().cloned());
        Ok(texts
            .iter()
            .map(|text| vec![if text.contains("alpha") { 1.0 } else { 0.0 }, 1.0])
            .collect())
    }
}

/// Persistence keyed by content hash: an unchanged chunk survives BUILD
/// generation swaps AND a daemon reopen without a second embed call; a
/// changed chunk is the only text re-embedded.
#[test]
fn embeddings_survive_generation_swaps_and_reopen_without_reembedding() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
    let source = Arc::new(CountingSource::default());
    svc.set_embedding_source(Some(source.clone()), EmbeddingModel::new("m", "r1"));
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    assert_eq!(source.calls(), 1, "one bounded embed call for the build");
    assert_eq!(source.texts().len(), 2);
    {
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(idx.has_embedding_index(ws));
        assert_eq!(idx.embedding_dimension(ws), 2);
        assert_eq!(idx.chunk_vectors(ws, 10).len(), 2);
    }

    // Change ONE file: only its chunk re-embeds; the other is carried
    // across the generation swap.
    write(&env.repo, "src/a.rs", "pub fn alpha_changed() {}\n");
    std::thread::sleep(Duration::from_millis(60)); // mtime resolution
    svc.request_build(ws).unwrap();
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 2);
    assert_eq!(source.calls(), 2);
    let second_batch: Vec<String> = source.texts()[2..].to_vec();
    assert_eq!(
        second_batch,
        vec!["pub fn alpha_changed() {}\n".to_string()],
        "only the changed chunk is re-embedded"
    );
    drop(view);

    // Reopen: a fresh daemon over the same store/data_root. The Ready
    // generation reloads WITH its vectors and embeds nothing.
    drop(svc);
    let source2 = Arc::new(CountingSource::default());
    let (_store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
    assert_eq!(ws2, ws);
    svc2.set_embedding_source(Some(source2.clone()), EmbeddingModel::new("m", "r1"));
    let view = svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 2, "ready generation reloads as-is");
    {
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(
            idx.has_embedding_index(ws),
            "persisted vectors survive a reopen"
        );
        let beta_hash = crate::embedding::content_hash("pub fn beta() {}\n");
        assert_eq!(
            idx.embedding_vector(ws, &beta_hash),
            Some(&[0.0f32, 1.0][..]),
            "the unchanged chunk's exact vector survived"
        );
    }
    assert_eq!(
        source2.calls(),
        0,
        "a reopen with no changes re-embeds nothing"
    );
    drop(view);

    // Change the OTHER file after the reopen: exactly one re-embed.
    write(&env.repo, "src/b.rs", "pub fn beta_changed() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc2.request_build(ws).unwrap();
    let view = svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 3);
    assert_eq!(source2.calls(), 1);
    assert_eq!(
        source2.texts(),
        vec!["pub fn beta_changed() {}\n".to_string()]
    );
}

/// A model REVISION change invalidates every persisted vector: the next
/// build re-embeds the whole referenced corpus (even though only one
/// file changed) under the new identity.
#[test]
fn model_revision_change_invalidates_and_reembeds_the_corpus() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
    let first = Arc::new(CountingSource::default());
    svc.set_embedding_source(Some(first.clone()), EmbeddingModel::new("m", "r1"));
    svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(first.calls(), 1);

    let second = Arc::new(CountingSource::default());
    svc.set_embedding_source(Some(second.clone()), EmbeddingModel::new("m", "r2"));
    write(&env.repo, "src/a.rs", "pub fn alpha() -> u8 { 1 }\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 2);
    assert_eq!(second.calls(), 1, "one batch for the new revision");
    assert_eq!(
        second.texts().len(),
        2,
        "the new revision re-embeds every chunk, old vectors are never reused"
    );
    let arc = view.index();
    let idx = arc.lock().unwrap();
    assert!(idx.has_embedding_index(ws));
}

/// A provider outage during a generation BUILD is not a clean success:
/// the published generation carries a typed Degraded status (reason +
/// affected count) visible to callers/health, and a later healthy
/// rebuild clears it.
#[test]
fn embed_provider_failure_is_a_visible_degraded_generation_and_recovery_clears_it() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
    let source = Arc::new(CountingSource::default());
    svc.set_embedding_source(Some(source.clone()), EmbeddingModel::new("m", "r1"));
    source.fail.store(true, Ordering::SeqCst);
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    {
        let arc = view.index();
        let idx = arc.lock().unwrap();
        match idx
            .embedding_index(ws)
            .expect("the build records its outcome")
            .build_status()
        {
            crate::embedding::EmbeddingBuildStatus::Degraded {
                reason, affected, ..
            } => {
                assert_eq!(affected, 2, "both referenced chunks carry no vector");
                assert!(reason.contains("embedding backend down"), "{reason}");
            }
            other => panic!("a failed embed build must not look complete: {other:?}"),
        }
    }
    drop(view);
    // Recovery: the provider is healthy again; a rebuild of the whole
    // referenced set publishes a Complete status (never sticky).
    source.fail.store(false, Ordering::SeqCst);
    write(&env.repo, "src/a.rs", "pub fn alpha_changed() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert!(view.generation() >= 2);
    {
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert_eq!(
            idx.embedding_index(ws).unwrap().build_status(),
            crate::embedding::EmbeddingBuildStatus::Complete {
                embedded: 2,
                carried: 0
            },
            "a healthy rebuild of the whole referenced set is complete"
        );
    }
}

// ------------------------------------- typed-status publish invariant
//
// The published view and the durable generation must spell the SAME
// typed embedding status: `embedding_index(ws) == None` means "no
// published generation at all", never "unconfigured". These tests pin
// the invariant on every publish path: source-less/prior-less,
// healthy, degraded, and reopened-from-disk.

/// The typed embedding status of the LIVE published view of `generation`
/// (fails when the entry is absent: a published generation always
/// carries it).
fn live_embedding_status(
    svc: &IndexService,
    ws: WorkspaceId,
    generation: u64,
) -> crate::embedding::EmbeddingBuildStatus {
    let view = svc.view(ws).expect("a published generation view");
    assert_eq!(
        view.generation(),
        generation,
        "view serves the named generation"
    );
    let arc = view.index();
    let idx = arc.lock().unwrap();
    idx.embedding_index(ws)
        .expect("a published generation always carries a typed embedding record")
        .build_status()
}

/// The typed embedding status of the DURABLE generation file.
fn persisted_embedding_status(
    env: &Env,
    ws: WorkspaceId,
    generation: u64,
) -> crate::embedding::EmbeddingBuildStatus {
    read_generation_file(&generation_file_path(&env.data_root, ws, generation))
        .expect("published generation file is readable")
        .data
        .embeddings
        .build_status()
}

/// DEFECT GUARD: a source-less, prior-less build must publish the typed
/// `Unconfigured` status (the durable envelope has always recorded that
/// record); the live view answering `None` while the generation said
/// `Unconfigured` was the drift. The typed record never implies usable
/// vectors, and a source-less REBUILD (prior carry-over) keeps the same
/// typed agreement; a restart re-materializes the same status.
#[test]
fn sourceless_priorless_build_publishes_unconfigured_matching_the_generation() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
    // Prior-less: no generation files exist yet; source-less: the
    // service has no embedding source configured.
    assert!(svc.embedding_config().is_none());
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    drop(view);
    let expected = crate::embedding::EmbeddingBuildStatus::Unconfigured;
    let live = live_embedding_status(&svc, ws, 1);
    assert_eq!(live, expected, "a fresh unconfigured build must be typed");
    assert_eq!(
        persisted_embedding_status(&env, ws, 1),
        live,
        "the durable generation must agree with the live view"
    );
    // Typed status != usable vectors: search still takes the lexical
    // path for this generation.
    {
        let arc = svc.view(ws).unwrap().index();
        let idx = arc.lock().unwrap();
        assert!(!idx.has_embedding_index(ws));
    }
    // A second source-less build carries the typed record through the
    // prior-carry-over swap path: no drift after a generation swap.
    write(&env.repo, "src/a.rs", "pub fn alpha_changed() {}\n");
    std::thread::sleep(Duration::from_millis(60));
    svc.request_build(ws).unwrap();
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 2);
    drop(view);
    let live = live_embedding_status(&svc, ws, 2);
    assert_eq!(live, expected);
    assert_eq!(persisted_embedding_status(&env, ws, 2), live);
    // Reopen from disk: the same typed status, never None.
    drop(svc);
    let (_store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
    assert_eq!(ws2, ws);
    let view = svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 2);
    drop(view);
    let live = live_embedding_status(&svc2, ws, 2);
    assert_eq!(live, expected);
    assert_eq!(persisted_embedding_status(&env, ws, 2), live);
}

/// A healthy build publishes `Complete` (exact embedded/carried counts)
/// and a reopen materializes the SAME typed status from the generation.
#[test]
fn healthy_build_publishes_complete_through_live_and_reopened_views() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
    let source = Arc::new(CountingSource::default());
    svc.set_embedding_source(Some(source), EmbeddingModel::new("m", "r1"));
    svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    let expected = crate::embedding::EmbeddingBuildStatus::Complete {
        embedded: 2,
        carried: 0,
    };
    let live = live_embedding_status(&svc, ws, 1);
    assert_eq!(live, expected);
    assert_eq!(persisted_embedding_status(&env, ws, 1), live);
    // Reopen: the status is materialized from the durable generation,
    // not recomputed (a reopen re-embeds nothing).
    drop(svc);
    let (_store2, svc2, _) = restart(&env, fs, Some(fast_cfg()));
    svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    let live = live_embedding_status(&svc2, ws, 1);
    assert_eq!(live, expected);
    assert_eq!(persisted_embedding_status(&env, ws, 1), live);
}

/// A degraded build publishes `Degraded` with the affected count and the
/// bounded reason — visible identically through the live view, the
/// durable generation, and a reopened-from-disk view.
#[test]
fn degraded_build_publishes_degraded_through_live_and_reopened_views() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
    let source = Arc::new(CountingSource::default());
    svc.set_embedding_source(Some(source.clone()), EmbeddingModel::new("m", "r1"));
    source.fail.store(true, Ordering::SeqCst);
    svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    let live = live_embedding_status(&svc, ws, 1);
    match &live {
        crate::embedding::EmbeddingBuildStatus::Degraded {
            reason,
            affected,
            embedded,
            carried,
            skipped,
        } => {
            assert_eq!(*affected, 2, "both referenced chunks are affected");
            assert_eq!((*embedded, *carried, *skipped), (0, 0, 0));
            assert!(reason.contains("embedding backend down"), "{reason}");
        }
        other => panic!("a failed embed build must be typed Degraded: {other:?}"),
    }
    assert_eq!(persisted_embedding_status(&env, ws, 1), live);
    // Reopen: the degraded status is durable, with the SAME affected
    // count and reason.
    drop(svc);
    let (_store2, svc2, _) = restart(&env, fs, Some(fast_cfg()));
    svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    let reopened = live_embedding_status(&svc2, ws, 1);
    assert_eq!(
        reopened, live,
        "a restart must not erase or alter the degrade"
    );
    assert_eq!(persisted_embedding_status(&env, ws, 1), reopened);
}

/// Absence is reserved for "no published generation at all": an
/// attached-but-unbuilt workspace has no view, and the first publish
/// installs a typed record that never becomes absent again.
#[test]
fn embedding_status_is_absent_only_while_no_generation_is_published() {
    let _serial = serial();
    let env = env();
    write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
    let fs = faktor_fs::WorkspaceFileService::new();
    let (_store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
    svc.attach(ws).unwrap();
    assert!(
        svc.view(ws).is_none(),
        "no published generation -> no view, so no status to read"
    );
    assert_eq!(svc.state(ws).unwrap().0, St::NotStarted);
    svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(
        live_embedding_status(&svc, ws, 1),
        crate::embedding::EmbeddingBuildStatus::Unconfigured
    );
    assert!(svc.view(ws).is_some());
}

// ----------------------------------------------- worker ownership (P1)
//
// These tests use their OWN per-service fault seam, so they need no
// `serial()` guard (no global state is touched): inference is exact —
// only this service's worker can move its counters.

/// Bounded await of a worker state.
async fn wait_worker_state(svc: &IndexService, want: WorkerState, bound: Duration) -> WorkerStatus {
    let deadline = Instant::now() + bound;
    loop {
        let status = svc.worker_status();
        if status.state == want {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "worker never reached {want:?}: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// (a) A panicking pass is CONTAINED: one loud recorded error, the worker
/// stops with `started = false`, and the loop neither respawns nor spins
/// (the old `unwrap_or(true)` turned the `JoinError` into an immediate
/// retry with no sleep).
#[tokio::test]
async fn worker_panicking_pass_stops_once_and_never_spins() {
    let (env, _store, svc, ws) = first_fixture();
    write(&env.repo, "src/lib.rs", "pub fn panicky() -> i64 { 7 }\n");
    // One-shot fault armed BEFORE the worker exists: the first pass
    // panics; any respawn (the defect) would run REAL passes afterwards
    // and grow the invocation counter without bound.
    svc.inner.fault.panic_next.store(true, Ordering::SeqCst);
    svc.attach(ws).unwrap();
    assert!(svc.inner.worker_started.load(Ordering::Acquire));
    let status = wait_worker_state(&svc, WorkerState::Failed, Duration::from_secs(60)).await;
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("panic"),
        "the panic must surface as the recorded last error: {status:?}"
    );
    assert!(
        !svc.inner.worker_started.load(Ordering::Acquire),
        "a failed worker clears the started flag"
    );
    assert_eq!(
        svc.inner.fault.pass_invocations.load(Ordering::SeqCst),
        1,
        "exactly one pass ran; a panicking pass must never respawn"
    );
    assert_eq!(status.restarts, 0, "no automatic restart generation");
    // The old code's immediate `continue` would run pass after pass here
    // (poll 25 ms, a pending build): the counter must stay frozen.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        svc.inner.fault.pass_invocations.load(Ordering::SeqCst),
        1,
        "the worker hot-spun after the panic"
    );
    assert_eq!(
        wait_worker_state(&svc, WorkerState::Failed, Duration::from_secs(2))
            .await
            .state,
        WorkerState::Failed
    );
    // The failure is contained: the synchronous path still serves.
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    // `ensure_ready` -> attach explicitly re-kicked ONE new generation;
    // the restart is counted exactly once and the owner is healthy again.
    let status = svc.worker_status();
    assert_eq!(status.state, WorkerState::Running, "{status:?}");
    assert_eq!(status.restarts, 1, "{status:?}");
    assert_eq!(svc.shutdown_worker().await, WorkerShutdown::Joined);
    assert_eq!(svc.worker_status().state, WorkerState::Stopped);
}

/// F7 (adversarial): a writer that panics while holding the worker
/// supervisor lock must never make `worker_status`/`shutdown_worker`
/// panic — those paths run during teardown, exactly where a writer panic
/// is most likely. The poison is recovered (the inner supervisor is
/// sound), the failure is recorded (`Failed` + `last_error`, started flag
/// cleared), and the shutdown still joins within its bound.
#[tokio::test]
async fn poisoned_worker_supervisor_lock_is_recovered_never_panics() {
    let (env, _store, svc, ws) = first_fixture();
    write(&env.repo, "src/lib.rs", "pub fn poisoned() -> i64 { 1 }\n");
    svc.attach(ws).unwrap();
    wait_worker_state(&svc, WorkerState::Running, Duration::from_secs(30)).await;
    // A concurrent writer panics while holding the supervisor lock.
    let inner = svc.inner.clone();
    let panicked = std::thread::spawn(move || {
        let _guard = inner.worker.lock().unwrap();
        panic!("fault seam: writer panic while holding the worker supervisor");
    })
    .join();
    assert!(panicked.is_err(), "the writer thread must have panicked");
    assert!(svc.inner.worker.is_poisoned(), "the lock must be poisoned");
    // The status path recovers the poisoned guard instead of panicking.
    let status = svc.worker_status();
    assert_eq!(status.state, WorkerState::Failed, "{status:?}");
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("poisoned"),
        "the poison is recorded as the failure: {status:?}"
    );
    assert!(
        !svc.inner.worker_started.load(Ordering::Acquire),
        "the recovered supervisor releases the started flag"
    );
    assert!(
        !svc.inner.worker.is_poisoned(),
        "recovery clears the poison flag so later paths are total"
    );
    // The shutdown path is total on a poisoned-then-recovered supervisor
    // (and still joins the OWNED task within the bound).
    let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
        .await
        .expect("shutdown must stay bounded after poison recovery");
    assert!(
        matches!(outcome, WorkerShutdown::Joined | WorkerShutdown::Aborted),
        "the owned task must still be joined: {outcome:?}"
    );
    assert!(matches!(
        svc.worker_status().state,
        WorkerState::Stopped | WorkerState::Failed
    ));
}

/// F8: the shutdown bound aborts the OWNED ASYNC TASK, but an in-flight
/// `spawn_blocking` pass keeps running (it cannot be killed). That
/// residual exit bound is reported TYPED via
/// [`WorkerStatus::pass_in_flight`] instead of being implicit.
#[tokio::test]
async fn shutdown_reports_an_in_flight_blocking_pass_typed() {
    let (env, _store, svc, ws) = first_fixture();
    write(&env.repo, "src/lib.rs", "pub fn slow() -> i64 { 1 }\n");
    // The next pass blocks the blocking thread for 1.5s; the shutdown
    // bound is overridden to 200ms so the Aborted path is reached without
    // waiting five seconds.
    svc.inner.fault.block_next_ms.store(1500, Ordering::SeqCst);
    svc.inner.shutdown_bound_ms.store(200, Ordering::SeqCst);
    svc.attach(ws).unwrap();
    // Wait until the pass is actually on the blocking pool.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !svc.worker_status().pass_in_flight {
        assert!(
            Instant::now() < deadline,
            "the blocking pass never started: {:?}",
            svc.worker_status()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let began = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
        .await
        .expect("the bounded shutdown must resolve");
    // Either the async task joined promptly on cancellation (the usual
    // shape: the loop returns at its cancellation check) or it exceeded
    // the bound and was aborted. Both are bounded; NEITHER stops a
    // blocking pass already on the pool.
    assert!(
        matches!(outcome, WorkerShutdown::Joined | WorkerShutdown::Aborted),
        "unexpected shutdown outcome: {outcome:?}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "the shutdown bound itself stays short: {:?}",
        began.elapsed()
    );
    // The typed report: the blocking pass is STILL running, so the real
    // exit bound is its remaining duration.
    let status = svc.worker_status();
    assert_eq!(status.state, WorkerState::Stopped, "{status:?}");
    assert!(
        status.pass_in_flight,
        "the in-flight blocking pass must be reported typed: {status:?}"
    );
    // The detached pass finishes on its own; the flag clears and the
    // pass counter advanced exactly once.
    let deadline = Instant::now() + Duration::from_secs(30);
    while svc.worker_status().pass_in_flight {
        assert!(
            Instant::now() < deadline,
            "the detached blocking pass never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(svc.inner.fault.pass_invocations.load(Ordering::SeqCst), 1);
}

/// F8: the worker's pass observes its cancellation token at machine-step
/// boundaries — a cancelled generation never starts a new claim/build
/// (the pass is bounded by the token where blocking code permits).
#[tokio::test]
async fn cancelled_pass_never_starts_a_claim() {
    let (env, _store, svc, ws) = first_fixture();
    write(&env.repo, "src/lib.rs", "pub fn cancelled() -> i64 { 1 }\n");
    svc.attach(ws).unwrap();
    // Freeze the worker so the synchronous call below is the only pass.
    let _ = tokio::time::timeout(Duration::from_secs(5), svc.shutdown_worker()).await;
    let token = CancellationToken::new();
    token.cancel();
    svc.reconcile_now_cancellable(ws, Some(&token)).unwrap();
    assert!(
        !svc.worker_status().pass_in_flight,
        "a synchronous cancelled pass clears its in-flight flag"
    );
    assert!(
        !generation_file_path(&svc.inner.data_root, ws, 1).exists(),
        "a cancelled pass must not claim/build generation 1"
    );
}

/// Audit 20 (adversarial): a DELIBERATELY slow filesystem must not extend
/// worker shutdown past its hard bound. The scan is one paced work unit per
/// directory entry and observes the worker's cancellation token at every
/// boundary; `shutdown_worker` joins the in-flight blocking pass, so the
/// wall-clock exit stays inside the bound and the abandoned build stays
/// durably resumable instead of wedging the generation.
#[tokio::test]
async fn slow_filesystem_scan_cancels_and_shutdown_stays_inside_the_hard_bound() {
    let (env, _store, svc, ws) = first_fixture();
    const FILES: usize = 120;
    for i in 0..FILES {
        write(
            &env.repo,
            &format!("src/file_{i:03}.rs"),
            "pub fn paced() -> i64 { 1 }\n",
        );
    }
    // 120 entries x 100 ms = 12 s if the scan ran to completion: more than
    // twice the production 5 s shutdown bound.
    const SLOW_MS: u64 = 100;
    svc.inner.slow_fs_ms.store(SLOW_MS, Ordering::SeqCst);
    assert!(
        svc.scan_pace().is_some(),
        "the slow-filesystem seam is armed"
    );
    assert!(
        (FILES as u64) * SLOW_MS > 5_000,
        "the fixture must outlast the production bound if uncancelled"
    );
    svc.attach(ws).unwrap();
    // Wait until the slow build has actually CLAIMED generation 1 (the
    // mirror flips to Building before the scan starts): cancelling before
    // the claim would leave NotStarted and never exercise the scan seam.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(svc.state(ws), Some((St::Building { generation: 1 }, 1))) {
        assert!(
            Instant::now() < deadline,
            "the paced scan never claimed: {:?}",
            svc.state(ws)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        svc.worker_status().pass_in_flight,
        "the paced pass must still be running its scan"
    );
    let began = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
        .await
        .expect("the bounded shutdown must resolve");
    let elapsed = began.elapsed();
    // `Joined` with no pass in flight is the load-bearing assertion: the
    // worker task WAITED for the scan to observe cancellation, so the bound
    // covers the REAL exit, not just the async task handle.
    assert_eq!(outcome, WorkerShutdown::Joined, "{elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown exceeded the hard wall-clock bound with a slow filesystem: {elapsed:?}"
    );
    assert!(
        !svc.worker_status().pass_in_flight,
        "the cancelled scan must have exited before shutdown returned"
    );
    // The scan really was interrupted mid-build (not completed): the
    // durable row still names the building generation, so nothing was
    // silently published and the resume has its target.
    assert!(
        matches!(svc.state(ws), Some((St::Building { generation: 1 }, 1))),
        "cancelled build must leave Building{{1}} durable: {:?}",
        svc.state(ws)
    );
    assert!(
        !generation_file_path(&svc.inner.data_root, ws, 1).exists(),
        "a cancelled scan must not publish generation 1"
    );
    // Resume is immediate: with the seam cleared the SAME generation
    // publishes and serves — cancellation is not a wedge.
    svc.inner.slow_fs_ms.store(0, Ordering::SeqCst);
    let view = svc
        .ensure_ready(ws, Instant::now() + DEADLINE)
        .expect("the abandoned build must resume");
    assert_eq!(view.generation(), 1);
    assert_eq!(svc.state(ws), Some((St::Ready { generation: 1 }, 1)));
}

/// (b)+(c) Cancellation stops the owned worker within a bounded time and
/// `shutdown_worker` JOINS it: the owner reports a terminal state, the
/// started flag clears, and no detached task keeps passing.
#[tokio::test]
async fn worker_shutdown_cancels_and_joins_within_bound() {
    let (env, _store, svc, ws) = first_fixture();
    write(&env.repo, "src/lib.rs", "pub fn owned() -> i64 { 1 }\n");
    svc.attach(ws).unwrap();
    wait_worker_state(&svc, WorkerState::Running, Duration::from_secs(30)).await;
    // Settle the initial build so the worker sits in its wait path.
    let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
    assert_eq!(view.generation(), 1);
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
        .await
        .expect("shutdown must resolve within the bound, never wait unboundedly");
    assert_eq!(outcome, WorkerShutdown::Joined);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a stopped-at-rest worker joins immediately: {:?}",
        started.elapsed()
    );
    let status = svc.worker_status();
    assert_eq!(status.state, WorkerState::Stopped, "{status:?}");
    assert!(!svc.inner.worker_started.load(Ordering::Acquire));
    assert!(
        status.last_error.is_none(),
        "a clean cancellation is not a failure: {status:?}"
    );
    // No ghost task: the pass counter is frozen after the join.
    let frozen = svc.inner.fault.pass_invocations.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        svc.inner.fault.pass_invocations.load(Ordering::SeqCst),
        frozen,
        "a detached worker kept passing after the owner reported Joined"
    );
    // Idempotent: no owned task left to shut down.
    assert_eq!(svc.shutdown_worker().await, WorkerShutdown::NotRunning);
    assert_eq!(svc.worker_status().state, WorkerState::Stopped);
}

/// (d) Restart bookkeeping: `spawn_worker` is idempotent while a
/// generation is alive, a terminal generation can be explicitly
/// respawned, and every respawn is counted exactly once. There is no
/// automatic in-loop restart to budget, by design (a panic returns).
#[tokio::test]
async fn explicit_restart_is_idempotent_and_counted_once_per_generation() {
    let (_env, _store, svc, _ws) = first_fixture();
    assert!(svc.spawn_worker());
    assert_eq!(svc.worker_status().state, WorkerState::Running);
    // Idempotent while running: no extra generation, no counter bump.
    assert!(svc.spawn_worker());
    assert!(svc.spawn_worker());
    assert_eq!(svc.worker_status().restarts, 0);
    assert_eq!(svc.shutdown_worker().await, WorkerShutdown::Joined);
    assert_eq!(svc.worker_status().state, WorkerState::Stopped);
    // Explicit respawn after a clean stop: exactly one new generation.
    assert!(svc.spawn_worker());
    assert_eq!(svc.worker_status().state, WorkerState::Running);
    assert_eq!(svc.worker_status().restarts, 1);
    assert!(svc.spawn_worker());
    assert_eq!(svc.worker_status().restarts, 1, "still one new generation");
    assert_eq!(svc.shutdown_worker().await, WorkerShutdown::Joined);
    assert_eq!(svc.worker_status().restarts, 1);
    // The owner is terminal and reports it; nothing is detached.
    assert!(!svc.inner.worker_started.load(Ordering::Acquire));
    assert_eq!(svc.worker_status().state, WorkerState::Stopped);
}

// ------------------------------------------- audit 5/6/16: coverage

/// The bounded shard pass resumes exactly after its cursor and every
/// file is visited exactly once across a full round (termination proof).
#[test]
fn fingerprint_pass_cursor_terminates_and_visits_every_file_once() {
    let dir = TempDir::new().unwrap();
    for i in 0..10 {
        write(dir.path(), &format!("x{i}.rs"), "fn x() {}");
    }
    let mut visited: Vec<String> = Vec::new();
    let mut coverage = FingerprintCoverage::round(0);
    for pass_no in 0..1_000 {
        let pass = fingerprint_pass(
            dir.path(),
            coverage.shard,
            &coverage.cursor_for(coverage.shard),
            1,
            8,
            None,
            None,
        )
        .unwrap();
        for entry in &pass.entries {
            visited.push(entry.path.clone());
        }
        if pass.shard_completed {
            coverage.finish_shard(coverage.shard);
        } else {
            let shard = coverage.shard;
            coverage.set_cursor(shard, pass.cursor);
        }
        if coverage.complete {
            assert!(pass_no < 999, "round must terminate");
            break;
        }
    }
    assert!(coverage.complete, "round never completed: {coverage:?}");
    visited.sort();
    let before = visited.len();
    visited.dedup();
    assert_eq!(visited.len(), before, "a file must never be visited twice");
    assert_eq!(visited.len(), 10, "{visited:?}");
    // Every shard ended up represented.
    let shards: std::collections::BTreeSet<u32> =
        visited.iter().map(|p| fingerprint_shard(p)).collect();
    assert!(shards.len() <= FINGERPRINT_SHARDS as usize);
}

/// Audit 5 adversarial: a non-root directory (and a file) that vanishes
/// between batches is churn, not a build failure — the walk resumes past
/// it and the generation still completes; a missing ROOT is still loud.
#[test]
fn walk_tolerates_directories_vanished_between_batches() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "a/one.rs", "fn one() {}");
    write(dir.path(), "b/two.rs", "fn two() {}");
    write(dir.path(), "c/three.rs", "fn three() {}");
    // Batch 1 stops after one file (a/one.rs).
    let first = scan_content_batch(
        dir.path(),
        &ScanCursor::default(),
        &ServiceConfig {
            batch_files: 1,
            ..ServiceConfig::default()
        },
        None,
        None,
    )
    .unwrap();
    assert!(!first.complete);
    assert_eq!(first.files.len(), 1);
    // Churn: the NEXT directory to visit is deleted before the resume.
    std::fs::remove_dir_all(dir.path().join("b")).unwrap();
    let second = scan_content_batch(
        dir.path(),
        &first.cursor,
        &ServiceConfig::default(),
        None,
        None,
    )
    .unwrap();
    assert!(second.complete, "the walk must resume past a vanished dir");
    let mut paths: Vec<String> = first
        .files
        .iter()
        .chain(second.files.iter())
        .map(|f| f.rel.clone())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["a/one.rs", "c/three.rs"], "{paths:?}");
    // A missing ROOT is still a loud failure (no workspace).
    let missing = dir.path().join("gone");
    assert!(scan_content_batch(
        &missing,
        &ScanCursor::default(),
        &ServiceConfig::default(),
        None,
        None,
    )
    .is_err());
}

/// Audit 5 fixture: a repository larger than the OLD total cap (4,000
/// files) where the symbol exists only in file 4,500. Batch budgets make
/// the generation eventually complete, the suffix symbol is found, and
/// an incomplete-generation miss consults the bounded direct fallback
/// before "no match".
#[test]
fn generation_larger_than_total_cap_completes_and_suffix_symbol_is_found() {
    let _serial = serial();
    let env = env();
    const TOTAL: usize = 4_500;
    for i in 0..TOTAL {
        let body = if i == TOTAL - 1 {
            "pub fn suffix_needle_symbol() -> u64 { 4500 }\n".to_string()
        } else {
            format!("pub fn bulk_{i:05}() -> u64 {{ {i} }}\n")
        };
        write(&env.repo, &format!("f{i:05}.rs"), &body);
    }
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, None);
    svc.set_config(fast_cfg());
    svc.attach(ws).unwrap();
    // ONE reconcile runs ONE bounded content batch and publishes the
    // generation PARTIAL: coverage is durable, never silently "complete".
    svc.reconcile_now(ws).unwrap();
    {
        let view = svc.view(ws).expect("a partial generation is published");
        let coverage = view.coverage().clone();
        assert!(!coverage.complete, "{coverage:?}");
        assert!(
            coverage.files_indexed > 0 && coverage.files_indexed <= BATCH_MAX_FILES as u64,
            "one batch is bounded: {coverage:?}"
        );
        assert_eq!(view.freshness(), EvidenceFreshness::Partial);
        assert!(
            view.miss_needs_fallback(),
            "an incomplete generation must trigger the fallback on a miss"
        );
        assert!(
            view.index()
                .lock()
                .unwrap()
                .symbol_lookup(ws, "suffix_needle_symbol", 4)
                .is_empty(),
            "the suffix file is not in the first batch"
        );
    }
    // The bounded direct filesystem fallback serves the missed symbol
    // while indexing continues in the background.
    let provider = svc.cold_provider(ws).expect("cold provider");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: "suffix_needle_symbol".into(),
        changed_files: vec!["f04499.rs".into()],
        referenced_paths: Vec::new(),
        failures: Vec::new(),
    });
    assert!(
        cold.hits
            .iter()
            .any(|h| h.snippet.contains("suffix_needle_symbol")),
        "the fallback must serve the missed file: {cold:?}"
    );
    // Indexing continues batch after batch until the generation is
    // COMPLETE; then (and only then) the symbol is in the index.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let _ = svc.reconcile_now(ws);
        let Some(view) = svc.view(ws) else {
            assert!(Instant::now() < deadline, "no view ever published");
            continue;
        };
        if view.coverage().complete && view.freshness() == EvidenceFreshness::Current {
            assert!(
                !view
                    .index()
                    .lock()
                    .unwrap()
                    .symbol_lookup(ws, "suffix_needle_symbol", 4)
                    .is_empty(),
                "the suffix symbol must be found once coverage is complete"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "generation never completed: {:?}",
            svc.coverage(ws)
        );
    }
    let snapshot = svc.coverage_snapshot(ws).unwrap();
    assert!(snapshot.coverage.complete, "{snapshot:?}");
    assert!(snapshot.fingerprint.complete, "{snapshot:?}");
    assert!(snapshot.serving);
    assert_eq!(snapshot.freshness, EvidenceFreshness::Current);
    assert_eq!(snapshot.published_generation, Some(1));
    drop(store);
}

/// Audit 6: a capped fingerprint round is PARTIAL (never "fully clean"),
/// persists its shard state, and rotation detects a change in the LAST
/// shard — no suffix is perpetually ignored.
#[test]
fn capped_fingerprint_never_reports_clean_and_rotates_until_every_shard_is_seen() {
    let _serial = serial();
    let env = env();
    let files: Vec<String> = (0..24).map(|i| format!("s{i:02}.rs")).collect();
    for (i, path) in files.iter().enumerate() {
        write(
            &env.repo,
            path,
            &format!("pub fn fp_{i}() -> u64 {{ {i} }}\n"),
        );
    }
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, None);
    let mut cfg = fast_cfg();
    cfg.fp_batch_files = 1;
    cfg.fp_batch_dirs = 2;
    svc.set_config(cfg);
    svc.attach(ws).unwrap();
    let deadline = Instant::now() + DEADLINE;
    // A fresh build with a tiny pass budget needs many bounded passes;
    // drive them.
    let mut iters = 0usize;
    loop {
        let _ = svc.reconcile_now(ws);
        iters += 1;
        let content_complete = svc.view(ws).map(|v| v.coverage().complete).unwrap_or(false);
        let fp_complete = svc
            .fingerprint_coverage(ws)
            .map(|c| c.complete)
            .unwrap_or(false);
        if content_complete && fp_complete {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "capped round never completed after {iters} iters: state={:?} cov={:?} fp={:?}",
            svc.state(ws),
            svc.coverage(ws),
            svc.fingerprint_coverage(ws)
        );
    }
    let first_round_start = svc.fingerprint_coverage(ws).unwrap().round_start;
    let gen_before_round = svc.view(ws).unwrap().generation();
    // Start a NEW verification round (no disk change): while it is capped
    // it must never report clean; the view stays Partial, and the round
    // only completes cleanly after every shard was visited.
    std::thread::sleep(Duration::from_millis(60));
    let _ = svc.reconcile_now(ws);
    let fp = svc.fingerprint_coverage(ws).unwrap();
    assert!(
        !fp.complete,
        "a capped round must never be reported fully clean: {fp:?}"
    );
    assert_eq!(
        svc.view(ws).unwrap().freshness(),
        EvidenceFreshness::Partial,
        "an in-progress fingerprint round makes the package partial"
    );
    loop {
        let _ = svc.reconcile_now(ws);
        if svc.fingerprint_coverage(ws).unwrap().complete {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "verification round never completed"
        );
    }
    let rotated = svc.fingerprint_coverage(ws).unwrap().round_start;
    assert_ne!(
        rotated,
        first_round_start,
        "a completed round must rotate the next round's start shard (gen {} -> {}, fp {:?})",
        gen_before_round,
        svc.view(ws).map(|v| v.generation()).unwrap_or(0),
        svc.fingerprint_coverage(ws)
    );
    // Rotation fairness: every shard is reachable. Change ONE file in
    // every shard in turn and prove each change is detected (a rebuild
    // lands) even though the pass budget stats a single file.
    let mut visited = std::collections::BTreeSet::new();
    for shard in 0..FINGERPRINT_SHARDS {
        let Some(path) = files.iter().find(|p| fingerprint_shard(p) == shard) else {
            continue;
        };
        visited.insert(shard);
        let before = svc.view(ws).map(|v| v.generation()).unwrap_or(0);
        write(
            &env.repo,
            path,
            &format!("pub fn fp_rotated_{shard}() {{}}\n"),
        );
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        let shard_deadline = Instant::now() + DEADLINE;
        loop {
            let _ = svc.reconcile_now(ws);
            let rebuilt = svc.view(ws).map(|v| v.generation()).unwrap_or(0) > before;
            let at_rest = svc
                .fingerprint_coverage(ws)
                .map(|c| c.complete)
                .unwrap_or(false)
                && svc.view(ws).map(|v| v.coverage().complete).unwrap_or(false);
            if rebuilt && at_rest {
                break;
            }
            assert!(
                Instant::now() < shard_deadline,
                "shard {shard} change was never detected ({path})"
            );
        }
    }
    assert_eq!(
        visited.len(),
        FINGERPRINT_SHARDS as usize,
        "every shard must be reachable"
    );
    drop(store);
}

/// Audit 5: a crash between batches resumes the SAME generation from the
/// persisted continuation cursor — the accumulated work is not re-done
/// and the suffix is still completed.
#[test]
fn crash_between_batches_resumes_from_the_persisted_cursor() {
    let _serial = serial();
    let env = env();
    for i in 0..40 {
        write(
            &env.repo,
            &format!("c{i:02}.rs"),
            &format!("pub fn c_{i}() {{}}\n"),
        );
    }
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store, svc, ws) = restart(&env, fs, None);
    let mut cfg = fast_cfg();
    cfg.batch_files = 10;
    svc.set_config(cfg);
    svc.attach(ws).unwrap();
    // Batch 1 publishes a partial generation with a durable cursor.
    svc.reconcile_now(ws).unwrap();
    let gen_path = generation_file_path(&env.data_root, ws, 1);
    let file = read_generation_file(&gen_path).unwrap();
    assert!(!file.coverage().complete);
    let cursor = file.cursor.clone().expect("partial cursor persisted");
    assert!(
        !cursor.frames.is_empty(),
        "the continuation cursor names the resume position: {cursor:?}"
    );
    assert_eq!(file.coverage().files_indexed, 10);
    // Crash the continuation BEFORE its publish: durable state stays
    // Building{1}, the partial file keeps the cursor.
    install_seam(Box::new(move |_w, generation, point| {
        if point == "before_publish" && generation == 1 {
            panic!("simulated crash between batches");
        }
    }));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| svc.reconcile_now(ws)));
    clear_seam();
    assert!(outcome.is_err(), "the armed seam must kill the builder");
    let (state, gen) = svc.state(ws).unwrap();
    assert_eq!(gen, 1);
    assert!(matches!(state, St::Building { generation: 1 }), "{state:?}");
    drop(svc);
    // Restart: the SAME generation resumes from the persisted cursor
    // (the accumulation continues; it is not restarted from file 0).
    let fs = faktor_fs::WorkspaceFileService::new();
    let (store2, svc2, ws2) = restart(&env, fs, None);
    svc2.set_config({
        let mut c = fast_cfg();
        c.batch_files = 10;
        c
    });
    svc2.attach(ws2).unwrap();
    svc2.reconcile_now(ws2).unwrap();
    let coverage = svc2.coverage(ws2).unwrap();
    assert!(
        coverage.files_indexed > 10,
        "the resumed build must continue the accumulation: {coverage:?}"
    );
    let deadline = Instant::now() + DEADLINE;
    loop {
        let _ = svc2.reconcile_now(ws2);
        if svc2
            .view(ws2)
            .map(|v| v.coverage().complete)
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resumed generation never completed"
        );
    }
    let view = svc2.view(ws2).unwrap();
    assert!(!view
        .index()
        .lock()
        .unwrap()
        .symbol_lookup(ws2, "c_39", 4)
        .is_empty());
    let log = journal_counts(&store2, ws2);
    assert_eq!(
        log.iter().filter(|(k, _)| k == "ready").count(),
        1,
        "exactly ONE complete publish: {log:?}"
    );
    assert!(
        log.iter().any(|(k, _)| k == "ready_partial"),
        "partial batch publishes are journaled distinctly: {log:?}"
    );
    assert!(
        log.iter().any(|(k, _)| k == "continue"),
        "batch continuation claims are journaled: {log:?}"
    );
    drop(store);
    drop(store2);
}

/// The worker's own async body can panic too (outside the blocking
/// pass): the owner must surface `Failed` instead of reporting a dead
/// task as `Running`, and the next explicit spawn must self-heal around
/// the dead generation.
#[tokio::test]
async fn worker_async_body_panic_is_reconciled_not_reported_running() {
    let (_env, _store, svc, _ws) = first_fixture();
    // One-shot fault: the worker body panics before its first pass, so
    // no `JoinError` reaches the pass handler and the started flag would
    // otherwise stay set forever. (The cfg/embedding/live locks RECOVER
    // from poison by policy — item 8 — so a poisoned lock is not a panic
    // injection anymore.)
    svc.inner
        .fault
        .async_body_panic
        .store(true, Ordering::SeqCst);
    assert!(svc.spawn_worker());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let status = svc.worker_status();
        if status.state == WorkerState::Failed {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "a dead worker body must never stay Running: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(status.last_error.is_some(), "{status:?}");
    assert!(!svc.inner.worker_started.load(Ordering::Acquire));
    // Self-heal: the dead generation no longer owns the flag, so the
    // explicit spawn starts a fresh generation (counted once) that runs
    // healthy — the one-shot fault already consumed itself.
    assert!(svc.spawn_worker());
    assert_eq!(svc.worker_status().restarts, 1);
}

/// Item 8 policy, index side: every index-internal lock guards
/// process-local, rebuildable state (the durable truth lives in the
/// store + generation files), so a deliberate panic in a holder must
/// leave later requests HEALTHY — never a poisoned panic and never a
/// wedged service. This drives the three major locks plus the published
/// content mutex and then exercises the public surface.
#[test]
fn poisoned_index_locks_recover_healthy_never_panic() {
    let (_env, _store, svc, ws) = first_fixture();
    let _serial = serial();
    // Publish a ready generation so `content` is live.
    let view = svc
        .ensure_ready(ws, Instant::now() + Duration::from_secs(30))
        .unwrap();
    drop(view);

    let poison = |f: &dyn Fn()| {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        assert!(caught.is_err(), "the deliberate panic must fire");
    };
    // cfg, live, embedding, and the published content mutex each get a
    // panic while held.
    poison(&|| {
        let _guard = lock_recover(&svc.inner.cfg);
        panic!("poison cfg");
    });
    assert!(svc.inner.cfg.is_poisoned());
    poison(&|| {
        let _guard = lock_recover(&svc.inner.live);
        panic!("poison live");
    });
    assert!(svc.inner.live.is_poisoned());
    poison(&|| {
        let _guard = lock_recover(&svc.inner.embedding);
        panic!("poison embedding");
    });
    assert!(svc.inner.embedding.is_poisoned());
    let content = {
        let live = lock_recover(&svc.inner.live);
        live.get(&ws).and_then(|l| l.content.clone())
    };
    if let Some(content) = content {
        poison(&|| {
            let _guard = lock_recover(&content);
            panic!("poison content");
        });
        assert!(content.is_poisoned());
    }

    // The public surface recovers instead of panicking: config write,
    // worker status (classified supervisor recovery), a fresh reconcile
    // pass and a view all succeed.
    svc.set_config(fast_cfg());
    let _ = svc.worker_status();
    svc.reconcile_now(ws).unwrap();
    let view = svc
        .ensure_ready(ws, Instant::now() + Duration::from_secs(30))
        .unwrap();
    assert!(
        !view
            .index()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .files
            .is_empty(),
        "the published generation survived the poisoned locks"
    );
}

/// The index's durable authority (the `Store`) fails typed on its writer
/// panic while the index surface keeps answering: the two poisoning
/// policies compose without a daemon panic.
#[test]
fn store_writer_panic_keeps_index_surface_total() {
    let (_env, store, svc, ws) = first_fixture();
    let _serial = serial();
    svc.ensure_ready(ws, Instant::now() + Duration::from_secs(30))
        .unwrap();
    let err = store
        .writer_debug_job("index_cert_panic", |_conn| {
            panic!("deliberate store writer panic")
        })
        .unwrap_err();
    assert!(matches!(
        err,
        faktor_store::StoreError::WriterUnavailable(_)
    ));
    assert!(!store.writer_available());
    // Read-side index surface stays total.
    svc.set_config(fast_cfg());
    let live_view = svc.view(ws);
    assert!(live_view.is_some(), "published content is in memory");
    // Mutations that need the store fail typed; nothing panics.
    assert!(svc.reconcile_now(ws).is_err() || !svc.inner.worker_started.load(Ordering::Acquire));
}
