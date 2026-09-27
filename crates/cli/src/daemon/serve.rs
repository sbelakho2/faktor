//! `daemon::serve`: cohesive slice of the daemon construction.

use super::*;

/// Bounded graceful window of the daemon's detached orchestrated drives at
/// shutdown. The registry then aborts and reaps stragglers within its own
/// [`faktor_orchestrator::runtime::task_executor::TaskDriveRegistry::ABORT_REAP_GRACE`],
/// so the WHOLE drive drain stays bounded by `grace + ABORT_REAP_GRACE` —
/// never unbounded, and never a dropped durable write (an aborted drive is
/// crash-equivalent and resumable from its durable rows).
pub(crate) const SERVE_DRIVE_SHUTDOWN_GRACE: std::time::Duration =
    std::time::Duration::from_secs(5);

/// The ONE post-signal daemon shutdown sequence (serve):
///
/// 0. stop the native listener (when its handle is passed): the handle OWNS
///    the serve task, so this is the bounded graceful join
///    ([`faktor_server::api::ServerHandle::shutdown`]) — never a detached
///    future; an unexpected death is logged with its typed status BEFORE the
///    stop, so it is never masked as a requested one;
/// 1. stop the worker plane (when enabled): the slot hands back the owned
///    `WorkerPlaneHandle`, which requests graceful shutdown and JOINS its
///    serve task within
///    [`faktor_server::worker_plane::WORKER_PLANE_SHUTDOWN_BOUND`] (a
///    straggler is aborted), so no new remote worker request lands while
///    the drives drain and no listener task is ever detached. An unexpected
///    death is logged with its typed code (the same status health and any
///    placement decision query) and the slot records the typed terminal
///    state;
/// 2. stop the graph-hosted repository index reconciliation worker (when
///    hosted): `IndexService::shutdown_worker` cancels and JOINS the owned
///    worker within its own bound (a straggler is aborted and awaited), so
///    no background index pass outlives the daemon or races the drain;
/// 3. stop the runtime's OWN lazily-hosted repository index worker (when the
///    runtime ever opened one): `AgentRuntime::shutdown_index_service`
///    cancels and JOINS the owned worker within the same service bound and
///    reports the outcome typed. A runtime that never hosted one is an inert
///    no-op — the accessor never opens the service as a side effect. This is
///    non-fatal by contract: a typed abort/join failure is logged, never
///    propagated (the durable rows stay the recovery authority);
/// 4. abort the post-ready loops (verification executor, SCM re-sync, billing
///    report schedule) — none of them is a durable-write producer whose
///    in-flight work is lost, and each re-runs its durable claims at the next
///    boot;
/// 5. close the TaskExecutor's drive registry and bounded-drain every
///    detached drive (the registry's graceful-then-abort contract: give
///    in-flight drives `grace` to land their record-first durable
///    writes/settlement, then abort and reap the stragglers within
///    `ABORT_REAP_GRACE`; an aborted run stays resumable through
///    `TaskExecutor::resume_run`). This MUST happen while the store and the
///    shadow service are still alive, so it runs here — before the backup
///    drain and before `graph` drops;
/// 6. drain the startup-backup task and the owned Ollama warm-up threads
///    (bounded), AFTER the drives so the final snapshot observes every
///    settled durable write.
///
/// The returned [`DriveDrainReport`](faktor_orchestrator::runtime::task_executor::DriveDrainReport)
/// is the testable evidence of the drive drain.
// Eight explicit owners/tasks is the sequence's whole shape; bundling them
// into a struct would not make the shutdown contract clearer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn shutdown_serving_daemon(
    tasks: &Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
    server: Option<faktor_server::api::ServerHandle>,
    worker_plane: Option<faktor_server::api::WorkerPlaneListener>,
    index: Option<Arc<faktor_index::IndexService>>,
    agent: Option<&Arc<AgentRuntime>>,
    verification_executor: tokio::task::JoinHandle<()>,
    scm_task: Option<tokio::task::JoinHandle<()>>,
    billing_report_task: Option<tokio::task::JoinHandle<()>>,
    backup_task: tokio::task::JoinHandle<()>,
) -> faktor_orchestrator::runtime::task_executor::DriveDrainReport {
    // Stop the native listener first (the primary intake): the handle OWNS
    // the serve task, so this is a bounded join, never a detached future.
    // Health is queried BEFORE the request so an unexpected death is named,
    // not masked by the stop (the same contract the worker plane follows
    // below).
    if let Some(server) = server {
        let health = server.status();
        if health.is_unavailable() {
            tracing::error!("{}", health.health_line());
        }
        match server.shutdown().await {
            Ok(()) => tracing::info!("native server: stopped (bounded graceful join)"),
            Err(error) => tracing::error!(
                code = error.code(),
                "native server: shutdown failed: {error}"
            ),
        }
    }
    // Stop the remote intake first: the slot owns the serve task, so this
    // is a bounded join, never a detached listener. Health is queried BEFORE
    // the request so an unexpected death is named, not masked by the stop;
    // the slot then records the typed terminal state for any later health
    // read.
    if let Some(listener) = worker_plane {
        let taken = listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_for_shutdown();
        if let Some((handle, health)) = taken {
            if health.is_unavailable() {
                tracing::error!("{}", health.health_line());
            }
            let terminal = match handle.shutdown().await {
                Ok(()) => {
                    tracing::info!("worker plane: stopped (bounded graceful join)");
                    faktor_server::worker_plane::WorkerPlaneStatus::Stopped
                }
                Err(error) => {
                    tracing::error!(
                        code = error.code(),
                        "worker plane: shutdown failed: {error}"
                    );
                    faktor_server::worker_plane::WorkerPlaneStatus::Unavailable {
                        code: error.code(),
                        message: error.to_string(),
                    }
                }
            };
            listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record_terminal(terminal);
        }
    }
    // Stop the repository index reconciliation worker (bounded join, before
    // the drive drain): a background pass must not outlive the daemon or
    // race the shutdown. `NotRunning` is the honest no-op when the worker
    // was never started (or already stopped).
    if let Some(index) = index {
        let started = std::time::Instant::now();
        match index.shutdown_worker().await {
            faktor_index::WorkerShutdown::NotRunning => {}
            faktor_index::WorkerShutdown::Joined => tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "index reconciliation worker: stopped (bounded graceful join)"
            ),
            faktor_index::WorkerShutdown::Aborted => {
                // F8: the async task was aborted, but a blocking pass cannot
                // be killed — report the TYPED residual so the operator knows
                // the real exit bound is the pass's remaining duration.
                let pass_in_flight = index.worker_status().pass_in_flight;
                tracing::error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    pass_in_flight,
                    "index reconciliation worker: did not stop within the bound; aborted and reaped{}",
                    if pass_in_flight {
                        " — a blocking pass is STILL RUNNING and sets the real exit bound"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    // Stop the runtime's OWN lazily-hosted index worker (adjacent to the
    // graph-hosted one, before the drive drain): the runtime has no async
    // teardown, so this accessor IS its join point. `None` = the runtime
    // never hosted a service (never opened); the call never opens one.
    // Non-fatal: an abort/join failure is logged typed, the shutdown
    // continues, and the durable rows remain the recovery authority.
    if let Some(agent) = agent {
        let started = std::time::Instant::now();
        match agent.shutdown_index_service().await {
            None => {}
            Some(faktor_index::WorkerShutdown::NotRunning) => {}
            Some(faktor_index::WorkerShutdown::Joined) => tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "runtime index worker: stopped (bounded graceful join)"
            ),
            Some(faktor_index::WorkerShutdown::Aborted) => {
                let pass_in_flight = agent
                    .index_service_worker_status()
                    .is_some_and(|status| status.pass_in_flight);
                tracing::error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    pass_in_flight,
                    "runtime index worker: did not stop within the bound; aborted and reaped{}",
                    if pass_in_flight {
                        " — a blocking pass is STILL RUNNING and sets the real exit bound"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    verification_executor.abort();
    if let Some(task) = scm_task {
        task.abort();
    }
    if let Some(task) = billing_report_task {
        task.abort();
    }
    let started = std::time::Instant::now();
    let report = tasks.shutdown_drives(SERVE_DRIVE_SHUTDOWN_GRACE).await;
    if report.total > 0 || report.unreaped > 0 {
        tracing::info!(
            total = report.total,
            completed = report.completed,
            aborted = report.aborted,
            unreaped = report.unreaped,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "orchestrated drive shutdown drain"
        );
    }
    if report.unreaped > 0 {
        tracing::error!(
            unreaped = report.unreaped,
            "drive shutdown could not reap every aborted drive; the durable rows remain the recovery authority"
        );
    }
    drain_startup_backup(backup_task).await;
    report
}

/// Shutdown drain for the startup-backup task: waits a bounded window for
/// the backup to finish (the daemon announced readiness long ago, so the
/// snapshot is normally long done), then aborts the async wrapper. The
/// abort cuts the sleep/gate, NOT the blocking snapshot (spawn_blocking
/// closures cannot be force-killed; the snapshot is bounded and finishes at
/// its own pace) — the daemon never waits unboundedly on shutdown.
pub(crate) async fn drain_startup_backup(mut backup_task: tokio::task::JoinHandle<()>) {
    const BACKUP_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
    // `&mut JoinHandle` is a Future: the handle stays borrowable so the
    // timeout path can still abort the async wrapper.
    match tokio::time::timeout(BACKUP_DRAIN_GRACE, &mut backup_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("startup backup task panicked: {e}"),
        Err(_) => {
            tracing::warn!("startup backup drain timed out; aborting the async wrapper");
            backup_task.abort();
        }
    }
    // The SAME bounded shutdown drain retires the owned Ollama warm-up
    // threads (audited spawn ownership): bounded join, deterministic runtime
    // stop inside each thread, never a leaked handle or unbounded wait.
    let (joined, detached) = drain_ollama_warmups(OLLAMA_WARMUP_DRAIN_GRACE);
    if joined + detached > 0 {
        tracing::info!(joined, detached, "ollama warm-up shutdown drain");
    }
}

/// The process-wide owner of every detached Ollama warm-up thread.
///
/// Audited gap: `warm_ollama` used to `std::thread::spawn` a thread that
/// owned a private Tokio runtime and DROP its `JoinHandle` — the thread (and
/// its runtime) leaked until process exit and no shutdown path could bound
/// it. Each warm-up is now an [`OllamaWarmup`] owner held in this registry:
/// the thread signals completion through a channel, stops its runtime
/// deterministically (`Runtime::shutdown_timeout`) before it exits, and the
/// shutdown drain joins it with a bounded timeout.
pub(crate) static OLLAMA_WARMUPS: std::sync::OnceLock<std::sync::Mutex<Vec<OllamaWarmup>>> =
    std::sync::OnceLock::new();

/// Bounded stop of the warm-up's private runtime after the probe returned:
/// a probe task that somehow lingers cannot outlive its owner's thread (the
/// blocking runtime shutdown cancels what it can and joins the blocking
/// pool within the grace).
pub(crate) const OLLAMA_WARMUP_RUNTIME_STOP_GRACE: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Bounded shutdown join window of the warm-up THREADS (never unbounded).
pub(crate) const OLLAMA_WARMUP_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// One owned Ollama warm-up thread: the retained `JoinHandle` plus the
/// completion signal of its body. `join_with_timeout` is the ONLY way to
/// retire it — a dropped owner would recreate the old leak.
pub(crate) struct OllamaWarmup {
    pub(crate) handle: Option<std::thread::JoinHandle<()>>,
    pub(crate) finished: Option<std::sync::mpsc::Receiver<()>>,
}

impl OllamaWarmup {
    /// Bounded join: `true` when the body finished (and its runtime was
    /// stopped) within `timeout`. A panicked body is joined too (its
    /// completion signal is disconnected, not missing). On timeout the
    /// thread is left detached — a thread cannot be force-killed — but it is
    /// a best-effort probe whose own runtime shutdown is already bounded,
    /// and the daemon never waits unboundedly on shutdown.
    pub(crate) fn join_with_timeout(self, timeout: std::time::Duration) -> bool {
        let (Some(handle), Some(finished)) = (self.handle, self.finished) else {
            return true;
        };
        match finished.recv_timeout(timeout) {
            Ok(()) => {
                let _ = handle.join();
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if handle.join().is_err() {
                    tracing::warn!("ollama warm-up thread panicked (joined)");
                }
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        }
    }
}

/// Spawn one named warm-up thread whose body signals completion on exit
/// (including panic: the wrapper's sender is owned by the thread stack).
/// `None` when the OS refused the thread (the warm-up is skipped, never a
/// half-owned handle).
pub(crate) fn spawn_ollama_warmup_thread(
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> Option<OllamaWarmup> {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    match std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            body();
            let _ = done_tx.send(());
        }) {
        Ok(handle) => Some(OllamaWarmup {
            handle: Some(handle),
            finished: Some(done_rx),
        }),
        Err(e) => {
            tracing::warn!("ollama warm-up thread failed to spawn: {e}");
            None
        }
    }
}

/// Register one owned warm-up in the process registry (the SAME path
/// `warm_ollama` uses; tests register seam bodies through it).
pub(crate) fn register_ollama_warmup(owner: OllamaWarmup) {
    let registry = OLLAMA_WARMUPS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(owner);
}

/// Bounded join of an explicit owner set: `(joined, detached)` counts. A
/// detached owner is logged (its own runtime stop is already bounded).
pub(crate) fn drain_warmup_owners(
    owners: Vec<OllamaWarmup>,
    timeout: std::time::Duration,
) -> (usize, usize) {
    let deadline = std::time::Instant::now() + timeout;
    let mut joined = 0usize;
    let mut detached = 0usize;
    for owner in owners {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if owner.join_with_timeout(remaining) {
            joined += 1;
        } else {
            detached += 1;
            tracing::warn!(
                "ollama warm-up thread missed the shutdown join window; it stays detached and exits with its own bounded runtime stop"
            );
        }
    }
    (joined, detached)
}

/// Shutdown drain of every registered warm-up: takes the registry (idempotent
/// — a later call finds none) and joins each owner within `timeout` in total.
/// The daemon's shutdown path reaches this through `drain_startup_backup`,
/// the one bounded post-ready task drain.
pub(crate) fn drain_ollama_warmups(timeout: std::time::Duration) -> (usize, usize) {
    let owners: Vec<OllamaWarmup> = match OLLAMA_WARMUPS.get() {
        Some(registry) => std::mem::take(
            &mut *registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ),
        None => Vec::new(),
    };
    drain_warmup_owners(owners, timeout)
}

/// Live capability warm-up for one Ollama provider (spec §10): the
/// concrete Arc is owned by the spawned thread, so probing reaches the
/// SAME instance the registry serves. Best-effort, never blocks; the thread
/// is OWNED by the process registry and bounded-joined at shutdown
/// ([`drain_ollama_warmups`]).
pub(crate) fn warm_ollama(ollama: Arc<faktor_ollama::OllamaProvider>) {
    let Some(owner) = spawn_ollama_warmup_thread("faktor-ollama-warmup", move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!("ollama warm-up runtime failed: {e}");
                return;
            }
        };
        match rt.block_on(ollama.refresh_from_live()) {
            Ok(n) => {
                tracing::info!("ollama: probed {n} model(s) from live discovery");
            }
            Err(e) => {
                tracing::warn!("ollama warm-up failed (defaults stay): {e}");
            }
        }
        // Deterministic runtime stop: no probe task outlives its thread.
        rt.shutdown_timeout(OLLAMA_WARMUP_RUNTIME_STOP_GRACE);
    }) else {
        return;
    };
    register_ollama_warmup(owner);
}

/// Adversarial covers of the audited Ollama warm-up ownership gap: the
/// handle is retained, the bounded join is honored, and the shutdown drain
/// empties the process registry (idempotently) instead of leaking threads.
#[cfg(test)]
mod ollama_warmup_tests {
    use super::*;

    /// The global registry is process-wide: tests that touch it serialize.
    static GLOBAL_WARMUP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn bounded_join_reports_finished_owners_and_detaches_stragglers() {
        let fast = spawn_ollama_warmup_thread("faktor-test-warmup-fast", || {}).unwrap();
        assert!(fast.join_with_timeout(std::time::Duration::from_secs(5)));
        // A panicked body is finished (its sender disconnected), not a
        // straggler: the owner is joined, never reported as detached.
        let panicked = spawn_ollama_warmup_thread("faktor-test-warmup-panic", || {
            panic!("warm-up body panic (test)");
        })
        .unwrap();
        assert!(panicked.join_with_timeout(std::time::Duration::from_secs(5)));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let slow = spawn_ollama_warmup_thread("faktor-test-warmup-slow", move || {
            let _ = release_rx.recv();
        })
        .unwrap();
        let started = std::time::Instant::now();
        assert!(!slow.join_with_timeout(std::time::Duration::from_millis(25)));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "bounded join never waits unboundedly"
        );
        // Let the detached thread exit so the test binary holds no runner.
        let _ = release_tx.send(());
    }

    #[test]
    fn shutdown_drain_empties_the_process_registry_within_bound() {
        let _serial = GLOBAL_WARMUP_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        register_ollama_warmup(
            spawn_ollama_warmup_thread("faktor-test-warmup-registered", || {}).unwrap(),
        );
        let started = std::time::Instant::now();
        let (joined, detached) = drain_ollama_warmups(std::time::Duration::from_secs(5));
        assert_eq!(joined, 1);
        assert_eq!(detached, 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // Idempotent: the registry was TAKEN, nothing is left to leak.
        let (joined, detached) = drain_ollama_warmups(std::time::Duration::from_millis(1));
        assert_eq!((joined, detached), (0, 0));
    }

    #[test]
    fn shutdown_drain_never_exceeds_the_bound_for_a_parked_thread() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let parked = spawn_ollama_warmup_thread("faktor-test-warmup-parked", move || {
            let _ = release_rx.recv();
        })
        .unwrap();
        let started = std::time::Instant::now();
        let (joined, detached) =
            drain_warmup_owners(vec![parked], std::time::Duration::from_millis(30));
        assert_eq!((joined, detached), (0, 1));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the drain stayed bounded"
        );
        let _ = release_tx.send(());
    }
}

/// Config resolution for `serve` (audit 31): an EXPLICIT --config path must
/// load STRICTLY (parse + semantic validation) — any failure is an Err the
/// caller turns into a startup error (exit 1); the daemon never boots on a
/// config it cannot fully honor. Without --config, defaults + best-effort
/// discovery stay lenient and nothing here can fail startup.
/// Test-facing convenience over [`serve_config_and_semantic`].
#[cfg(test)]
pub(crate) fn serve_config(config_path: Option<PathBuf>) -> Result<config::Config, String> {
    Ok(serve_config_and_semantic(config_path)?.0)
}

/// The strict daemon config PLUS the additive `[semantic]` section (audits
/// 48-54/58/79). The section is extracted from the raw document BEFORE the
/// frozen `Config` shape parses the rest, so it is strictly additive with an
/// empty default: absent/null keeps [`graph::SemanticCfg::default`] and a
/// present section is parsed with `deny_unknown_fields` (a typo'd key fails
/// startup, never silently changes behavior). Every other key keeps exactly
/// the strict load semantics (`Config` parse + `validate`).
pub(crate) fn serve_config_and_semantic(
    config_path: Option<PathBuf>,
) -> Result<(config::Config, graph::SemanticCfg), String> {
    let Some(path) = config_path else {
        return Ok((config::Config::default(), graph::SemanticCfg::default()));
    };
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("config {}: {e}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("config {}: {e}", path.display()))?;
    let semantic = match value
        .as_object_mut()
        .and_then(|object| object.remove("semantic"))
    {
        None | Some(serde_json::Value::Null) => graph::SemanticCfg::default(),
        Some(section) => serde_json::from_value(section)
            .map_err(|e| format!("config {} [semantic]: {e}", path.display()))?,
    };
    let config: config::Config =
        serde_json::from_value(value).map_err(|e| format!("config {}: {e}", path.display()))?;
    config
        .validate()
        .map_err(|e| format!("config {}: {e}", path.display()))?;
    // Strict provider-section validation (bounded caps, unique ids, valid
    // command/args/endpoint/timeout/auth-env for every external provider):
    // a hostile section refuses startup here, before any graph authority or
    // child process exists.
    semantic
        .validate()
        .map_err(|e| format!("config {} [semantic]: {e}", path.display()))?;
    Ok((config, semantic))
}

/// The interactive-terminal execution-authority policy for the configured
/// `[sandbox] shell` setting: the ONE construction the daemon entry points
/// (`serve`, `acp`) inject into the terminal authority AT CONSTRUCTION and
/// the doctor reads back. Interactive session terminals are user-initiated
/// and form their OWN trust class, distinct from the agent shell tool:
/// an UNSET `shell` selects the honest user-granted default
/// (`network_capable_user_granted`, no isolation claim, `inherit`), an
/// explicit `shell = "os_isolated"` demands OS isolation for terminals too
/// (typed refusal where no backend exists; Linux keeps the netns path), and
/// an explicit grant carries the configured non-required guarantee. The
/// AGENT class is resolved separately by `Config::sandbox_policy` (always
/// the secure `os_isolated`/`required` default when unset) and can never
/// inherit the terminal grant. The pairing is validated at this boundary —
/// the same refusal the daemon startup applies — never half-honored.
pub(crate) fn terminal_authority_policy(
    config: &config::Config,
) -> Result<faktor_server::native::terminal_authority::TerminalAuthorityPolicy, String> {
    // Boundary validation: an invalid shell/guarantee pairing or an
    // unparseable destination rule is refused here exactly as the daemon
    // build refuses it.
    config
        .sandbox_policy()
        .map_err(|e| format!("sandbox config: {e}"))?;
    Ok(
        faktor_server::native::terminal_authority::TerminalAuthorityPolicy::for_interactive_session_terminals(
            config.sandbox.shell,
            config.sandbox.network_guarantee,
        ),
    )
}

/// Forced-exit code of the SECOND shutdown signal during the drain (`128 +
/// SIGINT`, the conventional interrupted exit; SIGTERM maps to the same
/// bounded force-exit since the daemon's own drain is the graceful path).
pub(crate) const FORCE_EXIT_CODE: i32 = 130;

/// Test-only probe of the force-exit seam: a test cannot let the harness
/// process exit, so an installed probe receives the code instead. Production
/// has no probe installed and `std::process::exit` runs.
#[cfg(test)]
pub(crate) static FORCE_EXIT_PROBE: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<i32>>> =
    std::sync::Mutex::new(None);

/// Test-only marker: the FIRST shutdown signal was observed and the drain
/// (plus the second-signal watchdog) is being entered. Lets the signal tests
/// synchronize deterministically instead of sleeping.
#[cfg(test)]
pub(crate) static SIGNAL_DRAIN_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only count of armed signal waiters (handler registrations that
/// succeeded). A test delivers a real signal only once the waiter it targets
/// is armed, so the process can never be killed by an unhandled default.
#[cfg(test)]
pub(crate) static SIGNAL_WAITERS_ARMED: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Await the next process shutdown signal (SIGTERM or SIGINT). Registration
/// is process-wide; a registration failure is loud and falls back to
/// `ctrl_c`, never a panic. The returned label is the signal's name for
/// structured logs.
pub(crate) async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                #[cfg(test)]
                SIGNAL_WAITERS_ARMED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::select! {
                    _ = term.recv() => "SIGTERM",
                    _ = int.recv() => "SIGINT",
                }
            }
            (term, int) => {
                let err = term.err().or(int.err());
                tracing::error!(
                    error = %err.map(|e| e.to_string()).unwrap_or_default(),
                    "shutdown signal handlers could not be installed; falling back to ctrl_c"
                );
                let _ = tokio::signal::ctrl_c().await;
                "ctrl_c"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl_c"
    }
}

/// The second-signal force path: the drain is bounded, but an operator who
/// sends a second SIGTERM/SIGINT asks to stop NOW. The code is reported to
/// the test probe when one is installed; production exits immediately.
pub(crate) async fn on_force_signal(signal: &str) {
    tracing::error!(
        signal,
        code = FORCE_EXIT_CODE,
        "second shutdown signal during the drain; forcing exit"
    );
    #[cfg(test)]
    if let Some(probe) = FORCE_EXIT_PROBE.lock().unwrap().take() {
        let _ = probe.send(FORCE_EXIT_CODE);
        return;
    }
    std::process::exit(FORCE_EXIT_CODE);
}

/// Arm the second-signal watchdog. Owned by the daemon's shutdown path: it
/// lives only while the bounded drain runs and either fires (force exit) or
/// is dropped when the process ends normally.
pub(crate) fn spawn_force_exit_watchdog() {
    tokio::spawn(async move {
        let signal = wait_for_shutdown_signal().await;
        on_force_signal(signal).await;
    });
}

pub(crate) async fn serve(port: u16, data_dir: PathBuf, config_path: Option<PathBuf>) {
    if let Err(e) = serve_impl(port, data_dir, config_path, None, None).await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

/// The frozen CHILD digest-attestation line the release bootstrap launcher
/// requires from a non-unix launch before it reports readiness:
/// `faktor release digest=<64 lowercase hex>`. The launcher's parser
/// (`faktor_updater::release::CHILD_DIGEST_PREFIX`) mirrors this prefix
/// because the server cannot depend on the updater's private constants; the
/// two strings must stay byte-identical.
pub(crate) const RELEASE_DIGEST_ATTESTATION_PREFIX: &str = "faktor release digest=";

/// The typed outcome of one startup release-digest attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseAttestation {
    /// The bootstrap launcher exported no expected digest (`FAKTOR_RELEASE_DIGEST`
    /// unset, the unix/direct-start case): nothing was printed.
    NotRequested,
    /// Exactly one attestation line carrying the RUNNING binary's digest was
    /// printed. `matches_expected` is false when the launcher's export differs
    /// — still the ACTUAL digest (never a lie); the launcher then refuses.
    Printed { matches_expected: bool },
    /// No attestation is possible (malformed export or unreadable digest):
    /// nothing was printed and the loud typed error names why.
    Unavailable { reason: &'static str },
}

/// Emit the ONE release-digest attestation line when — and only when — the
/// release bootstrap launcher exported [`faktor_updater::RELEASE_DIGEST_ENV`]
/// for this child. The digest is computed with the SAME helper the health
/// build report uses ([`faktor_updater::self_digest`]: the streamed sha256 of
/// `current_exe`, i.e. this very binary), never a value copied from the
/// environment: the launcher accepts readiness only from the child's own
/// claim, so a mismatch must be observable, never papered over.
///
/// Honesty rules:
/// - a valid-length export is always answered with the ACTUAL digest, even
///   when it differs from the launcher's expected value, plus a loud typed
///   `faktor.release_digest.mismatch` error (the launcher refuses the launch);
/// - an export whose length cannot match a sha256 hex is a malformed request:
///   nothing is printed (a line the launcher must reject is never minted) and
///   a loud typed `faktor.release_digest.malformed_expected` error is logged;
/// - an unreadable self-digest is a loud typed
///   `faktor.release_digest.unreadable` error and no line.
///
/// The line carries no secrets (it is a hash of a public artifact) and is
/// written to stdout BEFORE the daemon binds/serves: [`serve_impl`] emits it
/// first and only then the frozen startup line, so a supervisor can never
/// observe readiness before the child's own digest claim.
pub(crate) fn emit_release_digest_attestation(
    out: &mut impl std::io::Write,
    expected: Option<&str>,
) -> std::io::Result<ReleaseAttestation> {
    let Some(expected) = expected else {
        return Ok(ReleaseAttestation::NotRequested);
    };
    let actual = match faktor_updater::self_digest() {
        Ok(actual) => actual,
        Err(e) => {
            tracing::error!(
                event = "faktor.release_digest.unreadable",
                "cannot attest the running binary digest ({e}); the bootstrap launcher will refuse this launch"
            );
            return Ok(ReleaseAttestation::Unavailable {
                reason: "self digest unreadable",
            });
        }
    };
    // The launcher parses exactly one sha256 hex length. An export of another
    // shape can never be attested honestly: log the typed refusal instead of
    // printing a line the launcher must reject as no attestation.
    if expected.len() != actual.len() {
        tracing::error!(
            event = "faktor.release_digest.malformed_expected",
            expected_len = expected.len(),
            actual_len = actual.len(),
            "the bootstrap launcher exported a malformed release digest; no attestation is possible"
        );
        return Ok(ReleaseAttestation::Unavailable {
            reason: "malformed expected digest",
        });
    }
    writeln!(out, "{RELEASE_DIGEST_ATTESTATION_PREFIX}{actual}")?;
    out.flush()?;
    let matches_expected = actual == expected;
    if !matches_expected {
        tracing::error!(
            event = "faktor.release_digest.mismatch",
            expected = %expected,
            actual = %actual,
            "the running binary does not match the bootstrap-verified release digest; the launcher will refuse this launch"
        );
    }
    Ok(ReleaseAttestation::Printed { matches_expected })
}

/// Adversarial covers of the daemon-side release digest attestation: the env
/// gate, the exactly-one-line/actual-digest/ordering contract, the loud typed
/// mismatch log and the digest-helper reuse proven against the health report.
#[cfg(test)]
mod release_attestation_tests {
    use super::*;

    /// A capturing `MakeWriter` sink for the loud typed error assertions.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `f` with a thread-local ERROR-level fmt subscriber whose output is
    /// captured, returning the value and the log text.
    fn capture_errors<T>(f: impl FnOnce() -> T) -> (T, String) {
        let sink = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .finish();
        let value = tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8(
            sink.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .expect("the captured log is UTF-8");
        (value, logs)
    }

    /// The unix/direct-start case: no export means NOTHING may appear on
    /// stdout (the frozen startup line stays the only line).
    #[test]
    fn an_unset_export_prints_no_attestation_line() {
        let mut out = Vec::new();
        let outcome = emit_release_digest_attestation(&mut out, None).unwrap();
        assert_eq!(outcome, ReleaseAttestation::NotRequested);
        assert!(out.is_empty(), "no line when env is unset: {out:?}");
    }

    /// A requested attestation is EXACTLY one line, 64 lowercase hex, the
    /// actual running-binary digest, and it precedes the frozen startup line
    /// (the order [`serve_impl`] writes: attestation, then bind, then the
    /// startup line that flips readiness).
    #[test]
    fn a_requested_export_prints_exactly_one_actual_line_before_readiness() {
        let actual = faktor_updater::self_digest().expect("the test binary hashes");
        assert!(
            faktor_updater::manifest::is_lower_hex(&actual, 64),
            "the helper yields 64 lowercase hex: {actual}"
        );
        let mut out = Vec::new();
        let outcome = emit_release_digest_attestation(&mut out, Some(&actual)).unwrap();
        assert_eq!(
            outcome,
            ReleaseAttestation::Printed {
                matches_expected: true
            }
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            format!("faktor release digest={actual}\n"),
            "exactly one line carrying the actual digest"
        );
        // Digest-helper reuse: the SAME value the health build report folds
        // into `self_sha256` (the running-binary digest a supervisor reads).
        let report = build_report_json(true);
        assert_eq!(report["self_sha256"].as_str(), Some(actual.as_str()));
    }

    /// The launcher-refusal path: the export names another digest. The daemon
    /// still prints the ACTUAL digest (never the expected one), logs the loud
    /// typed mismatch, and never fabricates an attestation.
    #[test]
    fn a_mismatched_export_prints_the_actual_digest_and_logs_loudly() {
        let actual = faktor_updater::self_digest().expect("the test binary hashes");
        let expected = "0".repeat(64);
        assert_ne!(
            actual, expected,
            "the fixture must differ from the real hash"
        );
        let mut out = Vec::new();
        let (outcome, logs) =
            capture_errors(|| emit_release_digest_attestation(&mut out, Some(&expected)).unwrap());
        assert_eq!(
            outcome,
            ReleaseAttestation::Printed {
                matches_expected: false
            }
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, format!("faktor release digest={actual}\n"));
        assert!(
            !text.contains(&expected),
            "the expected value is never attested: {text}"
        );
        assert!(
            logs.contains("faktor.release_digest.mismatch"),
            "the mismatch is a loud typed error: {logs}"
        );
        assert!(logs.contains(&expected) && logs.contains(&actual));
    }

    /// A malformed export can never be answered honestly: no line is minted
    /// (the launcher would reject it anyway) and the refusal is loud + typed.
    #[test]
    fn a_malformed_export_never_prints_a_minted_line() {
        let mut out = Vec::new();
        let (outcome, logs) =
            capture_errors(|| emit_release_digest_attestation(&mut out, Some("abc")).unwrap());
        assert_eq!(
            outcome,
            ReleaseAttestation::Unavailable {
                reason: "malformed expected digest"
            }
        );
        assert!(out.is_empty(), "no line for a malformed export: {out:?}");
        assert!(
            logs.contains("faktor.release_digest.malformed_expected"),
            "the malformed export is a loud typed error: {logs}"
        );
    }
}

/// Shared daemon serve core (audit 44 ordering): `agent.recover()` -> bind
/// -> print the frozen startup line -> spawn the gated backup task. A backup
/// can NEVER delay readiness: the task is spawned only after the startup
/// line, waits [`BACKUP_START_DELAY`], and is gated by policy.
///
/// `ready_tx` fires right after the startup line is printed (test probe for
/// the "startup line before any backup file exists" ordering guarantee);
/// `shutdown_rx`, when present, ends the daemon (the test harness's
/// deterministic trigger). Production passes `None`: the daemon then waits
/// for SIGTERM/SIGINT, runs the SAME [`shutdown_serving_daemon`] drain, and
/// arms a second-signal watchdog that force-exits (code 130) if the operator
/// insists while the bounded drain is still running. A signal can never kill
/// the process mid-write without the drain/backup/index joins.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_impl(
    port: u16,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
    shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), String> {
    let (config, semantic) = match serve_config_and_semantic(config_path) {
        Ok(loaded) => loaded,
        Err(e) => return Err(format!("config error: {e}")),
    };
    // The INTERACTIVE session-terminal execution-authority policy — the
    // user-initiated trust class — resolved BEFORE the config is consumed
    // and injected EXPLICITLY into the server surface below. An unset
    // `[sandbox] shell` selects the honest user-granted network-capable
    // default (no isolation claim); an EXPLICIT `shell = "os_isolated"`
    // restores OS isolation/typed refusal for terminals. The authority
    // never reads a global and never widens: what it enforces is what the
    // durable execution profile records. The agent shell-tool class keeps
    // its own secure default through `Config::sandbox_policy` and never
    // inherits this grant.
    let terminal_policy = terminal_authority_policy(&config)?;
    // Live chunk path (audit 41): BOUNDED channel (1024 events) + sink-side
    // coalescing under backpressure — a slow SSE consumer can never grow
    // the agent's memory. The drainer spawn lives in serve().
    let (chunk_sink, chunk_rx) = faktor_agent::ChunkSink::channel();
    // The whole graph is built HERE in the construction region (steps
    // 1-16 of graph::DAEMON_CONSTRUCTION_ORDER): store/session + CAS →
    // supervisor → checked transport → providers → router → budgets →
    // index → evidence → instructions → verification → agent →
    // orchestrator → shadows → tasks. Serve constructs NOTHING of its own.
    // The `[cloud]` section is resolved into the daemon's own data dir
    // BEFORE the config is consumed: when disabled (the default and every
    // pre-cloud config) the control-plane/SCM databases are neither created
    // nor opened, and the local daemon stays byte-identical. When enabled,
    // the control plane and the durable SCM rows are opened here (their
    // files live under the data dir; a hostile path is refused at startup).
    // The additive `[billing]` section (Wave 3) is captured BEFORE the
    // config is consumed by the daemon build: when disabled (the default)
    // no billing database is created and every billing route answers 409.
    let config_billing = config.billing.clone();
    // The additive `[workers]` section (remote/VPC worker plane) is captured
    // the same way: disabled by default (no worker database, every
    // /native/workers* route 409 `workers_disabled`, the TaskExecutor
    // placement seam disabled => local execution unchanged).
    let config_workers = config.workers.clone();
    // The additive `[worker_plane]` section (the worker-plane DEPLOYMENT
    // BOUNDARY) is captured the same way: disabled by default (no second
    // socket; the native listener is untouched). When enabled, a dedicated
    // listener with its own identity is bound under the TLS-or-gateway-only
    // rules and the typed refusal is raised BEFORE the daemon serves.
    let config_worker_plane = config.worker_plane.clone();
    // The additive `[enterprise]` section (retention/audit/admin plane) is
    // captured the same way: disabled by default (no enterprise database,
    // every /native/enterprise/* route 409 `enterprise_disabled`, the local
    // daemon otherwise untouched).
    let config_enterprise = config.enterprise.clone();
    // The `[cloud]` section is captured BEFORE the config is consumed too:
    // the SSO authority and the GitHub App surface are constructed after the
    // graph (they ride the graph's checked transport), and both read the
    // operator-staged payload directory. Disabled sections build nothing.
    let config_cloud = config.cloud.clone();
    // The control-plane path only: the durable SCM store is opened by the
    // graph itself (step 21, `DaemonGraph::scm`) and serve reuses it.
    let cloud_control_plane = if config.cloud.enabled {
        Some(
            config
                .cloud
                .control_plane_path(&data_dir)
                .map_err(|e| format!("cloud config: {e}"))?,
        )
    } else {
        None
    };
    // The `[updater]` section is resolved BEFORE the config is consumed:
    // when disabled (the default and every pre-updater config) NO operation
    // database and NO install root are ever created, and the daemon is
    // byte-identical to the pre-updater daemon. When enabled, the durable
    // operation store, the content-addressed install layout and the checked
    // download transport (the `[sandbox]` destination policy) are built
    // here; artifacts download through the daemon's own egress policy.
    let updater = if config.updater.enabled {
        let db = config
            .updater
            .database_path(&data_dir)
            .map_err(|e| format!("updater config: {e}"))?
            .ok_or("updater config: enabled section resolved no database")?;
        let install_root = config
            .updater
            .install_root_path(&data_dir)
            .map_err(|e| format!("updater config: {e}"))?
            .ok_or("updater config: enabled section resolved no install root")?;
        let policy = config
            .sandbox_policy()
            .map_err(|e| format!("sandbox config: {e}"))?
            .network
            .installed()
            .cloned()
            .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty);
        let store = Arc::new(
            faktor_updater::SqliteUpdaterStore::open(&db)
                .map_err(|e| format!("updater store {}: {e}", db.display()))?,
        );
        let fetcher = Arc::new(faktor_updater::CheckedHttpFetcher::new(
            faktor_provider::egress::CheckedHttpClient::try_with_policy(policy)
                .map_err(|e| format!("updater egress client: {e}"))?,
        ));
        let updater_config = faktor_updater::UpdaterConfig {
            channel: config
                .updater
                .channel()
                .map_err(|e| format!("updater config: {e}"))?,
            install_root,
            keys: config
                .updater
                .trusted_keys()
                .map_err(|e| format!("updater config: {e}"))?,
            max_artifact_bytes: config.updater.max_artifact_bytes_resolved(),
            clock_skew_ms: config.updater.clock_skew_ms_resolved(),
            host_os: std::env::consts::OS.to_string(),
            host_arch: std::env::consts::ARCH.to_string(),
            local_version: faktor_core::VERSION.to_string(),
            allow_legacy_manifests_once: config.updater.allow_legacy_manifests_once_resolved(),
        };
        let updater = faktor_updater::Updater::with_default_probe(updater_config, store, fetcher)
            .map_err(|e| format!("updater init: {e}"))?;
        Some(Arc::new(updater))
    } else {
        None
    };
    let graph =
        build_daemon_with_mcp_and_chunks_fast(&data_dir, Some(config), Some(chunk_sink), semantic)
            .await
            .map_err(|e| format!("daemon build failed: {e}"))?;
    let session = graph.session.clone();
    let agent = graph.agent.clone();
    let store = session.store();
    // Crash recovery runs before the first request (spec §7) — and before
    // bind, so all recovery work is done before readiness is announced.
    if let Err(e) = agent.recover() {
        tracing::error!("recovery failed: {e}");
    }
    // Durable verification-job recovery: stale Running rows of a previous
    // daemon are requeued BEFORE the executor can claim anything, so a
    // crashed check is retried rather than lost.
    recover_verification_jobs_at_startup(&session);
    // Durable queue-head recovery (boundary race): a pending prompt-queue row
    // whose turn already settled gets its runner now — before bind, so the
    // first request sees a daemon that has already re-attached every durable
    // head no new submit would have kicked.
    recover_pending_queues_at_startup(&graph);
    // Step 17 — the server surface consumes the graph's authorities: the
    // orchestrator and the TaskExecutor of ServerDeps are the SAME
    // instances the graph built (no second execution authority is ever
    // constructed in a daemon lifetime). The graph stays alive for the
    // whole serve below, so every authority — supervisor, shadow service,
    // budget ledger, index — lives exactly as long as the daemon.
    let mut deps = ServerDeps::new_with(
        session,
        agent,
        graph.permissions.clone(),
        graph.orchestrator.clone(),
        graph.tasks.clone(),
        graph.budgets.clone(),
    );
    // Explicit dependency injection: the daemon's terminal authority is
    // constructed under the configured shell contract resolved above (the
    // same source doctor reads), never under a hardcoded grant.
    deps = deps.with_terminal_policy(terminal_policy);
    deps.chunk_rx = Some(chunk_rx);
    // The additive real GitHub App surface (built only while the section is
    // enabled; its initial sync is backgrounded after readiness below). The
    // durable SCM store itself is the graph's (opened once at step 21) —
    // serve never opens a second store authority.
    let mut scm_daemon: Option<Arc<crate::scm_daemon::ScmDaemon>> = None;
    if let Some(control_plane_path) = cloud_control_plane {
        let control_plane_path =
            enabled_section_db_path("cloud control-plane", control_plane_path)?;
        let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
            Arc::new(
                faktor_cloud::SqliteControlPlaneStore::open(&control_plane_path).map_err(|e| {
                    format!(
                        "cloud control-plane store {}: {e}",
                        control_plane_path.display()
                    )
                })?,
            ),
            Arc::new(faktor_cloud::SystemClock),
        ));
        // The enabled `[cloud]` section opened the graph's SCM store at
        // construction (step 21); the control-plane routes, the webhook
        // surface and the completion-step adapter all share that ONE store.
        let scm_store = graph
            .scm
            .clone()
            .ok_or("cloud scm: the enabled [cloud] section resolved no scm store")?;
        deps = deps
            .with_control_plane(control_plane)
            .with_scm_store(scm_store.clone());
        // The GitHub App wiring rides the graph's ONE checked transport and
        // the operator-staged payload directory; a missing/corrupt/
        // too-permissive payload is a startup refusal (no half-wired SCM).
        scm_daemon = crate::scm_daemon::build_scm_daemon(
            &config_cloud,
            &data_dir,
            scm_store,
            graph.transport.clone(),
        )
        .map_err(|e| format!("cloud scm wiring: {e}"))?;
        if let Some(daemon) = &scm_daemon {
            deps = deps.with_scm_webhook(daemon.clone());
            tracing::info!("github app scm surface enabled");
        }
        // The completion-step SCM provider was transformed from the SAME
        // `[cloud.github_app]` section DURING daemon construction
        // (`build_daemon_core` wires `graph.tasks`), so serve never mutates
        // the built executor; a disabled section stays provider-less and a
        // contracted PR step records `native_pr_scm_not_configured`.
        tracing::info!("cloud control plane enabled");
    }
    // The additive `[cloud.sso]` section: the network OIDC adapter over the
    // daemon's checked transport plus the operator-staged client secret.
    // Disabled (the default) = no adapter is built and /native/sso/* keeps
    // its typed 409 parity.
    if let Some(sso_cfg) = config_cloud.sso.as_ref().filter(|sso| sso.enabled) {
        let payload_root = config_cloud
            .payload_root(&data_dir)
            .map_err(|e| format!("cloud config: {e}"))?;
        let authority =
            crate::sso_auth::build_sso_authority(sso_cfg, &payload_root, graph.transport.clone())
                .map_err(|e| format!("cloud sso wiring: {e}"))?;
        deps = deps.with_sso(authority);
        tracing::info!("sso authority enabled");
    }
    // The additive `[billing]` section (Wave 3): when disabled (the default)
    // no billing database is created and every billing route answers 409.
    // When enabled, the durable usage/credit ledger opens here (its own
    // `billing.db` through the SAME control-plane store + migration ladder),
    // the configured account is provisioned idempotently, and the
    // orchestrator's admission gate is wired to the entitlement service —
    // the gate is consulted at the three safe boundaries only, so an
    // entitlement change can never interrupt an in-flight
    // integration/rollback/completion transaction.
    let mut billing_gate: Option<Arc<dyn faktor_orchestrator::admission::AdmissionGate>> = None;
    // The additive `[billing.report]` schedule (disabled by default): a
    // durable period/cursor schedule that reports the folded usage through
    // the vendor adapter after readiness.
    let mut billing_report: Option<Arc<crate::billing_report::BillingReportRunner>> = None;
    if config_billing.enabled {
        let path = config_billing
            .billing_path(&data_dir)
            .map_err(|e| format!("billing config: {e}"))?;
        let organization = config_billing
            .organization()
            .map_err(|e| format!("billing config: {e}"))?;
        let account = config_billing
            .account_id()
            .map_err(|e| format!("billing config: {e}"))?;
        let service_config = config_billing
            .service_config()
            .map_err(|e| format!("billing config: {e}"))?
            .ok_or_else(|| {
                "billing config: an enabled section carries a service config".to_string()
            })?;
        let store = {
            let path = enabled_section_db_path("billing", path)?;
            Arc::new(
                faktor_cloud::SqliteControlPlaneStore::open(&path)
                    .map_err(|e| format!("billing store {}: {e}", path.display()))?,
            ) as Arc<dyn faktor_cloud::BillingStore>
        };
        let service = faktor_cloud::EntitlementService::with_system_clock(store, service_config)
            .map_err(|e| format!("billing service: {e}"))?;
        // The report schedule (additive; disabled by default): strict vendor
        // config, the durable billing store and the daemon's checked
        // transport. A missing vendor credential env var refuses startup —
        // never a silently unauthenticated report.
        if let Some(report_cfg) = config_billing
            .report
            .as_ref()
            .filter(|report| report.enabled)
        {
            let vendor_config = report_cfg.vendor_config()?;
            let policy = report_cfg.policy()?;
            let adapter = if report_cfg.unauthenticated() {
                faktor_cloud::BillingVendorAdapter::new(
                    service.clone(),
                    vendor_config,
                    graph.transport.clone(),
                    None,
                )
            } else {
                faktor_cloud::BillingVendorAdapter::from_env(
                    service.clone(),
                    vendor_config,
                    graph.transport.clone(),
                )
            }
            .map_err(|e| format!("billing report config: {e}"))?;
            billing_report = Some(Arc::new(crate::billing_report::BillingReportRunner::new(
                service.store().clone(),
                adapter,
                organization.clone(),
                policy,
                Arc::new(faktor_cloud::SystemClock),
            )));
            tracing::info!("billing report schedule enabled");
        }
        let account_name = config_billing
            .account_name
            .clone()
            .unwrap_or_else(|| "local".to_string());
        let managed = config_billing.managed.unwrap_or(true);
        service
            .ensure_account(&organization, &account, &account_name, managed)
            .map_err(|e| format!("billing account provisioning: {e}"))?;
        // Wave 5 residual: install the agent-side debit authority, so a
        // Faktor-managed provider attempt opens its durable credit hold
        // BEFORE dispatch (record-before-call) and settles/refunds with the
        // reservation ledger; BYOK providers are classified and never
        // debited. Without this install the agent dispatch path is exactly
        // the pre-billing path (no debit call exists).
        // A poisoned authority slot must refuse daemon startup (fail-closed):
        // silently continuing would run managed traffic without the debit
        // authority it is configured to have.
        graph
            .agent
            .set_provider_debits(Some(Arc::new(billing_debits::CloudAttemptDebits::new(
                service.clone(),
                organization.clone(),
                account.clone(),
            ))))
            .map_err(|e| format!("installing the agent debit authority: {e}"))?;
        billing_gate = Some(Arc::new(BillingAdmissionGate::new(
            service.clone(),
            organization,
        )));
        deps = deps.with_billing(service);
        tracing::info!("commercial billing enabled");
    }
    if let Some(gate) = billing_gate {
        graph.orchestrator.set_admission_gate(gate);
    }
    // The additive `[workers]` section: when disabled (the default) nothing
    // is built and no worker database is created. When enabled, the durable
    // plane opens under the data dir, its crash recovery runs BEFORE the
    // first request (expired leases requeue deterministically; unleased
    // assignments are re-attempted), the worker routes are wired, and the
    // TaskExecutor gains the placement seam so a new run is placed on an
    // eligible registered worker instead of executing locally.
    if config_workers.enabled {
        let path = config_workers
            .workers_path(&data_dir)
            .map_err(|e| format!("workers config: {e}"))?;
        let organization = config_workers
            .organization()
            .map_err(|e| format!("workers config: {e}"))?;
        let trust_domain = config_workers
            .trust_domain()
            .map_err(|e| format!("workers config: {e}"))?;
        let requirements = config_workers
            .requirements()
            .map_err(|e| format!("workers config: {e}"))?;
        let store = {
            let path = enabled_section_db_path("workers", path)?;
            Arc::new(
                faktor_worker::SqliteWorkerStore::open(&path)
                    .map_err(|e| format!("worker store {}: {e}", path.display()))?,
            ) as Arc<dyn faktor_worker::WorkerStore>
        };
        let plane = faktor_worker::WorkerPlane::with_system_clock(store);
        // Crash recovery before the first request: a lease whose heartbeat
        // window closed while the daemon was down expires (attempt terminal
        // + bounded requeue), and an assignment committed without its lease
        // is re-attempted. Idempotent: a clean restart reports nothing.
        let recovery = plane
            .recover(&organization)
            .map_err(|e| format!("worker recovery: {e}"))?;
        let resumed = plane
            .resume_assignments(&organization)
            .map_err(|e| format!("worker assignment recovery: {e}"))?;
        if !recovery.expired.is_empty() || !resumed.reassigned.is_empty() {
            tracing::warn!(
                expired = recovery.expired.len(),
                requeued = recovery.requeued.len(),
                exhausted = recovery.exhausted.len(),
                reassigned = resumed.reassigned.len(),
                "worker plane recovery resolved interrupted leases/assignments"
            );
        }
        graph
            .tasks
            .set_worker_placement(faktor_orchestrator::placement::WorkerPlacement::enabled(
                Arc::new(WorkerPlaneAdapter {
                    plane: plane.clone(),
                    organization,
                    trust_domain,
                    requirements,
                }),
            ));
        deps = deps.with_workers(plane);
        tracing::info!("remote/vpc worker plane enabled");
    }
    // The additive `[enterprise]` section: when disabled (the default)
    // nothing is built and no enterprise database is created. When enabled,
    // the retention/audit/admin service opens its own `enterprise.db`
    // (through the SAME control-plane store + migration ladder; the audit
    // table is append-only) and the GC route is wired to the daemon's REAL
    // session store + CAS, so protected rollback material is refused at the
    // scanner and again at the storage layer. An enabled section requires
    // the cloud section (the principal source); the pair is refused at
    // config load, so reaching here without it is a bug.
    if config_enterprise.enabled {
        if deps.control_plane.is_none() {
            return Err(
                "enterprise: an enabled [enterprise] section requires [cloud] enabled".into(),
            );
        }
        let path = config_enterprise
            .enterprise_path(&data_dir)
            .map_err(|e| format!("enterprise config: {e}"))?
            .ok_or_else(|| "enterprise config: enabled without a database path".to_string())?;
        let store = Arc::new(
            faktor_cloud::SqliteControlPlaneStore::open(&path)
                .map_err(|e| format!("enterprise store {}: {e}", path.display()))?,
        ) as Arc<dyn faktor_cloud::EnterpriseStore>;
        let service = Arc::new(
            faktor_cloud::EnterpriseService::with_system_clock(store)
                .map_err(|e| format!("enterprise service: {e}"))?,
        );
        let runtime = Arc::new(faktor_server::native::enterprise::RetentionRuntime::new(
            deps.session.store(),
            deps.session.cas(),
        ));
        deps = deps.with_enterprise(service, Some(runtime));
        tracing::info!("enterprise retention/audit plane enabled");
    }
    // The signed updater (additive; disabled by default): crash recovery
    // runs BEFORE the first request — an interrupted apply is resumed or
    // rolled back, never left half-applied. The daemon probe re-hashes the
    // swapped artifact (the CLI-local `faktor updater apply` runs the real
    // doctor quick check instead). When THIS process was launched through
    // the bootstrap launcher, its verified release digest is passed to
    // recovery: a release activation whose pointer is in place is only
    // resumed as applied when the running process IS the activated release.
    if let Some(updater) = &updater {
        let running_release_digest = faktor_updater::running_release().map(|(_, digest)| digest);
        match updater.recover_with_running_digest(now_ms(), running_release_digest.as_deref()) {
            Ok(outcomes) if !outcomes.is_empty() => {
                for outcome in &outcomes {
                    tracing::warn!(
                        "updater recovery resolved an interrupted operation: {outcome:?}"
                    );
                }
            }
            Ok(_) => {}
            Err(e) => tracing::error!("updater recovery failed: {e}"),
        }
        deps = deps.with_updater(updater.clone());
        tracing::info!(
            "signed updater enabled (channel {})",
            updater.config().channel
        );
    }
    // The running-build report (audit: activation must be observable as a
    // PROCESS fact, not metadata): the daemon logs one bounded JSON line to
    // stderr and — when the bootstrap launcher verified a release — folds
    // the release id + digest into `version`, so `/native/health` names the
    // exact artifact a supervisor is talking to. A directly started binary
    // reports no release identity (honest absence).
    tracing::info!("faktor build: {}", build_report_json(false));
    if let Some((release_id, digest)) = faktor_updater::running_release() {
        deps.version = format!("{}+release.{release_id}.{digest}", faktor_core::VERSION);
    }
    // The ONE semantic-provider registry: the SAME Arc the graph built and
    // the agent holds — the native introspection endpoints inspect only
    // `deps.semantic` (no parallel registry exists anywhere).
    deps = deps.with_semantic_registry(graph.semantic.clone());
    // The frontend generates the secret and passes it via env; the
    // daemon reads it here and never prints it. An explicitly supplied
    // value that is not the canonical 64-hex form fails startup loudly —
    // never a silent downgrade to weaker auth.
    deps.server_password = ServerPassword::try_from_env().map_err(|e| e.to_string())?;
    // The workspace root rides the global event envelope.
    deps.directory = std::env::current_dir()
        .ok()
        .map(|d| d.display().to_string());
    // Wire the native snapshot store so the wire revert/unrevert/diff
    // endpoints restore real files: the checkpoint store shares the
    // daemon's store + CAS (same rows, same blobs).
    let fs = faktor_fs::WorkspaceFileService::new();
    let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(
        deps.session.cas(),
        deps.session.store(),
    ));
    deps = deps.with_snapshots(fs, snapshots);
    // Wire the daemon's DURABLE evidence store of record (audit 82/CCR):
    // the native server receives the graph's ONE authority (the SAME `Arc`
    // the runtime's ContextCompiler selects from and the runtime's archiver
    // inserts into). No parallel authority is constructed here, so ids are
    // globally unique across restart, scope checks are enforced by the one
    // authority, and a foreign session can never read even knowing a
    // backing digest.
    deps = deps.with_evidence_store(Arc::new(std::sync::RwLock::new(Box::new(
        graph.evidence.clone(),
    ))));
    // Bind BEFORE readiness and BEFORE any backup work (audit 44): the
    // historic code ran rotate_backup synchronously between recover() and
    // bind, so a slow or cold backup delayed first-request readiness.
    // The additive `[worker_plane]` deployment boundary: resolve the typed
    // bind/transport config (a refusal here is a STARTUP refusal naming the
    // boundary) and bind the dedicated listener BEFORE the native listener,
    // so a refused boundary never leaves a half-started daemon. The native
    // listener stays loopback-only and reads none of these keys.
    let worker_plane_config = config_worker_plane
        .resolve()
        .map_err(|e| format!("worker plane config: {e}"))?;
    // The dedicated listener's daemon-queryable slot (see
    // [`faktor_server::api::WorkerPlaneListener`]): created BEFORE the deps
    // envelope is shared and installed with the bound handle before the
    // native listener starts, so `/native/health` reports the typed worker
    // plane state (serving / unavailable / stopped / disabled) and never
    // observes an enabled-but-uninstalled plane.
    let worker_plane_listener = worker_plane_config
        .as_ref()
        .map(|_| faktor_server::api::WorkerPlaneListener::default());
    if let Some(listener) = &worker_plane_listener {
        deps = deps.with_worker_plane_listener(listener.clone());
    }
    // The bounded live chunk stream is drained exactly once, before the deps
    // envelope is shared between the two listeners.
    faktor_server::drain_chunk_stream(&mut deps);
    let deps = std::sync::Arc::new(deps);
    if let Some(worker_plane_config) = worker_plane_config {
        let handle = faktor_server::serve_worker_plane(deps.clone(), worker_plane_config)
            .await
            .map_err(|e| format!("worker plane: {e}"))?;
        // The audit record of the boundary decision (including the
        // trusted-gateway acknowledgement, when given).
        tracing::info!("{}", handle.exposure.audit_line());
        if handle.exposure.beyond_loopback {
            tracing::warn!(
                "worker plane listens on {} beyond loopback behind an acknowledged trusted gateway ({}); TLS/mTLS terminate at the gateway",
                handle.addr,
                handle.exposure.audit_line()
            );
        }
        // The slot OWNS the handle from here on: `/native/health` queries its
        // typed status, and the shutdown sequence takes it back out for the
        // bounded join (never a detached listener, never an ownerless task).
        if let Some(listener) = &worker_plane_listener {
            listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .install(handle);
        }
    }
    // The release digest attestation (non-unix bootstrap readiness; see
    // [`emit_release_digest_attestation`]) is written BEFORE the native
    // listener binds/serves, so the child's own digest claim can never follow
    // the readiness it proves. Only the bootstrap launcher's exported
    // `FAKTOR_RELEASE_DIGEST` requests it; a directly started daemon (the
    // unix default) prints nothing here and stdout stays byte-identical.
    {
        let requested_release_digest = std::env::var(faktor_updater::RELEASE_DIGEST_ENV).ok();
        let mut stdout = std::io::stdout().lock();
        emit_release_digest_attestation(&mut stdout, requested_release_digest.as_deref())
            .map_err(|e| format!("release digest attestation: {e}"))?;
    }
    let handle = faktor_server::serve_arc(deps, port)
        .await
        .map_err(|e| format!("failed to bind: {e}"))?;
    // The frozen stdout line; nothing else may be printed (the ONE optional
    // release-digest attestation above precedes it). Readiness is now
    // announced — no backup has run yet and, by construction, cannot have.
    println!("{}", handle.startup_line);
    tracing::info!("faktor serving on {}", handle.addr);
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }
    // Automatic online backup (spec §24), post-ready and low priority: a
    // delayed spawned task, gated by policy (audit 44: interval +
    // staleness), so it never delays startup and never piles hourly
    // snapshots onto rapid restarts. Best effort, runs exactly once. The
    // SYNC backup/rotation work runs on the blocking pool
    // (`spawn_blocking` — P0-46: the sync SQLite snapshot used to run
    // inline on a Tokio worker, stalling every other task on that worker
    // for the whole snapshot); the async wrapper only sleeps, gates, and
    // joins.
    let backup_task = spawn_startup_backup(store, data_dir.clone());
    // The daemon verification executor: post-readiness, it claims and
    // resolves durable verification jobs of every session asynchronously.
    let verification_executor = spawn_verification_executor(&graph);
    // The GitHub App surface (post-readiness, like the backup/index): ONE
    // bounded initial installation/repository sync, then one idempotent
    // re-sync per verified webhook delivery through the bounded queue.
    let scm_task = scm_daemon.map(|daemon| tokio::spawn(daemon.run()));
    // The billing report schedule (post-readiness): one bounded tick per
    // configured interval; every period is reported at most once.
    let billing_report_task = billing_report.map(|runner| tokio::spawn(runner.run()));
    // Keep the daemon alive; when a shutdown is signaled, stop the owned
    // worker-plane listener FIRST (bounded graceful join of its serve task;
    // an unexpected death is named by the typed health snapshot), stop the
    // graph-hosted repository index reconciliation worker (bounded join),
    // close and DRAIN the TaskExecutor's detached drives (bounded
    // graceful-then-abort), then drain the backup task (bounded) and the
    // owned Ollama warm-up threads before the daemon returns. The drive
    // drain runs BEFORE the backup drain so the final snapshot sees every
    // settled record-first write; a straggler aborted by the registry stays
    // resumable from its durable rows.
    match shutdown_rx {
        Some(rx) => {
            let _ = rx.await;
            shutdown_serving_daemon(
                &graph.tasks,
                Some(handle),
                worker_plane_listener,
                graph.index.clone(),
                Some(&graph.agent),
                verification_executor,
                scm_task,
                billing_report_task,
                backup_task,
            )
            .await;
        }
        None => {
            // Production: SIGTERM/SIGINT trigger the SAME drain as the test
            // harness's shutdown channel. A second signal force-exits (the
            // drain itself is bounded, this is the operator's escalate).
            let signal = wait_for_shutdown_signal().await;
            tracing::info!(signal, "shutdown signal received; draining the daemon");
            #[cfg(test)]
            SIGNAL_DRAIN_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
            spawn_force_exit_watchdog();
            shutdown_serving_daemon(
                &graph.tasks,
                Some(handle),
                worker_plane_listener,
                graph.index.clone(),
                Some(&graph.agent),
                verification_executor,
                scm_task,
                billing_report_task,
                backup_task,
            )
            .await;
        }
    }
    Ok(())
}

