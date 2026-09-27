//! `daemon::wiring`: cohesive slice of the daemon construction.

#![allow(unused_imports)]

use super::*;

/// Transform the strict `[cloud.github_app]` section into the executor's
/// completion-step SCM provider DURING daemon construction: the canonical
/// [`faktor_scm::GitHubApp`] adapter over the daemon's ONE checked transport
/// and durable `scm.db` store, wrapped by
/// [`faktor_scm::GitHubCompletionScm`] for the executor's native PR step.
///
/// Disabled parity: while the section is absent/disabled nothing is read and
/// nothing is installed — a contracted PR step then records the explicit
/// `native_pr_scm_not_configured` blocker (never a silent skip). Production
/// builds the RS256 token source from the operator-staged PKCS#8 key
/// payload; the external seams may substitute the token source and the
/// clock, but never the config.
pub(crate) fn wire_completion_scm(
    cloud: &config::CloudCfg,
    data_dir: &std::path::Path,
    transport: &Arc<dyn HttpTransport>,
    store: Option<Arc<dyn faktor_scm::ScmStore>>,
    seams: &GithubAppSeams,
    tasks: &Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
) -> Result<(), String> {
    let Some(app_cfg) = cloud.github_app.as_ref().filter(|app| app.enabled) else {
        return Ok(());
    };
    if !cloud.enabled {
        return Err("cloud github_app: requires [cloud] enabled".into());
    }
    app_cfg.validate()?;
    // An enabled github_app always rides an enabled [cloud] section, whose
    // store the core opened above; a missing one is a construction-order
    // violation, never a silently provider-less executor.
    let store =
        store.ok_or("cloud github_app: the enabled [cloud] section resolved no scm store")?;
    let organization = app_cfg.organization()?;
    let app_config = app_cfg.app_config()?;
    // The external transport seam: a fake-server transport in certification,
    // the daemon's ONE checked transport in production.
    let transport: Arc<dyn HttpTransport> =
        seams.transport.clone().unwrap_or_else(|| transport.clone());
    let clock: Arc<dyn faktor_scm::Clock> = seams
        .clock
        .clone()
        .unwrap_or_else(|| Arc::new(faktor_scm::SystemClock));
    let tokens: Arc<dyn faktor_scm::InstallationTokenSource> = match &seams.token_source {
        Some(tokens) => tokens.clone(),
        None => {
            let payload_root = cloud
                .payload_root(data_dir)
                .map_err(|e| format!("cloud github_app: {e}"))?;
            let payloads = crate::payload::PayloadDir::new(payload_root);
            let private_key_name = app_cfg
                .key_payload
                .as_deref()
                .ok_or("cloud github_app: an enabled section requires `private_key`")?;
            let private_key_pem = payloads
                .load_private_key_pem(private_key_name)
                .map_err(|e| format!("cloud github_app: {e}"))?;
            let mut token_config = faktor_scm::GitHubAppTokenConfig {
                app_id: app_cfg.app_id.unwrap_or(0),
                private_key_pkcs8_pem: private_key_pem.into(),
                api_base: app_config.api_base.clone(),
                user_agent: app_config.user_agent.clone(),
                ..Default::default()
            };
            token_config.max_attempts = token_config.max_attempts.max(1);
            Arc::new(
                faktor_scm::GitHubAppTokenSource::new(
                    token_config,
                    transport.clone(),
                    clock.clone(),
                )
                .map_err(|e| format!("cloud github_app: {e}"))?,
            )
        }
    };
    let app =
        faktor_scm::GitHubApp::new(app_config, transport.clone(), tokens, store.clone(), clock)
            .map_err(|e| format!("cloud github_app: {e}"))?;
    let completion_scm = faktor_scm::GitHubCompletionScm::new(Arc::new(app), store, organization)
        .map_err(|e| format!("cloud github_app: {e}"))?;
    tasks.set_completion_scm_provider(Some(Arc::new(completion_scm)));
    tracing::info!("github app completion scm enabled");
    Ok(())
}

