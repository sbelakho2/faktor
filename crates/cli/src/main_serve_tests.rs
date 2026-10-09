//! `main_serve_tests`: out-of-line slice of the CLI test module.

use super::*;

use faktor_core::CancellationToken;

use faktor_core::{Capability, CapabilitySet, OpId};

use faktor_provider::testing::{sse_body, MockAction, MockServer};

use faktor_provider::{
    ContentPart, GenericAgentRequest, ProviderChunk, ProviderError, RequestMessage, RequestMeta,
    Role, ToolSpec,
};

use futures::StreamExt;

use std::pin::Pin;

/// Permission requester that never blocks on a UI (text-only turns never
/// ask, but AgentDeps requires one deterministically).
pub(crate) struct AlwaysAllow;

impl faktor_agent::PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _session: SessionId,
        _permission: &faktor_session::PermissionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = faktor_core::Result<faktor_core::capability::PermissionDecision>,
                > + Send,
        >,
    > {
        Box::pin(async { Ok(faktor_core::capability::PermissionDecision::Allow) })
    }
}

/// Minimal REAL daemon AgentDeps over an open session manager: text-only
/// turns, no MCP/verifier/supervisor (nothing here ever runs a process).
pub(crate) fn test_agent(
    session: Arc<SessionManager>,
    registry: ProviderRegistry,
) -> Arc<AgentRuntime> {
    let cas = session.cas();
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(ToolRegistry::new()),
        cas: Some(cas),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: daemon_instructions_resolver(&session),
        // Test graph: the passthrough pin (session-configured
        // provider/model win) + the REAL durable ledger over this
        // session manager (reservations ride the tempdir store).
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        model: "default".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are Faktor.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    };
    AgentRuntime::new(deps).unwrap()
}

#[test]
fn serve_config_is_strict_only_for_an_explicit_path() {
    // Audit 31: without --config, defaults (nothing can fail startup);
    // with an explicit --config, parse+validation failures are startup
    // errors — never a silent fallback to defaults.
    assert_eq!(
        serve_config(None).unwrap().model,
        config::Config::default().model,
        "no --config stays lenient"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("serve.json");
    std::fs::write(&path, "{not json").unwrap();
    let e =
        serve_config(Some(path.clone())).expect_err("an explicit broken config must fail startup");
    assert!(e.contains("serve.json"), "{e}");
    // Unknown fields and duplicate provider ids fail strict too.
    std::fs::write(&path, r#"{"model": "m", "surprise": 1}"#).unwrap();
    assert!(serve_config(Some(path.clone())).is_err());
    std::fs::write(
        &path,
        r#"{"providers": [
                {"kind": "ollama", "id": "twice", "base_url": null},
                {"kind": "open_ai", "id": "twice", "base_url": "http://x"}
            ]}"#,
    )
    .unwrap();
    let e = serve_config(Some(path.clone())).expect_err("duplicate ids fail strict");
    assert!(e.contains("twice"), "{e}");
    // A healthy explicit config still loads.
    std::fs::write(
        &path,
        r#"{"config_version": 1, "model": "m", "providers": [
                {"kind": "ollama", "id": "o", "base_url": null}
            ]}"#,
    )
    .unwrap();
    assert_eq!(serve_config(Some(path)).unwrap().model, "m");
}

#[test]
fn semantic_registry_arc_is_the_one_authority_for_agent_and_server() {
    // Audit 83 lock: the graph builds the semantic registry EXACTLY
    // once; the SAME Arc flows to AgentDeps and to ServerDeps (the
    // exact serve_impl assembly), so the native introspection surface
    // can never report a parallel registry.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let session = SessionManager::open(data.join("store"), data.join("cas"), true).unwrap();
    let supervisor = ProcessSupervisor::new(session.cas());
    let semantic = graph::SemanticCfg {
        providers: vec![faktor_semantic::SemanticProviderConfig::Process {
            id: faktor_semantic::SemanticProviderId::parse("graph-proc").unwrap(),
            command: "/bin/true".to_string(),
            args: vec![],
            timeout_ms: 1_000,
        }],
        ..Default::default()
    };
    let graph = build_daemon_core(
        &data,
        session,
        supervisor,
        config::Config::default(),
        vec![],
        None,
        semantic,
        None,
        GithubAppSeams::default(),
    )
    .expect("daemon core builds with a configured semantic provider");
    assert_eq!(graph.semantic.providers().len(), 1);
    assert!(
        Arc::ptr_eq(graph.agent.semantic_registry(), &graph.semantic),
        "the agent must hold the graph's semantic Arc"
    );
    // The ServerDeps assembly mirrors serve_impl exactly.
    let mut deps = ServerDeps::new_with(
        graph.session.clone(),
        graph.agent.clone(),
        graph.permissions.clone(),
        graph.orchestrator.clone(),
        graph.tasks.clone(),
        graph.budgets.clone(),
    );
    deps = deps.with_semantic_registry(graph.semantic.clone());
    let served = deps.semantic.as_ref().expect("semantic wired");
    assert!(
        Arc::ptr_eq(served, graph.agent.semantic_registry()),
        "the native surface (deps.semantic) and the agent must share ONE Arc"
    );
    assert_eq!(served.providers()[0].id().as_str(), "graph-proc");
}