/// The ACP agent server (`faktor-cli acp`): build the REAL daemon graph over
/// the data dir (config from `faktor-plus.json` in the data dir when present,
/// else defaults; NO MCP layer — the acp surface needs the same providers,
/// tools, session store and agent the native daemon serves) and serve the
/// ACP wire protocol on stdin/stdout until EOF or `shutdown`.
pub(crate) async fn acp(data_dir: PathBuf) {
    let config = load_acp_config(&data_dir);
    // The interactive session-terminal execution-authority policy — the
    // user-initiated trust class, resolved BEFORE the config is consumed —
    // so the ACP terminal authority receives it at construction (explicit
    // injection; no global read, no hardcoded grant). Unset means the
    // honest user-granted default; explicit `os_isolated` isolates/refuses.
    let terminal_policy = match terminal_authority_policy(&config) {
        Ok(policy) => policy,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("data dir error: {e}");
        std::process::exit(1);
    }
    // The ACP surface is stdio-only: no SSE subscribers exist, so there is
    // no chunk sink (None = the runtime skips live-chunk overhead entirely).
    // The daemon graph is built whole by [`build_daemon`] (the construction
    // region; steps 1-16) — ACP constructs no supervisor or executor of its
    // own and takes its session/agent references from the graph.
    let graph = match build_daemon(&data_dir, Some(config)) {
        Ok(graph) => graph,
        Err(e) => {
            eprintln!("daemon build failed: {e}");
            std::process::exit(1);
        }
    };
    let (session, agent) = (graph.session.clone(), graph.agent.clone());
    // Crash recovery runs before the first request (spec §7), like serve.
    if let Err(e) = agent.recover() {
        tracing::error!("recovery failed: {e}");
    }
    // Durable queue-head recovery, exactly like serve: the same order (after
    // `agent.recover()`, before the first prompt is accepted) over the same
    // executor, so a killed process's pending durable head is driven without
    // any new submit.
    recover_pending_queues_at_startup(&graph);
    // The ACP prompt surface enters the SAME product execution authority the
    // daemon server uses (the graph's ONE TaskExecutor over the ONE session
    // store) — ACP translates its wire prompts, it never drives the agent.
    let prompts =
        faktor_server::native::PromptExecutionService::new(graph.tasks.clone(), session.clone());
    // The negotiated `faktor.terminal` seam: the daemon's session-owned
    // terminal authority (the same faktor-pty + ownership-row registry the
    // native terminal surface serves) over the SAME session manager the
    // backend drives. Without this the extension is never negotiated and
    // every `terminal/*` method stays the official -32601.
    let terminal_authority: Arc<dyn faktor_acp::TerminalAuthority> = Arc::new(
        DaemonTerminalAuthority::new(session.clone(), terminal_policy),
    );
    let backend = DaemonAcpBackend::new(session, agent, prompts);
    match AcpServer::new(backend)
        .with_terminal_authority(terminal_authority)
        .run_stdio()
        .await
    {
        Ok(()) => {}
        Err(e) => {
            eprintln!("acp server error: {e}");
            std::process::exit(1);
        }
    }
}