/// Automatic-backup interval (audit 44): at most one snapshot per
/// `BACKUP_MIN_INTERVAL_SECS` of wall time — unless the newest backup no
/// longer matches the store's size, which means the store changed since the
/// snapshot was taken (a crash-recovery run counts).
pub(crate) const BACKUP_MIN_INTERVAL_SECS: u64 = 3600;

/// Retention quota (spec §24): keep at most this many complete backups…
pub(crate) const BACKUP_MAX_FILES: usize = 8;

/// …and at most this many bytes across the whole backups directory
/// (drop the oldest while either bound is exceeded).
pub(crate) const BACKUP_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Post-readiness delay before the startup backup task acts: the daemon is
/// announced and accepting connections well before any snapshot work starts.
pub(crate) const BACKUP_START_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Every COMPLETE backup under `<data_dir>/backups` (`faktor-plus-*.db`),
/// newest by mtime first. In-progress snapshots write under a `.db.tmp-*`
/// name and are published into place only when complete (one atomic
/// `faktor_fs::atomic::atomic_adopt`), so they are invisible here by
/// construction.
pub(crate) fn list_backups(data_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let backups = data_dir.join("backups");
    let Ok(files) = std::fs::read_dir(&backups) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = files
        .flatten()
        .map(|f| f.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("faktor-plus-") && name.ends_with(".db")
        })
        .collect();
    out.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    out.reverse();
    out
}

/// Interval + staleness gate: the startup backup is due when no backup
/// exists, when the newest is older than [`BACKUP_MIN_INTERVAL_SECS`], or
/// when the newest no longer matches the store file's size (the daemon
/// wrote since it was taken).
pub(crate) fn backup_due(data_dir: &std::path::Path) -> bool {
    let db_path = data_dir.join("store").join("faktor-plus.db");
    let Ok(db_meta) = std::fs::metadata(&db_path) else {
        return false;
    };
    let Some(newest) = list_backups(data_dir).into_iter().next() else {
        return true;
    };
    let meta = std::fs::metadata(&newest).ok();
    let stale = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(|m| {
            m.elapsed()
                .map(|e| e >= std::time::Duration::from_secs(BACKUP_MIN_INTERVAL_SECS))
                .unwrap_or(true)
        })
        .unwrap_or(true);
    let resized = meta.map(|m| m.len()).unwrap_or(0) != db_meta.len();
    stale || resized
}

/// Remove interrupted-backup temp files older than an hour (a crashed writer
/// can leave them behind; live writers are always younger). Best effort.
pub(crate) fn sweep_stale_backup_tmp(backups: &std::path::Path) {
    let Ok(files) = std::fs::read_dir(backups) else {
        return;
    };
    for f in files.flatten() {
        let p = f.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains(".db.tmp-") || !older_than(&p, std::time::Duration::from_secs(3600)) {
            continue;
        }
        let _ = std::fs::remove_file(&p);
    }
}

/// True when the file's mtime is at least `age` in the past (missing or
/// unreadable files are never "stale": fail closed).
pub(crate) fn older_than(p: &std::path::Path, age: std::time::Duration) -> bool {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|e| e > age)
        .unwrap_or(false)
}

/// Online backup with rotation (spec §24): one crash-safe snapshot per
/// daemon start the interval gate admits; retention keeps the newest
/// [`BACKUP_MAX_FILES`] and never more than [`BACKUP_MAX_TOTAL_BYTES`] total.
/// The snapshot is written to a `.db.tmp-*` name and published as ONE
/// atomic step through `faktor_fs::atomic::atomic_adopt` (fsync the temp,
/// rename into place, fsync the directory), so a crash mid-backup can never
/// leave a partial file that reads as a complete backup (and the
/// gate/retention scans never see one). Best effort — a backup failure
/// never stops the daemon.
pub(crate) fn rotate_backup(store: &faktor_store::Store, data_dir: &std::path::Path) {
    let backups = data_dir.join("backups");
    if std::fs::create_dir_all(&backups).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = backups.join(format!("faktor-plus-{ts}.db"));
    let tmp = backups.join(format!("faktor-plus-{ts}.db.tmp-{}", std::process::id()));
    if let Err(e) = store.backup_to(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("automatic backup failed: {e}");
        return;
    }
    if let Err(e) = faktor_fs::atomic::atomic_adopt(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("automatic backup finalize failed: {e}");
        return;
    }
    tracing::info!("automatic backup written to {}", dest.display());
    // Retention quota: drop the OLDEST files while the count exceeds
    // BACKUP_MAX_FILES or the total bytes exceed BACKUP_MAX_TOTAL_BYTES.
    // The just-written snapshot is newest and never a candidate.
    let files = list_backups(data_dir);
    let mut total: u64 = files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut kept = files.len();
    for victim in files.iter().rev() {
        let over_count = kept > BACKUP_MAX_FILES;
        let over_bytes = total > BACKUP_MAX_TOTAL_BYTES && kept > 1;
        if !over_count && !over_bytes {
            break;
        }
        if let Ok(m) = std::fs::metadata(victim) {
            total = total.saturating_sub(m.len());
        }
        if std::fs::remove_file(victim).is_err() {
            break;
        }
        kept -= 1;
    }
    // Opportunistic sweep of interrupted-writer debris from crashed runs.
    sweep_stale_backup_tmp(&backups);
}