#[test]
fn daemon_instructions_resolver_uses_durable_session_roots_only() {
    // P0-32: the daemon resolver must never invent a root — loading
    // repository rules at the wrong root would silently misapply them.
    // Resolution goes through the SessionManager workspace table only:
    // an unknown/rootless workspace is an Empty set (documented), and a
    // durable workspace root serves its AGENTS.md with live-epoch
    // semantics (rewrite -> new epoch/content; pinned old epoch serves
    // the cached old tree).
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let resolver = daemon_instructions_resolver(&session);
    // Unknown workspace id: Empty, never an error, never the CWD.
    assert!(resolver.resolve(999, None).unwrap().is_empty());
    // A durable workspace root serves the tree at that root.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "always: durable daemon rules\n").unwrap();
    let ws = session.create_workspace(repo.to_str().unwrap()).unwrap();
    let loaded = resolver.resolve(ws.raw(), None).unwrap();
    assert!(loaded
        .active_for("anything", &[])
        .iter()
        .any(|i| i.content.contains("durable daemon rules")));
    let e1 = loaded.epoch().unwrap();
    // A rewrite moves the epoch and the served content; the pinned old
    // epoch still sees the old content through the cache.
    std::fs::write(repo.join("AGENTS.md"), "always: rewritten daemon rules\n").unwrap();
    let v2 = resolver.resolve(ws.raw(), None).unwrap();
    assert_ne!(v2.epoch().unwrap(), e1);
    assert!(v2
        .active_for("x", &[])
        .iter()
        .any(|i| i.content.contains("rewritten daemon rules")));
    let pinned = resolver.resolve(ws.raw(), Some(e1)).unwrap();
    assert!(
        pinned
            .active_for("x", &[])
            .iter()
            .any(|i| i.content.contains("durable daemon rules")),
        "a pinned old epoch must still serve the old tree"
    );
}

#[test]
fn daemon_startup_recovery_requeues_stale_running_jobs_only() {
    // Audit P0-5/26 production wiring: the startup sweep re-queues the
    // stale Running row a dead executor left behind and NEVER touches a
    // terminal row.
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap(),
    );
    let ws = manager.create_workspace("/w").unwrap();
    let sid = manager
        .create_session(ws, "recover", "fake", "m")
        .unwrap()
        .id();
    let handle = manager.get_session(sid).unwrap().unwrap();
    let task = handle.task_id().unwrap();
    let op = manager.try_next_op_id().unwrap().raw();
    let check = |id: &str| faktor_session::VerificationAttemptCheck {
        check_id: id.into(),
        command: format!("make {id}"),
        inline: None,
    };
    let job = |id: &str| faktor_session::VerificationJobInput {
        check_id: id.into(),
        kind: "test".into(),
        command: format!("make {id}"),
        program: "make".into(),
        args: vec![id.into()],
        spec_json: "{}".into(),
        budget_ms: 10_000,
    };
    handle
        .begin_verification_attempt(
            task.raw(),
            1,
            op,
            "/w",
            &[],
            &[check("make_test"), check("make_check")],
            &[job("make_test"), job("make_check")],
        )
        .unwrap();
    // One executor died mid-check (Running); one finished (Passed).
    handle
        .claim_verification_job(
            task.raw(),
            "make_test",
            op,
            manager.try_next_op_id().unwrap().raw(),
        )
        .unwrap();
    handle
        .claim_verification_job(
            task.raw(),
            "make_check",
            op,
            manager.try_next_op_id().unwrap().raw(),
        )
        .unwrap();
    handle
        .resolve_verification_job(
            task.raw(),
            "make_check",
            op,
            faktor_session::VerificationJobState::Passed,
            None,
            Some("{}".into()),
        )
        .unwrap();
    recover_verification_jobs_at_startup(&manager);
    let jobs = handle.verification_attempt_jobs(task.raw(), op).unwrap();
    let stale = jobs
        .iter()
        .find(|j| j.check_id == "make_test")
        .expect("stale row");
    assert_eq!(stale.state, faktor_session::VerificationJobState::Queued);
    assert!(stale.note.as_deref().unwrap().contains("restart"));
    let done = jobs
        .iter()
        .find(|j| j.check_id == "make_check")
        .expect("terminal row");
    assert_eq!(done.state, faktor_session::VerificationJobState::Passed);
}

#[test]
fn daemon_instructions_resolver_reads_the_live_shadow_of_a_shadowed_workspace() {
    // P0-48: while exactly ONE session of the workspace carries a live
    // shadow row, the resolver's rules come from the SHADOW root (the
    // drive reads the instruction environment of the world it mutates);
    // with no live shadow (or after retirement) the stored workspace
    // root serves byte-identically; ambiguity is a loud degrade.
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let resolver = daemon_instructions_resolver(&session);
    let repo = dir.path().join("repo");
    let shadow = dir.path().join("shadows").join("1").join("sh-x");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&shadow).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "always: user-checkout rules\n").unwrap();
    std::fs::write(shadow.join("AGENTS.md"), "always: shadow-world rules\n").unwrap();
    let ws = session.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = session.create_session(ws, "t", "fake", "m").unwrap();
    let loaded = resolver.resolve(ws.raw(), None).unwrap();
    assert!(loaded
        .active_for("anything", &[])
        .iter()
        .any(|i| i.content.contains("user-checkout rules")));
    // Begin a durable shadow (the exact row the ShadowRoots service
    // writes): the workspace's rules now resolve from the shadow.
    let row = faktor_session::ShadowRow {
        session_id: handle.id().raw(),
        shadow_id: "sh-x".into(),
        base_root: repo.to_str().unwrap().into(),
        root: shadow.to_str().unwrap().into(),
        state: faktor_session::ShadowRowState::Active,
        base_entries: 1,
        base_bytes: 1,
        created_ms: session.now_ms(),
    };
    session.put_shadow_row(handle.id(), &row).unwrap();
    let shadowed = resolver.resolve(ws.raw(), None).unwrap();
    assert!(
        shadowed
            .active_for("anything", &[])
            .iter()
            .any(|i| i.content.contains("shadow-world rules")),
        "the live shadow re-points instruction loading"
    );
    // Retire the shadow: the stored root is authoritative again.
    let mut retired = row;
    retired.state = faktor_session::ShadowRowState::Integrated;
    session.put_shadow_row(handle.id(), &retired).unwrap();
    let back = resolver.resolve(ws.raw(), None).unwrap();
    assert!(back
        .active_for("anything", &[])
        .iter()
        .any(|i| i.content.contains("user-checkout rules")));
    // Two live shadows on one workspace: ambiguous — a TYPED refusal
    // (authority law), never a guessed root and never a silent degrade to
    // the stored tree.
    let other = session.create_session(ws, "other", "fake", "m").unwrap();
    let mut other_row = faktor_session::ShadowRow {
        session_id: other.id().raw(),
        shadow_id: "sh-y".into(),
        base_root: repo.to_str().unwrap().into(),
        root: shadow.to_str().unwrap().into(),
        state: faktor_session::ShadowRowState::Active,
        base_entries: 1,
        base_bytes: 1,
        created_ms: session.now_ms(),
    };
    session.put_shadow_row(other.id(), &other_row).unwrap();
    let mut revived = retired;
    revived.state = faktor_session::ShadowRowState::IntegrationBlocked;
    session.put_shadow_row(handle.id(), &revived).unwrap();
    other_row.state = faktor_session::ShadowRowState::Active;
    let err = resolver.resolve(ws.raw(), None).unwrap_err();
    assert!(
        matches!(err, faktor_instructions::RulesLoadError::Unreadable(_)),
        "ambiguous shadows must be a typed refusal: {err:?}"
    );
    let detail = err.to_string();
    assert!(
        detail.contains("unavailable") && detail.contains("ambiguous"),
        "the refusal names the ambiguous authority: {detail}"
    );
}