/// Daemon config for `acp`: `faktor-plus.json` next to the data dir when it
/// exists; a broken file is a loud warning that falls back to defaults (the
/// daemon still serves — same policy as `serve`).
pub(crate) fn load_acp_config(data_dir: &std::path::Path) -> config::Config {
    let path = data_dir.join("faktor-plus.json");
    if !path.exists() {
        return config::Config::default();
    }
    match config::Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("config error: {e}; using defaults");
            config::Config::default()
        }
    }
}

/// The daemon's worker-plane placement adapter: the orchestrator's seam
/// consults it BEFORE any local durable write of a new run. It maps the
/// provider-neutral placement spec onto the worker plane:
///
/// - no eligible worker => `Local` (the run executes locally exactly as
///   before; nothing is written to the worker plane);
/// - an eligible worker => the plane mints one IMMUTABLE job generation,
///   CAS-accepts its lease and `Remote` is returned: the local executor
///   starts NOTHING and the plane's durable job/lease owns the attempt.
///
/// A plane failure is returned as a string and becomes the executor's typed
/// `ExecError::PlacementRefused` — never a silent local run.
pub(crate) struct WorkerPlaneAdapter {
    pub(crate) plane: Arc<faktor_worker::WorkerPlane>,
    pub(crate) organization: faktor_cloud::OrganizationId,
    pub(crate) trust_domain: String,
    pub(crate) requirements: faktor_worker::JobRequirements,
}