/// Startup-backup task seam (P0-46): the async wrapper sleeps the
/// post-readiness delay, applies the interval/staleness gate, and runs the
/// SYNC snapshot+rotation on the blocking pool — never on a Tokio worker.
/// Returns the JoinHandle the shutdown path drains.
pub(crate) fn spawn_startup_backup(
    store: Arc<faktor_store::Store>,
    data_dir: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        tokio::time::sleep(BACKUP_START_DELAY).await;
        if backup_due(&data_dir) {
            let store = store.clone();
            let dir = data_dir.clone();
            // spawn_blocking: the SQLite backup API is synchronous and can
            // hold the caller's thread for the whole snapshot; a Tokio
            // worker must never sit in it (P0-46 worker starvation).
            if let Err(e) = tokio::task::spawn_blocking(move || rotate_backup(&store, &dir)).await {
                tracing::warn!("startup backup worker failed: {e}");
            }
        } else {
            tracing::info!(
                "startup backup skipped: a backup newer than {BACKUP_MIN_INTERVAL_SECS}s exists"
            );
        }
    })
}

/// Typed summary of the startup verification recovery sweep. Every count is
/// diagnostic and asserted by tests; a read failure is NEVER silently a
/// no-op — `unreadable`/`scan_failed` name the durable work left for the
/// next boot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerificationRecoverySummary {
    pub(crate) requeued: usize,
    pub(crate) orphaned: usize,
    pub(crate) touched: usize,
    /// Sessions whose durable verification rows could not be read (store
    /// error, or a listed session whose handle vanished): their rows are
    /// LEFT AS THEY ARE (durable work) and the failure is logged typed.
    pub(crate) unreadable: usize,
    /// The session scan itself failed: nothing could be swept this boot.
    pub(crate) scan_failed: bool,
}

/// Daemon startup verification recovery sweep (audit P0-5/26 production
/// wiring): every session's stale `Running` verification jobs are re-queued
/// before the executor starts, so a check whose executor died mid-run is
/// retried honestly — never silently dropped, never a pass. Runs BEFORE
/// readiness is announced, like every other crash-recovery step. A store
/// error is loud and leaves the durable rows for the next boot; it is never
/// reported as "nothing to recover".
pub(crate) fn recover_verification_jobs_at_startup(
    session: &Arc<faktor_session::SessionManager>,
) -> VerificationRecoverySummary {
    let mut summary = VerificationRecoverySummary::default();
    let ids = match session.store().session_ids() {
        Ok(ids) => ids,
        Err(e) => {
            summary.scan_failed = true;
            tracing::error!(
                error = %e,
                "verification recovery could not scan sessions; every durable verification row \
                 stays for the next boot: {e}"
            );
            return summary;
        }
    };
    for sid in ids {
        match session.get_session(sid) {
            Ok(Some(handle)) => match handle.recover_verification_jobs_after_restart() {
                Ok(report) if report.requeued + report.orphaned > 0 => {
                    summary.requeued += report.requeued;
                    summary.orphaned += report.orphaned;
                    summary.touched += 1;
                }
                Ok(_) => {}
                Err(e) => {
                    summary.unreadable += 1;
                    tracing::error!(
                        session = %sid,
                        error = %e,
                        "verification recovery for session {sid} could not read its durable rows; \
                         they stay for the next boot: {e}"
                    );
                }
            },
            Ok(None) => {
                summary.unreadable += 1;
                tracing::error!(
                    session = %sid,
                    "verification recovery found a listed session with no handle (store \
                     inconsistency); its durable rows could not be swept this boot"
                );
            }
            Err(e) => {
                summary.unreadable += 1;
                tracing::error!(
                    session = %sid,
                    error = %e,
                    "verification recovery could not open session {sid}; its durable rows stay \
                     for the next boot: {e}"
                );
            }
        }
    }
    if summary.requeued + summary.orphaned > 0 || summary.unreadable > 0 || summary.scan_failed {
        tracing::info!(
            requeued = summary.requeued,
            orphaned = summary.orphaned,
            touched = summary.touched,
            unreadable = summary.unreadable,
            scan_failed = summary.scan_failed,
            "verification recovery settled"
        );
    }
    summary
}