/// Write one fake complete backup `faktor-plus-{ts_ms}.db` of `size`
/// bytes with an explicit mtime (deterministic ordering for gate and
/// retention tests).
pub(crate) fn write_backup(
    dir: &std::path::Path,
    ts_ms: u64,
    mtime: std::time::SystemTime,
    size: u64,
) -> std::path::PathBuf {
    let p = dir.join("backups").join(format!("faktor-plus-{ts_ms}.db"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, vec![0u8; size as usize]).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_modified(mtime).unwrap();
    p
}

#[test]
fn backup_gate_skips_a_fresh_matching_snapshot_and_honors_staleness_and_size() {
    let hour = std::time::Duration::from_secs(3600);
    // Fresh store for each scenario so mtime ordering stays unambiguous.
    // (a) No backups at all: due.
    {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        drop(session);
        assert!(backup_due(dir.path()), "no backups: startup backup is due");
    }
    // (b) A fresh backup whose size matches the store: NOT due (audit 44
    // interval gate — rapid restarts must not pile hourly snapshots).
    {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
            .unwrap()
            .len();
        drop(session);
        write_backup(dir.path(), 1, std::time::SystemTime::now(), db_len);
        assert!(
            !backup_due(dir.path()),
            "fresh same-size backup must gate the snapshot"
        );
    }
    // (c) Same size but old: due.
    {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
            .unwrap()
            .len();
        drop(session);
        write_backup(
            dir.path(),
            1,
            std::time::SystemTime::now() - 2 * hour,
            db_len,
        );
        assert!(backup_due(dir.path()), "old snapshot is stale: due");
    }
    // (d) Fresh but the store RESIZED since the snapshot (crash-recovery
    // wrote): due even inside the interval.
    {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
            .unwrap()
            .len();
        drop(session);
        write_backup(
            dir.path(),
            1,
            std::time::SystemTime::now() - std::time::Duration::from_secs(600),
            db_len - 1,
        );
        assert!(
            backup_due(dir.path()),
            "a resized store makes a fresh snapshot stale: due"
        );
    }
}

#[test]
fn rotate_backup_enforces_the_count_quota_and_keeps_the_newest() {
    // 10 old backups + the new snapshot = 11 candidates; retention must
    // drop the OLDEST until BACKUP_MAX_FILES (8) remain — never the
    // snapshot just written.
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let store = session.store();
    let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
        .unwrap()
        .len();
    let now = std::time::SystemTime::now();
    for i in 0..10u64 {
        write_backup(
            dir.path(),
            1000 + i,
            now - std::time::Duration::from_secs((i + 1) * 3600),
            100,
        );
    }
    rotate_backup(&store, dir.path());
    let files = list_backups(dir.path());
    assert_eq!(
        files.len(),
        BACKUP_MAX_FILES,
        "11 candidates must be rotated down to {BACKUP_MAX_FILES}"
    );
    // The newest survivor is the snapshot just written (matches the
    // store size; the seeded fakes are 100 bytes).
    let newest_len = std::fs::metadata(&files[0]).unwrap().len();
    assert_eq!(newest_len, db_len, "the fresh snapshot must survive");
    // No interrupted-writer temp files are ever listed or kept.
    assert!(files.iter().all(|p| !p.to_string_lossy().contains(".tmp-")));
}

#[test]
fn backup_finalize_is_one_atomic_adoption_and_never_lists_partial_temp() {
    // A crashed writer's `.db.tmp-*` is invisible to the gate/retention
    // scans; the finalize step publishes it whole (fsync + rename +
    // directory fsync through the shared authority) and consumes it.
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let store = session.store();
    // Seed a fresh same-size fake so the gate would otherwise skip.
    let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
        .unwrap()
        .len();
    write_backup(dir.path(), 1, std::time::SystemTime::now(), db_len);
    let backups = dir.path().join("backups");
    // Crash residue: a partially written snapshot under the temp name.
    let tmp = backups.join(format!("faktor-plus-999.db.tmp-{}", std::process::id()));
    std::fs::write(&tmp, b"partial sqlite pages").unwrap();
    assert!(
        !list_backups(dir.path()).iter().any(|p| p == &tmp),
        "an in-progress temp is never a complete backup"
    );
    // The atomic adoption publishes it and consumes the temp.
    let dest = backups.join("faktor-plus-999.db");
    faktor_fs::atomic::atomic_adopt(&tmp, &dest).unwrap();
    assert!(!tmp.exists(), "the adopted temp is renamed, not copied");
    assert_eq!(std::fs::read(&dest).unwrap(), b"partial sqlite pages");
    assert!(list_backups(dir.path()).iter().any(|p| p == &dest));
    // A missing temp fails loudly and never mints a destination.
    let ghost = backups.join("faktor-plus-1000.db.tmp-ghost");
    assert!(faktor_fs::atomic::atomic_adopt(&ghost, &backups.join("faktor-plus-1000.db")).is_err());
    assert!(!backups.join("faktor-plus-1000.db").exists());
    drop(store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_line_precedes_backup_and_the_interval_gate_skips_a_fresh_snapshot() {
    // Audit 44: serve must print the startup line BEFORE any backup file
    // exists, and when the interval gate says skip, no backup may appear
    // even after the delayed backup task would have fired.
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
        .unwrap()
        .len();
    drop(session);
    // Seed a fresh backup matching the store: the gate says skip.
    write_backup(dir.path(), 1, std::time::SystemTime::now(), db_len);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(0, dir2, None, Some(ready_tx), Some(shutdown_rx)).await
    });
    // The startup line is printed (readiness): no backup has run yet.
    tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");
    // Wait past the post-ready delay + margin: the gate must still skip.
    tokio::time::sleep(BACKUP_START_DELAY + std::time::Duration::from_millis(500)).await;
    assert_eq!(
        list_backups(dir.path()).len(),
        1,
        "the interval gate must skip the backup on a fresh snapshot"
    );
    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(10), daemon)
        .await
        .expect("daemon must stop on shutdown")
        .expect("serve_impl returns Ok")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_backup_runs_only_after_readiness_when_due() {
    // With no existing backup the startup backup IS due, but it must run
    // strictly AFTER the startup line: at readiness no backup file
    // exists yet; it appears only after the post-ready delay.
    let dir = tempfile::tempdir().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(0, dir2, None, Some(ready_tx), Some(shutdown_rx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");
    assert!(
        list_backups(dir.path()).is_empty(),
        "startup line must precede any backup file"
    );
    // The delayed task then writes exactly one snapshot.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if list_backups(dir.path()).len() == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gated backup task must run after readiness"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(10), daemon)
        .await
        .expect("daemon must stop on shutdown")
        .expect("serve_impl returns Ok")
        .unwrap();
}

/// Serializes the real-signal tests: SIGTERM/SIGINT are delivered to the
/// WHOLE process, so concurrent signal tests would cross-consume each
/// other's signals (or worse, race an unarmed waiter).
pub(crate) static SIGNAL_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Wait until `count` shutdown-signal waiters are armed (each successful
/// SIGTERM/SIGINT registration increments the counter). Bounded.
pub(crate) async fn wait_for_armed_signal_waiters(count: u32) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst) < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the shutdown-signal waiter was never armed (armed \
             {})",
            SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst)
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn send_self_signal(signal: i32) {
    // SAFETY: `kill(2)` with our own pid and a valid signal number; the
    // signal tests install the tokio handlers BEFORE sending.
    unsafe {
        libc::kill(libc::getpid(), signal);
    }
}

/// Production serve has NO shutdown channel: SIGTERM must trigger the
/// SAME full drain (`shutdown_serving_daemon`: worker-plane join, index
/// worker join, drive drain, backup drain) and return cleanly — proving
/// the production path is no longer `std::future::pending()`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_sigterm_runs_the_full_drain_and_returns_clean() {
    let _guard = SIGNAL_TEST_GUARD.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let baseline = SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon =
        tokio::task::spawn(async move { serve_impl(0, dir2, None, Some(ready_tx), None).await });
    tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");
    wait_for_armed_signal_waiters(baseline + 1).await;
    send_self_signal(libc::SIGTERM);
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
        .await
        .expect("SIGTERM must trigger the bounded drain, never kill the process")
        .expect("the serve task must not panic");
    assert!(
        result.is_ok(),
        "the drained daemon returns cleanly: {result:?}"
    );
}

/// The second signal during the drain takes the FORCE path: it must
/// report the force-exit code (the test probe replaces `process::exit`,
/// which would kill the harness) while the first signal's bounded drain
/// still completes cleanly.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_signal_during_the_drain_forces_exit_through_the_probe() {
    let _guard = SIGNAL_TEST_GUARD.lock().await;
    SIGNAL_DRAIN_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let (probe_tx, probe_rx) = tokio::sync::oneshot::channel();
    *FORCE_EXIT_PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe_tx);
    let dir = tempfile::tempdir().unwrap();
    let baseline = SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon =
        tokio::task::spawn(async move { serve_impl(0, dir2, None, Some(ready_tx), None).await });
    tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");
    wait_for_armed_signal_waiters(baseline + 1).await;
    send_self_signal(libc::SIGTERM);
    // Wait until the drain was entered (the watchdog is then spawned)
    // and its own signal waiter is armed before escalating.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !SIGNAL_DRAIN_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first signal never reached the drain"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    wait_for_armed_signal_waiters(baseline + 2).await;
    send_self_signal(libc::SIGTERM);
    let code = tokio::time::timeout(std::time::Duration::from_secs(10), probe_rx)
        .await
        .expect("the second signal must take the force path")
        .expect("the force probe must fire");
    assert_eq!(code, FORCE_EXIT_CODE);
    // The probe replaced the process exit: the first signal's drain
    // still completes cleanly in-process.
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
        .await
        .expect("the drain must still finish")
        .expect("the serve task must not panic");
    assert!(
        result.is_ok(),
        "the drained daemon returns cleanly: {result:?}"
    );
}