impl faktor_orchestrator::placement::WorkerPlacementSeam for WorkerPlaneAdapter {
    fn place(
        &self,
        spec: &faktor_orchestrator::placement::PlacementSpec,
    ) -> Result<faktor_orchestrator::placement::PlacementDecision, String> {
        let mut requirements = self.requirements.clone();
        // The executor's spec carries the run shape; the daemon's `[workers]`
        // defaults carry the environment requirements. A spec-provided field
        // wins (the executor currently sends none).
        if let Some(os) = &spec.os {
            requirements.os = Some(os.clone());
        }
        if let Some(arch) = &spec.arch {
            requirements.arch = Some(arch.clone());
        }
        if !spec.toolchains.is_empty() {
            requirements.toolchains = spec.toolchains.clone();
        }
        if let Some(region) = &spec.region {
            requirements.region = Some(region.clone());
        }
        if spec.min_cpu_cores > 0 {
            requirements.min_cpu_cores = spec.min_cpu_cores;
        }
        if spec.min_memory_mb > 0 {
            requirements.min_memory_mb = spec.min_memory_mb;
        }
        if spec.gpu {
            requirements.gpu = true;
        }
        requirements.trust_domain = self.trust_domain.clone();
        requirements
            .normalize()
            .map_err(|e| format!("placement requirements: {e}"))?;
        // Only mint a generation when a worker can actually take it: the
        // local run stays the answer otherwise.
        match self
            .plane
            .find_eligible(&self.organization, &self.trust_domain, &requirements)
        {
            Ok(None) => return Ok(faktor_orchestrator::placement::PlacementDecision::Local),
            Ok(Some(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
        let job_key = faktor_worker::JobKey::try_new(spec.job_key.clone())
            .map_err(|e| format!("placement job key: {e}"))?;
        let scheduled = self
            .plane
            .schedule_job(
                &self.organization,
                &self.trust_domain,
                &job_key,
                requirements,
                &spec.payload_digest,
                faktor_worker::default_requeue(),
                None,
            )
            .map_err(|e| e.to_string())?;
        match scheduled.lease {
            Some(lease) => Ok(faktor_orchestrator::placement::PlacementDecision::Remote {
                job_id: scheduled.job.job_id.to_string(),
                worker_id: lease.worker_id.to_string(),
                generation: scheduled.generation.as_u64(),
                lease_id: lease.lease_id.to_string(),
            }),
            // The CAS lost a race (the worker vanished between the
            // eligibility read and the accept): execute locally rather than
            // stranding the run.
            None => Ok(faktor_orchestrator::placement::PlacementDecision::Local),
        }
    }
}

/// The Wave 3 billing admission gate of the local daemon: it maps the
/// orchestrator's provider-neutral boundary hook onto the cloud entitlement
/// service for the daemon's configured tenant organization. The gate is
/// consulted at the three safe boundaries only (new task, new child spawn,
/// new provider attempt BEFORE dispatch); a refusal is a typed
/// `ExecError::AdmissionRefused` naming the exact limit and admits nothing.
pub(crate) struct BillingAdmissionGate {
    pub(crate) service: Arc<faktor_cloud::EntitlementService>,
    pub(crate) organization: faktor_cloud::OrganizationId,
}

impl BillingAdmissionGate {
    pub(crate) fn new(
        service: Arc<faktor_cloud::EntitlementService>,
        organization: faktor_cloud::OrganizationId,
    ) -> Self {
        Self {
            service,
            organization,
        }
    }
}

impl faktor_orchestrator::admission::AdmissionGate for BillingAdmissionGate {
    fn check(
        &self,
        request: &faktor_orchestrator::admission::AdmissionRequest,
    ) -> Result<(), faktor_orchestrator::admission::AdmissionRefusal> {
        use faktor_orchestrator::admission::AdmissionBoundary as Hook;
        let hook_boundary = request.boundary;
        let boundary = match hook_boundary {
            Hook::NewTask => faktor_cloud::AdmissionBoundary::NewTask,
            Hook::NewChildSpawn => faktor_cloud::AdmissionBoundary::NewChildSpawn,
            Hook::NewProviderAttempt => faktor_cloud::AdmissionBoundary::NewProviderAttempt,
        };
        let request = faktor_cloud::AdmissionRequest {
            boundary,
            task_id: request.task_id,
            provider: request.provider.clone(),
            observed: faktor_cloud::ObservedUsage {
                active_tasks: request.observed.active_tasks,
                children_of_task: request.observed.children_of_task,
                provider_attempts_of_task: request.observed.provider_attempts_of_task,
                estimated_provider_cost_micro: request.observed.estimated_provider_cost_micro,
            },
        };
        match self.service.check_admission(&self.organization, &request) {
            Ok(_) => Ok(()),
            Err(e) => Err(faktor_orchestrator::admission::AdmissionRefusal::new(
                hook_boundary,
                e.limit.clone(),
                e.to_string(),
            )),
        }
    }
}

/// The ACP backend over the REAL daemon: sessions, turns, and abort go
/// through the durable `SessionManager` and `AgentRuntime` (crash recovery,
/// journal, cancellation, and providers included). ACP wire/lifecycle
/// handling lives in `faktor-acp`; this seam only maps requests onto the
/// runtime, mirroring how the native server endpoints drive the daemon.
pub(crate) struct DaemonAcpBackend {
    pub(crate) session: Arc<SessionManager>,
    pub(crate) agent: Arc<AgentRuntime>,
    /// The ONE product execution entry every ACP `session/prompt` goes
    /// through: an ordinary prompt becomes an in-session run through the
    /// daemon's TaskExecutor (default shadow mutation), identical to the
    /// Native and SDK prompt surfaces.
    pub(crate) prompts: Arc<faktor_server::native::PromptExecutionService>,
}

impl DaemonAcpBackend {
    pub(crate) fn new(
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        prompts: Arc<faktor_server::native::PromptExecutionService>,
    ) -> Self {
        Self {
            session,
            agent,
            prompts,
        }
    }

    /// The session's provider: `params.provider` when given, else the ONLY
    /// registered provider instance (an ACP client without a provider
    /// preference binds the daemon's single provider deterministically).
    /// Zero or several providers refuse loudly instead of guessing.
    pub(crate) fn resolve_provider(&self, params: &Value) -> Result<String, String> {
        if let Some(p) = params.get("provider").and_then(Value::as_str) {
            return Ok(p.to_string());
        }
        let ids = self.agent.deps().providers.ids();
        match ids.len() {
            1 => Ok(ids.into_iter().next().expect("len 1")),
            0 => Err(
                "session/new: no providers are registered; configure one or pass params.provider"
                    .into(),
            ),
            _ => Err(format!(
                "session/new: multiple providers registered ({ids:?}); pass params.provider"
            )),
        }
    }
}

impl AcpBackend for DaemonAcpBackend {
    fn agent_info(&self) -> Value {
        let mut families: Vec<String> = self
            .agent
            .deps()
            .providers
            .all()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        families.sort();
        families.dedup();
        json!({
            "name": "Faktor",
            "version": faktor_core::VERSION,
            "providerFamilies": families,
        })
    }

    fn create_session(&self, params: &Value) -> Result<String, String> {
        let workspace = params
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("/");
        let title = params.get("title").and_then(Value::as_str).unwrap_or("acp");
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.agent.deps().model.clone());
        let provider = self.resolve_provider(params)?;
        let ws = self
            .session
            .create_workspace(workspace)
            .map_err(|e| e.message)?;
        let row = self
            .session
            .create_session(ws, title, &provider, &model)
            .map_err(|e| e.message)?;
        // Session lifecycle hook (audit): sessions are created here (the
        // session manager), not in the runtime — the daemon entry fires
        // SessionStart best-effort right after creation. Native server-side
        // session creation lives in crates/server, outside the CLI.
        self.agent
            .run_lifecycle_hook(faktor_hooks::HookEvent::SessionStart, row.id());
        Ok(row.id().to_string())
    }

    fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
        let sid = parse_session_id(session_id)?;
        if text.trim().is_empty() {
            return Err("prompt must not be empty".into());
        }
        if text.len() > faktor_session::MAX_PROMPT_BYTES {
            return Err(format!(
                "prompt of {} bytes exceeds the {} byte bound",
                text.len(),
                faktor_session::MAX_PROMPT_BYTES
            ));
        }
        // The ONE execution entry: the prompt becomes an in-session run
        // through the daemon's TaskExecutor (default shadow mutation). The
        // AcpBackend seam is synchronous (one serialized ACP request at a
        // time); the service call is async, so bridge sync → async on the
        // serve task via block_in_place (multi-threaded daemon runtime).
        let service = self.prompts.clone();
        let request = faktor_server::native::PromptRequest {
            prompt: text.to_string(),
            ..Default::default()
        };
        let receipt = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(service.prompt(sid, request))
        })
        .map_err(|e| e.to_string())?;
        let session = self.session.clone();
        if receipt.queued {
            // The prompt durably queued behind another actor's active turn;
            // the executor's own runner delivers it. Report the queued
            // acceptance, mirroring the previous backend behavior.
            let state = session
                .get_session(sid)
                .map_err(|e| e.message)?
                .ok_or_else(|| format!("session {sid}"))?
                .state()
                .map_err(|e| e.message)?;
            return Ok(json!({
                "status": "queued",
                "finalState": state,
            }));
        }
        // Accepted: wait for the turn machine to leave the mid-turn states
        // (the same durable wait the wire prompt path performs), then report
        // the machine's final state. Bounded by the configured turn budget
        // (fallback when the operator opted out with 0): a never-settling
        // machine yields a typed timeout instead of polling forever.
        let settle_deadline = {
            let budget = self.session.turn_budget_ms();
            if budget == 0 {
                TURN_SETTLE_FALLBACK
            } else {
                std::time::Duration::from_millis(budget)
            }
        };
        let state = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(await_turn_settled(
                &session,
                sid,
                settle_deadline,
            ))
        })?;
        Ok(json!({
            "status": "completed",
            "finalState": state,
        }))
    }

    fn abort(&self, session_id: &str) -> Result<(), String> {
        let sid = parse_session_id(session_id)?;
        self.agent.abort(sid).map(|_| ()).map_err(|e| e.message)
    }

    fn list_sessions(&self) -> Vec<String> {
        self.session
            .list_sessions(None)
            .map(|handles| handles.iter().map(|h| h.id().to_string()).collect())
            .unwrap_or_default()
    }
}