/// Typed summary of the startup queue-head recovery.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueRecoverySummary {
    pub(crate) candidates: usize,
    /// Sessions whose live queue still carried a runnable head.
    pub(crate) runnable: usize,
    /// Sessions whose durable queue state could not be read. They are KICKED
    /// ANYWAY (a read error is never an empty queue) and logged typed.
    pub(crate) unreadable: usize,
    /// The candidate scan itself failed: no kick could be computed here (the
    /// durable rows stay pending for the next boot).
    pub(crate) scan_failed: bool,
}

/// Daemon startup queue-head recovery (boundary race): a killed process can
/// leave a durable non-terminal prompt-queue row whose active turn already
/// settled; without a runner the row would wait for the next submit or settle
/// (durable, but stalled). The durable row itself is the runnable marker
/// (`Store::sessions_with_pending_queues`), so the executor's recovery entry
/// starts/arms exactly one bounded runner per session carrying one — never a
/// new submit, never a second concurrent drive, never an unbounded wait.
///
/// Runs BEFORE readiness, after `agent.recover()` and the verification
/// requeue, with the session store, manager and executor fully open. A store
/// read error is loud and NEVER classified as "already drained": the session
/// is handed to the executor's recovery entry anyway (which re-checks and
/// kicks, never releasing on a read error). A drive registry that refuses
/// the spawn is logged by the executor itself — those rows stay durably
/// pending for the next recovery, so nothing is ever lost.
pub(crate) fn recover_pending_queues_at_startup(graph: &DaemonGraph) -> QueueRecoverySummary {
    let mut summary = QueueRecoverySummary::default();
    let candidates = match graph.session.store().sessions_with_pending_queues() {
        Ok(sessions) => sessions,
        Err(e) => {
            summary.scan_failed = true;
            tracing::error!(
                error = %e,
                "queue recovery could not scan sessions; every durable queue row stays pending \
                 for the next boot: {e}"
            );
            return summary;
        }
    };
    summary.candidates = candidates.len();
    for session in &candidates {
        match graph.session.get_session(*session) {
            Ok(Some(handle)) => match handle.queued_prompt_count() {
                Ok(count) if count > 0 => summary.runnable += 1,
                Ok(_) => {}
                Err(e) => {
                    summary.unreadable += 1;
                    summary.runnable += 1;
                    tracing::error!(
                        session = %session,
                        error = %e,
                        "queue recovery could not read session {session}'s durable queue head; \
                         treating it as NON-EMPTY and handing it to the runner: {e}"
                    );
                }
            },
            Ok(None) => {
                summary.unreadable += 1;
                tracing::error!(
                    session = %session,
                    "queue recovery found a listed session with no handle (store inconsistency); \
                     its durable queue row stays pending"
                );
            }
            Err(e) => {
                summary.unreadable += 1;
                summary.runnable += 1;
                tracing::error!(
                    session = %session,
                    error = %e,
                    "queue recovery could not open session {session}; handing its durable queue \
                     head to the runner anyway: {e}"
                );
            }
        }
    }
    graph.tasks.recover_pending_queues();
    if summary.candidates > 0 || summary.scan_failed {
        tracing::info!(
            candidates = summary.candidates,
            runnable = summary.runnable,
            unreadable = summary.unreadable,
            scan_failed = summary.scan_failed,
            "queue recovery: durable queue heads handed to runners"
        );
    }
    summary
}