/// The forced-exit path is reachable through the probe even without a
/// serve task (classification seam).
#[cfg(unix)]
#[tokio::test]
async fn force_signal_reports_the_force_exit_code() {
    let _guard = SIGNAL_TEST_GUARD.lock().await;
    let (probe_tx, probe_rx) = tokio::sync::oneshot::channel();
    *FORCE_EXIT_PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe_tx);
    on_force_signal("SIGTERM").await;
    assert_eq!(probe_rx.await.unwrap(), FORCE_EXIT_CODE);
}

/// Serve startup queue recovery: a durable pending queue head left by a
/// killed process (active turn already aborted, no live runner anywhere)
/// is claimed by EXACTLY ONE runner as part of the serve startup
/// sequence, with no new submit. The startup-wiring harness configures no
/// provider (scripted providers are not a config surface), so the claimed
/// head's turn fails closed on the missing model — exactly once; real
/// end-to-end execution of a recovered head is proven one layer down
/// (`task_executor_tests::relaunch_recovery_drains_a_pending_head_without_a_new_submit`,
/// which asserts the provider call count is exactly one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_startup_drains_a_pending_durable_queue_head_without_a_submit() {
    use faktor_core::event::EventKind;
    let dir = tempfile::tempdir().unwrap();
    // The kill residue: A is the active turn and B queues durably behind
    // it. Neither is ever driven; A is aborted so the machine is idle and
    // the pending head B is exactly what startup recovery must find.
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let ws = session.create_workspace("/w").unwrap();
    let sid = session
        .create_session(ws, "queue recovery", "fake", "m")
        .unwrap()
        .id();
    let agent = test_agent(session.clone(), ProviderRegistry::new());
    let receipt_a = agent.submit(sid, "A prompt", &[]).unwrap();
    let receipt_b = agent.submit(sid, "B prompt", &[]).unwrap();
    assert!(receipt_b.queued, "B queues behind A");
    agent.abort_op(sid, Some(receipt_a.op_id)).unwrap();
    let handle = session.get_session(sid).unwrap().unwrap();
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        1,
        "B survives the kill durably"
    );

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let data_dir = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(0, data_dir, None, Some(ready_tx), Some(shutdown_rx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");

    // Startup recovery hands the head to exactly one runner: the durable
    // row leaves the non-terminal set, with no submit after the seed.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while handle.queued_prompt_count().unwrap() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "startup recovery must drain the pending head"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // Grace window for any duplicate runner to show itself.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        0,
        "the head stays drained"
    );
    let counts = handle.queue_status_counts().unwrap();
    for status in ["pending", "claimed", "running"] {
        assert_eq!(counts.get(status), None, "queue row is terminal: {counts}");
    }
    let events = handle.events_range(1, None).unwrap();
    let count = |kind: EventKind| {
        events
            .iter()
            .filter(|e| e.kind == kind && e.op_id == Some(receipt_b.op_id))
            .count()
    };
    assert_eq!(
        count(EventKind::PromptReceived),
        1,
        "B was queued exactly once"
    );
    assert_eq!(
        count(EventKind::PromptAdmitted),
        1,
        "B was claimed exactly once"
    );
    let terminal = events
        .iter()
        .filter(|e| {
            e.op_id == Some(receipt_b.op_id)
                && matches!(e.kind, EventKind::TurnCompleted | EventKind::Failed)
        })
        .count();
    assert_eq!(terminal, 1, "B's one admitted turn ended exactly once");

    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
        .await
        .expect("daemon must stop on shutdown")
        .expect("serve_impl returns Ok")
        .unwrap();
}