/// Session ids ride the ACP wire as plain decimal strings. Hostile ids
/// (non-numeric, zero, overflowing u64) are loud errors — never a panic.
pub(crate) fn parse_session_id(s: &str) -> Result<SessionId, String> {
    let raw: u64 = s.parse().map_err(|_| format!("invalid session id {s:?}"))?;
    if raw == 0 {
        return Err("invalid session id \"0\"".into());
    }
    Ok(SessionId::new(raw))
}

/// Poll interval of the ACP turn-settle wait.
pub(crate) const TURN_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Fallback ACP turn-settle deadline when the operator configured an
/// unbounded (`0`) wall-clock turn budget: the wait is still bounded.
pub(crate) const TURN_SETTLE_FALLBACK: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

/// Wait for the durable turn machine to leave the mid-turn states, with an
/// explicit wall deadline. `read` returns the current durable state (or a
/// typed read error). A machine that never settles (dead executor, lost
/// wakeup, a stuck driver) exhausts the deadline and returns a TYPED timeout
/// naming the state last observed — the ACP bridge never polls forever and
/// never reports a silent success.
pub(crate) async fn await_settled<F>(
    mut read: F,
    deadline: std::time::Duration,
) -> Result<faktor_core::state::AgentState, String>
where
    F: FnMut() -> Result<faktor_core::state::AgentState, String>,
{
    let started = std::time::Instant::now();
    loop {
        let state = read()?;
        if !faktor_server::native::turn_machine_busy(state) {
            return Ok(state);
        }
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            return Err(format!(
                "turn did not settle within {deadline:?} (still {state:?} after {elapsed:?})"
            ));
        }
        tokio::time::sleep(TURN_SETTLE_POLL.min(deadline - elapsed)).await;
    }
}