/// The daemon verification executor (audit P0-5/26 production wiring): a
/// background loop that claims Queued durable verification jobs of every
/// session, executes them through the REAL [`faktor_agent::AgentRuntime`]
/// execution primitive (claim -> execute -> resolve), and then re-settles a
/// run whose EXACT root verification attempt became terminal. Without this
/// executor a pending attempt would only ever settle on the next genuine
/// turn; with it, an ordinary background check resolves asynchronously and
/// the task's completion waits for exactly that result.
///
/// Store read errors are logged TYPED and deduplicated (this loop ticks 4x/s;
/// an unchanged error logs once, a changed one immediately) — a broken store
/// is never an invisible `continue`.
pub(crate) fn spawn_verification_executor(graph: &DaemonGraph) -> tokio::task::JoinHandle<()> {
    let session = graph.session.clone();
    let agent = graph.agent.clone();
    let tasks = graph.tasks.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_scan_error: Option<String> = None;
        let mut last_open_error: Option<String> = None;
        loop {
            tick.tick().await;
            let ids = match session.store().session_ids() {
                Ok(ids) => {
                    last_scan_error = None;
                    ids
                }
                Err(e) => {
                    // Deduplicated loudness: the durable queues are untouched
                    // and the next tick retries; an unchanged failure logs
                    // once instead of flooding 4x/s.
                    let message = e.to_string();
                    if last_scan_error.as_deref() != Some(message.as_str()) {
                        tracing::error!(
                            error = %message,
                            "verification executor could not scan sessions; durable jobs stay \
                             queued and the next tick retries: {message}"
                        );
                        last_scan_error = Some(message);
                    }
                    continue;
                }
            };
            for sid in ids {
                let handle = match session.get_session(sid) {
                    Ok(Some(handle)) => {
                        last_open_error = None;
                        handle
                    }
                    Ok(None) => {
                        let message = format!("session {sid} listed with no handle");
                        if last_open_error.as_deref() != Some(message.as_str()) {
                            tracing::error!(
                                session = %sid,
                                "verification executor found a listed session with no handle \
                                 (store inconsistency); its durable jobs stay queued"
                            );
                            last_open_error = Some(message);
                        }
                        continue;
                    }
                    Err(e) => {
                        let message = e.to_string();
                        if last_open_error.as_deref() != Some(message.as_str()) {
                            tracing::error!(
                                session = %sid,
                                error = %message,
                                "verification executor could not open session {sid} (its durable \
                                 jobs stay queued): {message}"
                            );
                            last_open_error = Some(message);
                        }
                        continue;
                    }
                };
                let resolved = match agent.execute_open_verification_jobs(&handle).await {
                    Ok(resolved) => resolved,
                    // No current attempt / unresolvable root: nothing to
                    // execute (never an error loop). Debug-typed so a real
                    // refusal is still traceable.
                    Err(e) => {
                        tracing::debug!(
                            session = %sid,
                            error = %e,
                            "verification executor found nothing to execute for session {sid}"
                        );
                        continue;
                    }
                };
                if resolved == 0 {
                    continue;
                }
                if let Err(e) = tasks.settle_resolved_verifications(sid).await {
                    tracing::warn!("post-executor settlement of session {sid} failed: {e}");
                }
            }
        }
    })
}

/// Resolve one ENABLED section's database path. The section's resolver
/// returns `None` only for a disabled section; reaching this helper with
/// `None` means the enabled guard and the resolver disagree (a config
/// combination no `unreachable!` may turn into a startup abort). Refuse with
/// a typed CONFIG error naming the section, never a panic and never an
/// implicit fallback path.
pub(crate) fn enabled_section_db_path(
    section: &str,
    resolved: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, String> {
    resolved
        .ok_or_else(|| format!("{section} config: the enabled section resolved no database path"))
}

use faktor_learning::LearningStore as _;