/// Kill-relaunch integration: a FIRST full daemon (real graph, real
/// executor) leaves B durably queued behind an aborted A, then its drive
/// registry is shut down exactly like a process death. The relaunched
/// `serve` startup path must claim B exactly once (one admitted turn),
/// with no new submit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_startup_recovery_drains_a_killed_daemons_queue_head_on_relaunch() {
    use faktor_core::event::EventKind;
    let dir = tempfile::tempdir().unwrap();
    // First daemon process: real graph, A active, B durably queued behind
    // it, neither driven.
    let graph = build_daemon_with_mcp_and_chunks_fast(
        dir.path(),
        None,
        None,
        graph::SemanticCfg::default(),
    )
    .await
    .unwrap();
    let session = graph.session.clone();
    let ws = session.create_workspace("/w").unwrap();
    let sid = session
        .create_session(ws, "relaunch recovery", "fake", "m")
        .unwrap()
        .id();
    let receipt_a = graph.agent.submit(sid, "A prompt", &[]).unwrap();
    let receipt_b = graph.agent.submit(sid, "B prompt", &[]).unwrap();
    assert!(receipt_b.queued, "B queues behind A");
    // Kill: the drive registry is closed (every later settle kick is
    // refused), then the active turn is aborted so A is not resumed and
    // B's durable row is the only runnable marker left.
    let report = graph
        .tasks
        .shutdown_drives(std::time::Duration::from_millis(50))
        .await;
    assert!(
        report.all_reaped(),
        "the killed daemon reaped its drives: {report:?}"
    );
    graph.agent.abort_op(sid, Some(receipt_a.op_id)).unwrap();
    assert_eq!(
        session
            .get_session(sid)
            .unwrap()
            .unwrap()
            .queued_prompt_count()
            .unwrap(),
        1,
        "B survives the kill durably"
    );
    drop(graph);

    // Relaunch through the REAL serve startup path; no new submit.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let data_dir = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(0, data_dir, None, Some(ready_tx), Some(shutdown_rx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");
    // Observe the relaunched directory through a fresh manager (the
    // killed one is gone): the head drains with exactly one claim.
    let observer =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let handle = observer.get_session(sid).unwrap().unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while handle.queued_prompt_count().unwrap() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "relaunch recovery must drain the killed head"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(handle.queued_prompt_count().unwrap(), 0);
    let events = handle.events_range(1, None).unwrap();
    let admitted = events
        .iter()
        .filter(|e| e.kind == EventKind::PromptAdmitted && e.op_id == Some(receipt_b.op_id))
        .count();
    assert_eq!(admitted, 1, "the relaunched daemon claimed B exactly once");
    let terminal = events
        .iter()
        .filter(|e| {
            e.op_id == Some(receipt_b.op_id)
                && matches!(e.kind, EventKind::TurnCompleted | EventKind::Failed)
        })
        .count();
    assert_eq!(terminal, 1, "B's one admitted turn ended exactly once");
    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
        .await
        .expect("daemon must stop on shutdown")
        .expect("serve_impl returns Ok")
        .unwrap();
}