/// The session-backed [`await_settled`]: resolves the durable handle on every
/// poll (a recreated handle is never a stale in-memory state).
pub(crate) async fn await_turn_settled(
    session: &Arc<SessionManager>,
    sid: SessionId,
    deadline: std::time::Duration,
) -> Result<faktor_core::state::AgentState, String> {
    await_settled(
        || {
            let handle = match session.get_session(sid) {
                Ok(Some(h)) => h,
                Ok(None) => return Err(format!("session {sid}")),
                Err(e) => return Err(e.message),
            };
            handle.state().map_err(|e| e.message)
        },
        deadline,
    )
    .await
}

/// The daemon's session-owned terminal authority as the ACP
/// `faktor.terminal` seam: a thin adapter over
/// [`faktor_server::native::terminal_authority::TerminalRegistry`] — the same
/// `faktor-pty` + ownership-row authority the native terminal surface uses —
/// so the ACP server answers terminal methods only when the capability is
/// negotiated AND this authority is attached. Without it (or without
/// negotiation) every `terminal/*` method stays the official `-32601`.
pub(crate) struct DaemonTerminalAuthority {
    pub(crate) registry: Arc<faktor_server::native::terminal_authority::TerminalRegistry>,
}

impl DaemonTerminalAuthority {
    /// The authority is rooted at the SAME session manager the ACP backend
    /// drives, so a terminal can only be created for a real durable session
    /// and its row carries that session's durable task/operation identity.
    /// The execution policy (the configured `[sandbox]` shell contract) is
    /// injected AT CONSTRUCTION — the same instance value the native surface
    /// gets, so both adapters enforce and record one configured mode.
    pub(crate) fn new(
        session: Arc<SessionManager>,
        policy: faktor_server::native::terminal_authority::TerminalAuthorityPolicy,
    ) -> Self {
        Self {
            registry:
                faktor_server::native::terminal_authority::TerminalRegistry::for_manager_with_policy(
                    &session, policy,
                ),
        }
    }
}