/// F9 (adversarial): a config the DAEMON refuses (semantic validation
/// failure) must never be reported as an enabled worker plane — the
/// doctor runs serve's own load+validate path and reports the refused
/// state with an issue.
#[test]
fn doctor_refuses_a_config_the_daemon_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let config = dir.path().join("config.json");
    // Duplicate provider ids PARSE but `Config::validate` refuses them:
    // serve exits with a config error, so the doctor must refuse too.
    std::fs::write(
            &config,
            r#"{"model": "m", "providers": [{"kind": "ollama", "id": "dup"}, {"kind": "ollama", "id": "dup"}], "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}, "worker_plane": {"enabled": true}}"#,
        )
        .unwrap();
    let report = doctor_run_with_config(dir.path(), false, Some(&config));
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.starts_with("worker plane: FAILED")
                && line.contains("state=refused")
                && line.contains("config_refused")),
        "the refused config names the refused state: {:?}",
        report.lines
    );
    assert!(
        !report
            .lines
            .iter()
            .any(|line| line.starts_with("worker plane: enabled")),
        "a config the daemon refuses must never report the plane as enabled: {:?}",
        report.lines
    );
    assert!(report.issues >= 1, "{:?}", report.lines);
}

/// The additive `cloud-db` doctor section (P1 durability): a real
/// control-plane database is reported with its writer-recorded
/// `synchronous=FULL` policy, integrity, verified backup age and its
/// SELF-CONSISTENT migration restore point (fresh creation writes a
/// verified `-pre-migration-v0-` point); the LOCAL-authority caveat is
/// printed.
#[test]
fn doctor_reports_the_commercial_db_section_with_policy_and_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    drop(faktor_cloud::SqliteControlPlaneStore::open(&path).unwrap());
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "healthy: {:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("LOCAL commercial deployment authority")
                && l.starts_with("cloud-db:")),
        "{:?}",
        report.lines
    );
    let section = report
        .lines
        .iter()
        .find(|l| l.starts_with("cloud-db control-plane.db:"))
        .expect("the section names the database");
    assert!(section.contains("synchronous=FULL"), "{section}");
    assert!(section.contains("journal_mode=wal"), "{section}");
    assert!(section.contains("integrity=ok"), "{section}");
    assert!(report
        .lines
        .iter()
        .any(|l| l.contains("last verified backup") && l.contains("control-plane")));
    assert!(report
        .lines
        .iter()
        .any(|l| l.contains("migration restore point") && l.contains("-pre-migration-v0-")));
}

/// Doctor fails loudly (naming the DB) when the newest verified rotating
/// backup is older than the threshold: a backup that old no longer bounds
/// the loss window.
#[test]
fn doctor_fails_loudly_on_a_stale_rotating_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    drop(faktor_cloud::SqliteControlPlaneStore::open(&path).unwrap());
    let (backup, _) = faktor_cloud::durability::latest_backup(&path)
        .expect("the first open writes a verified rotating backup");
    let aged = std::time::SystemTime::now()
        - std::time::Duration::from_secs(faktor_cloud::durability::BACKUP_MAX_AGE_SECS + 60);
    std::fs::File::options()
        .write(true)
        .open(&backup)
        .unwrap()
        .set_modified(aged)
        .unwrap();
    let report = doctor_run(dir.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db control-plane.db:") && l.contains("STALE")),
        "{:?}",
        report.lines
    );
}

/// Doctor reports the SCM / worker / updater stores with their
/// writer-recorded policy, integrity, a verified backup age and their
/// pre-migration restore point — the three DBs that previously appeared
/// as unmarked.
#[test]
fn doctor_reports_policy_backup_and_restore_points_for_the_three_stores() {
    let dir = tempfile::tempdir().unwrap();
    let scm_path = dir.path().join("scm.db");
    drop(faktor_scm::SqliteScmStore::open(&scm_path).unwrap());
    let worker_path = dir.path().join("workers.db");
    drop(faktor_worker::SqliteWorkerStore::open(&worker_path).unwrap());
    let updater_path = dir.path().join("update.db");
    drop(faktor_updater::SqliteUpdaterStore::open(&updater_path).unwrap());
    // Roll each cursor back one version and reopen: the reopen writes a
    // VERIFIED pre-migration restore point before migrating, exactly the
    // state doctor must surface.
    for (path, from_version) in [
        (&scm_path, 1i64),
        (&worker_path, 1i64),
        (&updater_path, 2i64),
    ] {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {from_version}"))
            .unwrap();
        drop(conn);
    }
    drop(faktor_scm::SqliteScmStore::open(&scm_path).unwrap());
    drop(faktor_worker::SqliteWorkerStore::open(&worker_path).unwrap());
    drop(faktor_updater::SqliteUpdaterStore::open(&updater_path).unwrap());
    let report = doctor_run(dir.path(), false);
    for rel in ["scm.db", "workers.db", "update.db"] {
        let section = report
            .lines
            .iter()
            .find(|l| l.starts_with(&format!("cloud-db {rel}:")))
            .unwrap_or_else(|| panic!("{rel} is reported: {:?}", report.lines));
        assert!(section.contains("journal_mode=wal"), "{section}");
        assert!(section.contains("synchronous=FULL"), "{section}");
        assert!(section.contains("integrity=ok"), "{section}");
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with(&format!("cloud-db {rel}: last verified backup"))),
            "{rel} backup age: {:?}",
            report.lines
        );
        assert!(
            report.lines.iter().any(|l| l
                .starts_with(&format!("cloud-db {rel}: migration restore point"))
                && l.contains("-pre-migration-v")),
            "{rel} restore point: {:?}",
            report.lines
        );
    }
    assert_eq!(report.issues, 0, "{:?}", report.lines);
}

// ------------------------------------------------------------- wiring

/// One chat request whose user text can echo configured secrets.
pub(crate) fn chat_req(text: &str) -> GenericAgentRequest {
    GenericAgentRequest {
        model: "m".into(),
        system: "sys".into(),
        messages: vec![RequestMessage {
            role: Role::User,
            content: vec![ContentPart::text(text)],
        }],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        max_output: Some(64),
        reasoning: None,
        stream: true,
        meta: RequestMeta {
            operation_id: OpId::new(1),
            session_id: SessionId::new(1),
            provider: "cli-wiring-test".into(),
            attempt: 0,
            deadline_ms: 10_000,
            cancellation: CancellationToken::new(),
        },
    }
}