/// Map the daemon registry error onto the ACP terminal error taxonomy
/// (invalid params vs internal refusal).
pub(crate) fn map_terminal_registry_error(
    error: faktor_server::native::terminal_authority::TerminalRegistryError,
) -> faktor_acp::TerminalError {
    use faktor_server::native::terminal_authority::TerminalRegistryError as RegistryError;
    match error {
        RegistryError::Invalid(message) => faktor_acp::TerminalError::Invalid(message),
        RegistryError::Refused(message) => faktor_acp::TerminalError::Refused(message),
        RegistryError::Unavailable(message) => faktor_acp::TerminalError::Unavailable(message),
    }
}

impl faktor_acp::TerminalAuthority for DaemonTerminalAuthority {
    fn create(
        &self,
        session_id: &str,
        spec: &faktor_acp::TerminalSpec,
    ) -> Result<Arc<dyn faktor_acp::TerminalHandle>, faktor_acp::TerminalError> {
        let request = faktor_server::native::terminal_authority::TerminalSpawnRequest {
            command: spec.command.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            rows: spec.rows,
            cols: spec.cols,
        };
        let handle = self
            .registry
            .create(session_id, &request)
            .map_err(map_terminal_registry_error)?;
        Ok(Arc::new(DaemonTerminalHandle { handle }))
    }
}

/// One live daemon terminal behind the ACP [`faktor_acp::TerminalHandle`]
/// trait (delegation only; the authority owns the process).
pub(crate) struct DaemonTerminalHandle {
    pub(crate) handle: faktor_server::native::terminal_authority::TerminalHandle,
}

impl faktor_acp::TerminalHandle for DaemonTerminalHandle {
    fn terminal_id(&self) -> &str {
        self.handle.terminal_id()
    }
    fn pid(&self) -> u32 {
        self.handle.pid()
    }
    fn is_alive(&self) -> bool {
        self.handle.is_alive()
    }
    fn ownership_id(&self) -> &str {
        self.handle.ownership_id()
    }
    fn write(&self, bytes: &[u8]) -> Result<(), faktor_acp::TerminalError> {
        self.handle
            .write(bytes)
            .map_err(map_terminal_registry_error)
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<(), faktor_acp::TerminalError> {
        self.handle
            .resize(rows, cols)
            .map_err(map_terminal_registry_error)
    }
    fn drain_output(&self) -> Vec<u8> {
        self.handle.drain_output()
    }
    fn kill(&self) -> Result<(), faktor_acp::TerminalError> {
        self.handle.kill().map_err(map_terminal_registry_error)
    }
}