/// Drive one chat call to completion; `Err` carries the provider error
/// (a policy/secret refusal arrives before any server contact).
pub(crate) async fn chat_text(
    provider: Arc<dyn Provider>,
    text: &str,
) -> Result<String, ProviderError> {
    let mut stream = provider.stream(chat_req(text));
    let mut out = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(ProviderChunk::Text { text: t }) => out.push_str(&t),
            Ok(ProviderChunk::Done) => break,
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// A config with ONE OpenAI-compatible provider at `base` and the given
/// sandbox network rows (`None` = no sandbox section = crate defaults).
pub(crate) fn egress_cfg(
    dir: &std::path::Path,
    file: &str,
    base: &str,
    key_env: Option<&str>,
    rows: Option<&[String]>,
) -> config::Config {
    let mut body = serde_json::json!({
        "model": "m",
        "providers": [{
            "kind": "open_ai",
            "id": "mocked",
            "base_url": base,
            "api_key_env": key_env,
            // The tests below target loopback mock servers, so the
            // entry carries the explicit loopback address-class rule.
            "allow_loopback": true,
        }],
    });
    if let Some(rows) = rows {
        body["sandbox"] = serde_json::json!({ "network": rows });
    }
    let path = dir.join(file);
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    config::Config::load(&path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_secret_registry_blocks_a_configured_key_echo_before_connect() {
    const KEY_ENV: &str = "FAKTOR_TEST_CLI_WIRING_FAKE_KEY";
    const SECRET: &str = "kp-cli-secret-token-91f7c2e8d4";
    // The value must not trip the frozen GENERIC scan patterns (sk-*,
    // ghp_, AKIA, ...): only the configured-secret registry can catch
    // it, so a block proves the registry was populated from the key.
    std::env::set_var(KEY_ENV, SECRET);
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_body(&[serde_json::json!({
                "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]
            })]),
        },
    );
    let base = server.base_url().await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = egress_cfg(
        dir.path(),
        "secret.json",
        &base,
        Some(KEY_ENV),
        Some(&[format!("http://127.0.0.1:{port}")]),
    );
    let graph = build_daemon(dir.path(), Some(cfg)).unwrap();
    let provider = graph.providers.get("mocked").unwrap();
    // Control: a clean body reaches the mock (the scan does not
    // false-positive on ordinary chat text).
    let text = chat_text(provider.clone(), "hello").await.unwrap();
    assert_eq!(text, "ok");
    assert_eq!(server.request_count(), 1);
    // The key echo: the whole payload is scanned and the request is
    // denied BEFORE any connect — the mock never sees it.
    let err = chat_text(provider, SECRET)
        .await
        .expect_err("a configured key echoed in the body must be blocked");
    assert!(err.message.contains("secret"), "{}", err.message);
    assert!(!err.retryable);
    assert_eq!(
        server.request_count(),
        1,
        "the key-echo request never arrived at the server"
    );
    drop(graph);
    std::env::remove_var(KEY_ENV);
}

#[test]
fn network_guarantee_required_flows_into_the_daemon_sandbox_gate() {
    // Authority direction (audit P0-39): the configured guarantee
    // reaches the daemon's PermissionEngine, which DECIDES the spawn
    // requirement (Required => DenyAll) and performs NO platform
    // pre-judgement. The capability verdict stays the plain rule (Ask
    // by default); enforcement honesty belongs to the spawn layer,
    // which refuses typed when it cannot isolate.
    let cfg = config::Config {
        sandbox: config::SandboxCfg {
            network_guarantee: faktor_sandbox::SandboxGuarantee::Required,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        cfg.sandbox_policy().unwrap().network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required
    );
    let dir = tempfile::tempdir().unwrap();
    let graph = build_daemon(dir.path(), Some(cfg)).unwrap();
    let sandbox = graph
        .agent
        .deps()
        .sandbox
        .clone()
        .expect("daemon sandbox wired");
    assert_eq!(
        sandbox.policy().network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required,
        "the guarantee surfaces into the daemon SandboxPolicy"
    );
    assert_eq!(
        sandbox.spawn_network_requirement(),
        faktor_terminal::NetworkIsolationRequirement::DenyAll,
        "Required must reach the spawn seam as DenyAll"
    );
    let decision = sandbox.evaluate(&Capability::ExecuteShell {
        command: "echo hi".into(),
    });
    assert_eq!(
        decision,
        faktor_core::PermissionDecision::Ask,
        "no preflight platform guessing: the rule decides (Ask default)"
    );
    drop(graph);
}

#[test]
fn daemon_hooks_run_env_clear_exact_through_the_daemon_supervisor() {
    // (b) cli-level hook harness: the daemon hook registry constructor
    // (supervisor-rooted, daemon envelope) runs a hook whose env is
    // cleared EXACTLY (only explicit entries + FAKTOR_HOOK_INPUT), and
    // the audit row lands with the bounded stdout.
    std::env::set_var("FAKTOR_TEST_DAEMON_ONLY_SECRET", "must-not-leak-to-hooks");
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let supervisor = ProcessSupervisor::new(session.cas());
    let spec = faktor_hooks::HookSpec {
        id: "env-0".into(),
        events: vec![faktor_hooks::HookEvent::PreTool],
        command: "/usr/bin/env".into(),
        args: vec![],
        env: vec![("FAKTOR_TEST_HOOK_VISIBLE".into(), "visible".into())],
        env_allowlist: true,
        deadline_ms: 5000,
        failure_policy: faktor_hooks::FailurePolicy::FailClosed,
        ..Default::default()
    };
    let registry = hook_registry(&supervisor, vec![spec]).expect("registry built");
    let verdict = registry.run(
        faktor_hooks::HookEvent::PreTool,
        &faktor_hooks::HookInput {
            workspace_root: Some(std::env::temp_dir()),
            ..Default::default()
        },
    );
    assert_eq!(verdict, faktor_hooks::HookVerdict::Allow);
    let audit = registry.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].hook_id, "env-0");
    let stdout = &audit[0].stdout_head;
    assert!(
        stdout.contains("FAKTOR_TEST_HOOK_VISIBLE=visible"),
        "explicit entries pass: {stdout}"
    );
    assert!(
        stdout.contains("FAKTOR_HOOK_INPUT="),
        "the input JSON rides the env: {stdout}"
    );
    assert!(
        !stdout.contains("FAKTOR_TEST_DAEMON_ONLY_SECRET"),
        "env-clear exact: the daemon env must never reach the hook: {stdout}"
    );
    // The run went through the supervisor registry and left nothing
    // alive (bounded child, reaped).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !supervisor.alive().is_empty() {
        assert!(std::time::Instant::now() < deadline, "child leaked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::env::remove_var("FAKTOR_TEST_DAEMON_ONLY_SECRET");
}

#[test]
fn daemon_hook_deadline_kills_the_group_and_audits_the_refusal() {
    // (b) deadline group-kill through the daemon supervisor: an
    // over-deadline hook is killed process-group-wide, its audit row
    // records the refusal (never the partial output), and no child is
    // left behind.
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let supervisor = ProcessSupervisor::new(session.cas());
    let spec = faktor_hooks::HookSpec {
        id: "slow".into(),
        events: vec![faktor_hooks::HookEvent::PreTool],
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 30".into()],
        env_allowlist: true,
        deadline_ms: 300,
        failure_policy: faktor_hooks::FailurePolicy::FailClosed,
        ..Default::default()
    };
    let registry = hook_registry(&supervisor, vec![spec]).expect("registry built");
    let verdict = registry.run(
        faktor_hooks::HookEvent::PreTool,
        &faktor_hooks::HookInput {
            workspace_root: Some(std::env::temp_dir()),
            ..Default::default()
        },
    );
    assert!(
        matches!(verdict, faktor_hooks::HookVerdict::Deny { .. }),
        "fail-closed deadline refusal: {verdict:?}"
    );
    let audit = registry.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].hook_id, "slow");
    assert_eq!(audit[0].verdict, "deny");
    assert!(
        audit[0].duration_ms >= 200,
        "the deadline dominated: {} ms",
        audit[0].duration_ms
    );
    assert!(
        audit[0].exit_code.is_none() && audit[0].stdout_head.is_empty(),
        "killed before output; partial output never decides: {:?}",
        audit[0]
    );
    // Group-kill: no live child survives the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !supervisor.alive().is_empty() {
        assert!(std::time::Instant::now() < deadline, "killed child leaked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn full_scope_env_hooks_run_under_the_daemon_envelope() {
    // The daemon envelope grants the full capability lattice: an env
    // hook whose typed scope is the TOP element still registers and
    // runs (construction proof of the with_supervisor envelope seam).
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let supervisor = ProcessSupervisor::new(session.cas());
    let spec = faktor_hooks::HookSpec {
        id: "full".into(),
        events: vec![faktor_hooks::HookEvent::TaskComplete],
        command: "/bin/echo".into(),
        args: vec!["done".into()],
        permission_scope: CapabilitySet::ALL,
        ..Default::default()
    };
    let registry = hook_registry(&supervisor, vec![spec]).expect("registry built");
    let verdict = registry.run(
        faktor_hooks::HookEvent::TaskComplete,
        &faktor_hooks::HookInput {
            workspace_root: Some(std::env::temp_dir()),
            ..Default::default()
        },
    );
    assert_eq!(verdict, faktor_hooks::HookVerdict::Allow);
    assert_eq!(registry.audit().len(), 1);
}
