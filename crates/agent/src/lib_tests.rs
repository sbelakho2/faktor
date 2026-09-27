use super::*;

#[cfg(test)]
pub(crate) async fn poll_evidence_with_wall_budget(
    provider: Arc<dyn EvidenceProvider>,
    session: SessionId,
    query: EvidenceQuery,
    budget: std::time::Duration,
) -> Vec<faktor_context::assembler::Evidence> {
    let outcome = poll_evidence_with_wall_budget_outcome(provider, session, query, budget).await;
    if outcome.status.is_degraded() {
        log_evidence_poll_degrade(&outcome.status, budget);
    }
    outcome.evidence
}

/// The agent may never match on provider names (Commandment 4). This test
/// locks that invariant structurally across the whole crate.
#[cfg(test)]
mod no_provider_switching {
    #[test]
    fn agent_source_has_no_provider_name_conditionals() {
        // Scan production sources only (skip test modules, whose own
        // assertions necessarily mention the forbidden literals).
        let mut sources = String::new();
        for file in [
            "lib.rs",
            "runtime/mod.rs",
            "runtime/turn/mod.rs",
            "runtime/turn/drive.rs",
            "runtime/turn/queue.rs",
            "runtime/turn/state.rs",
            "runtime/request.rs",
            "runtime/media.rs",
            "runtime/retrieval.rs",
            "runtime/routing.rs",
            "runtime/provider_loop.rs",
            "runtime/tool_loop.rs",
            "runtime/retry.rs",
            "runtime/settlement.rs",
            "runtime/compaction.rs",
            "runtime/verification_attribution.rs",
            "tool.rs",
            "tool_json.rs",
            "loop_detect.rs",
            "stall.rs",
        ] {
            let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let text = strip_test_modules(&text);
            sources.push_str(&text);
        }
        for needle in [
            "if provider ==",
            "match provider",
            "provider == \"deepseek\"",
            "provider == \"ollama\"",
            "provider == \"openai\"",
        ] {
            assert!(
                !sources.contains(needle),
                "agent source must not contain {needle:?}"
            );
        }
    }

    fn strip_test_modules(src: &str) -> String {
        // Remove #[cfg(test)] blocks so the invariant test cannot see its
        // own literals.
        let mut out = String::new();
        let mut rest = src;
        while let Some(idx) = rest.find("#[cfg(test)]") {
            out.push_str(&rest[..idx]);
            rest = &rest[idx + "#[cfg(test)]".len()..];
            // Skip to the closing brace of the mod at depth 0.
            let mut depth = 0i32;
            let mut consumed = 0usize;
            let mut found = false;
            for (i, c) in rest.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            consumed = i + 1;
                            found = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if found {
                rest = &rest[consumed..];
            }
        }
        out.push_str(rest);
        out
    }
}

// ---------------------------------------------------------------- service

/// Advisory evidence poll (audits 14/26): a provider failure MUST be
/// observable — typed status plus a structured diagnostic — never a silent
/// empty package. `NoEvidence` and `RetrievalFailed` are distinguishable,
/// and the legacy status-free wrapper is only allowed because the typed
/// path it delegates to logs the degradation loudly.
#[cfg(test)]
mod advisory_evidence_poll_tests {
    use crate::lib_tests::poll_evidence_with_wall_budget;
    use crate::*;
    use faktor_context::assembler::Evidence;
    use futures::future::BoxFuture;

    fn query() -> EvidenceQuery {
        EvidenceQuery {
            prompt: "where is the parser".into(),
            changed_files: vec!["src/a.rs".into()],
            failures: vec![],
        }
    }

    fn budget() -> std::time::Duration {
        std::time::Duration::from_millis(500)
    }

    struct FailingProvider {
        error: faktor_core::Error,
    }

    impl EvidenceProvider for FailingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            let error = self.error.clone();
            Box::pin(async move { Err(error) })
        }
    }

    struct EmptyProvider;

    impl EvidenceProvider for EmptyProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct ServingProvider;

    impl EvidenceProvider for ServingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async {
                Ok(vec![Evidence {
                    path: "src/a.rs".into(),
                    snippet: "fn parser()".into(),
                    score: 0.9,
                }])
            })
        }
    }

    struct PanickingProvider;

    impl EvidenceProvider for PanickingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async {
                panic!("provider exploded mid-poll");
                #[allow(unreachable_code)]
                Ok(Vec::new())
            })
        }
    }

    struct HangingProvider;

    impl EvidenceProvider for HangingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(futures::future::pending())
        }
    }

    fn session() -> SessionId {
        SessionId::new(7)
    }

    #[tokio::test]
    async fn failing_provider_is_typed_and_logged_loudly_not_silently_empty() {
        let failing = || {
            Arc::new(FailingProvider {
                error: faktor_core::Error::new(
                    faktor_core::error::ErrorKind::Provider {
                        code: "e503".into(),
                        retryable: true,
                    },
                    "embedding backend down",
                ),
            }) as Arc<dyn EvidenceProvider>
        };
        // Typed path: the failure is a status, not an indistinguishable
        // empty answer.
        let outcome =
            poll_evidence_with_wall_budget_outcome(failing(), session(), query(), budget()).await;
        assert!(outcome.evidence.is_empty());
        assert_eq!(
            outcome.status,
            EvidencePollStatus::RetrievalFailed {
                code: "e503".into(),
                retryable: true,
                message: "embedding backend down".into(),
            }
        );
        assert!(outcome.status.is_degraded());
        // Legacy path (the frozen runtime.rs call shape): the empty package
        // is accompanied by a degraded diagnostic, never silent.
        let before = evidence_poll_degraded_diagnostics();
        let legacy = poll_evidence_with_wall_budget(failing(), session(), query(), budget()).await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(
            legacy.is_empty(),
            "the advisory degrade is an empty package"
        );
        assert_eq!(
            after,
            before + 1,
            "a retrieval failure must emit exactly one degraded diagnostic"
        );
    }

    #[tokio::test]
    async fn empty_answer_is_no_evidence_not_a_retrieval_failure() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(EmptyProvider),
            session(),
            query(),
            budget(),
        )
        .await;
        assert!(outcome.evidence.is_empty());
        assert_eq!(outcome.status, EvidencePollStatus::NoEvidence);
        assert!(!outcome.status.is_degraded());
        // An honest empty answer is NOT a degradation: no diagnostic fires.
        let before = evidence_poll_degraded_diagnostics();
        let legacy =
            poll_evidence_with_wall_budget(Arc::new(EmptyProvider), session(), query(), budget())
                .await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(legacy.is_empty());
        assert_eq!(after, before, "no-evidence is not a retrieval failure");
    }

    #[tokio::test]
    async fn serving_provider_returns_the_package_unchanged() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(ServingProvider),
            session(),
            query(),
            budget(),
        )
        .await;
        assert_eq!(outcome.status, EvidencePollStatus::Served);
        assert!(!outcome.status.is_degraded());
        assert_eq!(outcome.evidence.len(), 1);
        assert_eq!(outcome.evidence[0].path, "src/a.rs");
    }

    #[tokio::test]
    async fn provider_panic_is_caught_and_typed() {
        // The provider panic must surface as a typed status; under a loaded
        // runner the 500ms default let the wall budget win the race. The
        // assertion is about the typed panic, so give the catch path room.
        let panic_budget = std::time::Duration::from_secs(5);
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(PanickingProvider),
            session(),
            query(),
            panic_budget,
        )
        .await;
        assert!(outcome.evidence.is_empty());
        match &outcome.status {
            EvidencePollStatus::ProviderPanicked { message } => {
                assert!(message.contains("provider exploded"), "{message}");
            }
            other => panic!("a panic must be typed, not silent: {other:?}"),
        }
        // The legacy path reports it too.
        let before = evidence_poll_degraded_diagnostics();
        let legacy = poll_evidence_with_wall_budget(
            Arc::new(PanickingProvider),
            session(),
            query(),
            panic_budget,
        )
        .await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(legacy.is_empty());
        assert_eq!(after, before + 1, "a provider panic must be reported");
    }

    #[tokio::test]
    async fn missed_wall_budget_is_typed_and_never_blocks() {
        let started = std::time::Instant::now();
        let before = evidence_poll_degraded_diagnostics();
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(HangingProvider),
            session(),
            query(),
            std::time::Duration::from_millis(20),
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(outcome.evidence.is_empty());
        assert_eq!(
            outcome.status,
            EvidencePollStatus::TimedOut { budget_ms: 20 }
        );
        assert!(outcome.status.is_degraded());
        // The legacy wrapper reports the missed budget as well.
        let legacy = poll_evidence_with_wall_budget(
            Arc::new(HangingProvider),
            session(),
            query(),
            std::time::Duration::from_millis(20),
        )
        .await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(legacy.is_empty());
        assert_eq!(after, before + 1, "a timeout must be reported");
    }

    #[tokio::test]
    async fn hostile_provider_message_is_bounded() {
        let huge = "x".repeat(64 * 1024);
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(FailingProvider {
                error: faktor_core::Error::new(faktor_core::error::ErrorKind::Internal, huge),
            }),
            session(),
            query(),
            budget(),
        )
        .await;
        match outcome.status {
            EvidencePollStatus::RetrievalFailed { message, .. } => {
                assert!(
                    message.len() <= EVIDENCE_POLL_MESSAGE_MAX_BYTES,
                    "{}",
                    message.len()
                );
            }
            other => panic!("expected a typed retrieval failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_provider_error_kinds_map_to_their_kind_name() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(FailingProvider {
                error: faktor_core::Error::timeout("index busy"),
            }),
            session(),
            query(),
            budget(),
        )
        .await;
        assert_eq!(
            outcome.status,
            EvidencePollStatus::RetrievalFailed {
                code: "Timeout".into(),
                retryable: true,
                message: "index busy".into(),
            }
        );
    }
}

/// Adversarial coverage of the bounded evidence executor (P1): a provider
/// that never yields must never grow the thread/worker population, excess
/// polls must be explicitly typed, shutdown/Drop must join owned workers
/// within a bound, and the off-turn blocking bridge must differentiate a
/// panic from a normal value.
#[cfg(test)]
mod bounded_evidence_executor_tests {
    use crate::*;
    use faktor_context::assembler::Evidence;
    use futures::future::BoxFuture;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn query() -> EvidenceQuery {
        EvidenceQuery {
            prompt: "where is the parser".into(),
            changed_files: vec!["src/a.rs".into()],
            failures: vec![],
        }
    }

    fn session() -> SessionId {
        SessionId::new(11)
    }

    struct ServingProvider;

    impl EvidenceProvider for ServingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async {
                Ok(vec![Evidence {
                    path: "src/a.rs".into(),
                    snippet: "fn parser()".into(),
                    score: 0.9,
                }])
            })
        }
    }

    /// A future that yields forever: cooperative — the worker's cancellation
    /// select drops it the moment the caller's budget fires.
    struct CooperativeHangingProvider;

    impl EvidenceProvider for CooperativeHangingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(futures::future::pending())
        }
    }

    /// A provider that blocks its OS thread INSIDE the first poll forever:
    /// not cancellable from safe Rust, so it can strand at most the fixed
    /// worker count.
    struct SyncBlockingProvider;

    impl EvidenceProvider for SyncBlockingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async move {
                let (_tx, rx) = std::sync::mpsc::channel::<()>();
                let _ = rx.recv();
                Ok(vec![])
            })
        }
    }

    /// Serializes the heavy measurement tests: the process-wide evidence
    /// worker counters are shared and the leak math needs a quiet window.
    static HEAVY_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Live OS threads of this process when the platform exposes them
    /// (Linux `/proc/self/task`, macOS `ps -M`); `None` elsewhere.
    fn live_thread_count() -> Option<usize> {
        #[cfg(target_os = "linux")]
        let count = std::fs::read_dir("/proc/self/task").ok().map(|d| d.count());
        #[cfg(target_os = "macos")]
        let count = std::process::Command::new("ps")
            .arg("-M")
            .arg(std::process::id().to_string())
            .output()
            .ok()
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .count()
                    .checked_sub(1)
            });
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let count = None;
        count
    }

    /// Bounded settle for OS-thread growth assertions: other tests in this
    /// binary run in parallel and their transient tokio worker/blocking
    /// threads can overlap a measurement. Wait (bounded) for the
    /// process-wide count to fit `before + slack`; a real per-poll leak is
    /// ~200 threads and cannot settle.
    async fn settled_thread_count(before: usize, slack: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut after = live_thread_count().unwrap_or(before);
        while after > before + slack && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
            after = live_thread_count().unwrap_or(after);
        }
        after
    }

    /// Initialize the process-wide pool once, so later spawn-counter deltas
    /// measure only this test's executors. Also pins the DOCUMENTED fixed
    /// size of the production pool: the whole concurrency budget of the
    /// feature is `EVIDENCE_EXECUTOR_WORKERS` owned workers with a
    /// `EVIDENCE_EXECUTOR_QUEUE_CAPACITY`-bounded admission queue.
    async fn warm_global_executor() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(outcome.status, EvidencePollStatus::Served);
        let global = global_evidence_executor();
        assert_eq!(
            global.worker_count(),
            EVIDENCE_EXECUTOR_WORKERS,
            "the production pool is fixed at its documented worker count"
        );
        assert_eq!(
            global.stats().capacity,
            EVIDENCE_EXECUTOR_QUEUE_CAPACITY,
            "the production pool keeps its documented bounded queue"
        );
    }

    #[tokio::test]
    async fn hundreds_of_polls_never_grow_the_worker_population() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        assert_eq!(executor.worker_count(), 2, "the pool is fixed at start");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before + 2,
            "start owns exactly its fixed worker count"
        );
        let spawned_after_start = EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst);
        let threads_before = live_thread_count();
        let (mut served, mut timed_out) = (0usize, 0usize);
        for i in 0..200usize {
            let (provider, budget): (Arc<dyn EvidenceProvider>, Duration) = if i % 4 == 0 {
                (Arc::new(ServingProvider), Duration::from_secs(5))
            } else {
                (
                    Arc::new(CooperativeHangingProvider),
                    Duration::from_millis(2),
                )
            };
            let outcome =
                poll_on_executor(executor.clone(), provider, session(), query(), budget).await;
            match outcome.status {
                EvidencePollStatus::Served => served += 1,
                EvidencePollStatus::TimedOut { .. } => timed_out += 1,
                other => panic!("unexpected status under bounded polling: {other:?}"),
            }
        }
        assert_eq!(served, 50, "served polls keep their normal answer");
        assert_eq!(timed_out, 150, "every missed budget is typed, none hangs");
        assert_eq!(executor.worker_count(), 2, "the pool must stay fixed");
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned_after_start,
            "200 polls must never spawn a worker thread (the old leak spawned one detached thread per poll)"
        );
        let stats = executor.stats();
        assert_eq!(stats.enqueued, 200);
        // The worker's completion counter settles just after each reply; the
        // last timed-out poll races its caller, so wait (bounded) for the
        // settle instead of asserting on a snapshot.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stats = stats;
        while stats.completed < 200 {
            assert!(
                Instant::now() < deadline,
                "every admitted poll must settle: {stats:?}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
            stats = executor.stats();
        }
        assert_eq!(stats.workers, 2, "the pool must stay fixed");
        assert_eq!(stats.capacity, 4, "the queue bound is fixed");
        assert!(stats.max_active <= 2, "active high-water: {stats:?}");
        assert!(stats.max_queue_depth <= 4, "queue high-water: {stats:?}");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before + 2,
            "200 polls must neither spawn nor leak an evidence worker"
        );
        if let Some(before) = threads_before {
            let after = settled_thread_count(before, 64).await;
            assert!(
                after <= before + 64,
                "200 polls must not leak threads (generous ceiling): before={before} after={after}"
            );
        }
    }

    #[tokio::test]
    async fn saturating_stuck_provider_is_typed_and_never_spawns_more_workers() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        let spawned_after_start = EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst);
        let threads_before = live_thread_count();
        let polls: Vec<_> = (0..60)
            .map(|_| {
                poll_on_executor(
                    executor.clone(),
                    Arc::new(SyncBlockingProvider),
                    session(),
                    query(),
                    Duration::from_millis(60),
                )
            })
            .collect();
        let outcomes = futures::future::join_all(polls).await;
        let (mut timed_out, mut refused) = (0usize, 0usize);
        for outcome in outcomes {
            match outcome.status {
                EvidencePollStatus::TimedOut { .. } => timed_out += 1,
                EvidencePollStatus::NotSpawned { message } => {
                    assert!(message.contains("saturated"), "{message}");
                    refused += 1;
                }
                other => panic!("stuck-provider polls must be typed, got {other:?}"),
            }
        }
        assert!(timed_out >= 2, "the two workers admit at least two polls");
        assert!(refused >= 1, "the burst must overflow the bounded pool");
        assert_eq!(timed_out + refused, 60, "every poll is accounted for");
        assert_eq!(
            executor.worker_count(),
            2,
            "saturation never grows the pool"
        );
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned_after_start,
            "a saturating burst must not spawn a worker"
        );
        let stats = executor.stats();
        assert_eq!(
            stats.enqueued as usize + stats.refused as usize,
            60,
            "admission is a bounded partition: {stats:?}"
        );
        assert!(stats.max_active <= 2, "active high-water: {stats:?}");
        assert!(stats.max_queue_depth <= 4, "queue high-water: {stats:?}");
        if let Some(before) = threads_before {
            let after = settled_thread_count(before, 64).await;
            assert!(
                after <= before + 64,
                "the stuck provider must not leak threads: before={before} after={after}"
            );
        }
        // Sequential hundreds of polls against the same wedged pool: every
        // poll is admitted into reclaimed capacity (cancelled queued entries
        // are purged) or refused, and ALWAYS typed — never a new thread.
        for _ in 0..200 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(SyncBlockingProvider),
                session(),
                query(),
                Duration::from_millis(5),
            )
            .await;
            assert!(
                matches!(
                    outcome.status,
                    EvidencePollStatus::TimedOut { .. } | EvidencePollStatus::NotSpawned { .. }
                ),
                "sequential polls against a wedged pool must stay typed: {:?}",
                outcome.status
            );
        }
        assert_eq!(executor.worker_count(), 2, "sequential polls grow nothing");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before + 2,
            "the stuck pool strands exactly the fixed worker count, never one thread per poll"
        );
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned_after_start,
            "200 sequential polls must not spawn a worker"
        );
        // The synchronously blocked workers cannot be cancelled from safe
        // Rust: a SHORT bounded shutdown reports the truth and abandons at
        // most the fixed worker count (never one thread per poll). The
        // executor handle is released, so Drop is immediate afterwards.
        let started = Instant::now();
        let disposition = executor.shutdown(Duration::from_millis(100));
        assert_eq!(
            disposition,
            EvidenceExecutorShutdownState::Abandoned { workers: 2 },
            "a synchronously blocked worker must be reported as abandoned, with the count"
        );
        assert_eq!(
            executor.shutdown(Duration::from_millis(100)),
            disposition,
            "the terminal disposition is persisted, not re-derived"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bounded join attempt must not wait out the provider"
        );
        assert_eq!(executor.worker_count(), 0);
        drop(executor);
    }

    #[tokio::test]
    async fn shutdown_cancels_cooperative_workers_within_bound() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        let mut polls = Vec::new();
        for _ in 0..2 {
            polls.push(tokio::spawn(poll_on_executor(
                executor.clone(),
                Arc::new(CooperativeHangingProvider),
                session(),
                query(),
                Duration::from_secs(30),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().max_active < 2 {
            assert!(Instant::now() < deadline, "workers never started the polls");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let started = Instant::now();
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean,
            "cooperative workers must be cancelled and joined"
        );
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean,
            "a repeated shutdown reports the same clean disposition"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown must join within its bound"
        );
        assert_eq!(executor.worker_count(), 0, "workers are owned, then joined");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "shutdown must leave no evidence worker alive"
        );
        for poll in polls {
            match poll.await.unwrap().status {
                EvidencePollStatus::NotSpawned { .. } => {}
                other => panic!("a shutdown-cancelled poll must be typed, got {other:?}"),
            }
        }
        let late = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            matches!(late.status, EvidencePollStatus::NotSpawned { .. }),
            "a closed executor refuses with a typed status: {:?}",
            late.status
        );
    }

    #[tokio::test]
    async fn drop_joins_cooperative_workers_within_bound() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        let mut polls = Vec::new();
        for _ in 0..2 {
            polls.push(tokio::spawn(poll_on_executor(
                executor.clone(),
                Arc::new(CooperativeHangingProvider),
                session(),
                query(),
                Duration::from_secs(30),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().max_active < 2 {
            assert!(Instant::now() < deadline, "workers never started the polls");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(
            Arc::strong_count(&executor),
            1,
            "the in-flight polls must not keep the executor alive"
        );
        let started = Instant::now();
        drop(executor);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Drop must cancel and join owned workers within the bound"
        );
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "Drop must join every owned worker"
        );
        for poll in polls {
            match poll.await.unwrap().status {
                EvidencePollStatus::NotSpawned { .. } => {}
                other => panic!("a Drop-cancelled poll must be typed, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn off_turn_failure_is_a_typed_degradation_never_none() {
        assert_eq!(run_off_turn_thread_outcome(|| 21 + 21).await, Ok(42));
        match run_off_turn_thread_outcome(|| -> u32 { panic!("off-turn boom") }).await {
            Err(OffTurnThreadFailure::Panicked { message }) => {
                assert!(message.contains("off-turn boom"), "{message}");
            }
            other => panic!("a panic must be a typed outcome, got {other:?}"),
        }
        // P2: the typed failure maps to an explicit evidence degradation —
        // never `None`, never conflated with an honest empty answer.
        let failure = run_off_turn_thread_outcome(|| -> u32 { panic!("cold ladder boom") })
            .await
            .expect_err("the panic must be a typed outcome");
        let status = evidence_status_from_off_turn_failure(failure);
        match &status {
            EvidencePollStatus::ProviderPanicked { message } => {
                assert!(message.contains("cold ladder boom"), "{message}");
            }
            other => panic!("a panicking off-turn task must be ProviderPanicked, got {other:?}"),
        }
        assert!(
            status.is_degraded(),
            "a broken off-turn task is never no-evidence"
        );
        let unscheduled =
            evidence_status_from_off_turn_failure(OffTurnThreadFailure::Unscheduled {
                message: "pool closed".into(),
            });
        assert!(
            matches!(
                unscheduled,
                EvidencePollStatus::NotSpawned { ref message } if message.contains("pool closed")
            ),
            "{unscheduled:?}"
        );
        assert!(unscheduled.is_degraded());
    }

    /// Test seam: a gate that parks the provider's OS thread until the test
    /// releases it, so an ABANDONED worker can actually drain and prove that
    /// capacity recovers (the forever-blocked [`SyncBlockingProvider`]
    /// proves the terminal cap/circuit behavior instead).
    #[derive(Default)]
    struct BlockGate {
        released: std::sync::Mutex<bool>,
        cv: std::sync::Condvar,
        entered: AtomicUsize,
    }

    impl BlockGate {
        fn block(&self) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
            while !*released {
                released = self.cv.wait(released).unwrap_or_else(|p| p.into_inner());
            }
        }

        fn release_all(&self) {
            *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.cv.notify_all();
        }

        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }
    }

    struct GatedBlockingProvider {
        gate: Arc<BlockGate>,
    }

    impl EvidenceProvider for GatedBlockingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            let gate = self.gate.clone();
            Box::pin(async move {
                // Blocks the worker's OS thread inside the first poll: the
                // P1 scenario (not cancellable from safe Rust).
                gate.block();
                Ok(vec![Evidence {
                    path: "src/gated.rs".into(),
                    snippet: "served after release".into(),
                    score: 1.0,
                }])
            })
        }
    }

    /// P1: a stuck provider is quarantined after the hard retirement
    /// deadline, its physical thread is abandoned after the grace and its
    /// logical slot is REPLACED — evidence stays available AT the absolute
    /// runtime abandoned cap (healthy replacements keep serving); the
    /// degraded/open circuit appears only when a stuck worker needs an
    /// abandonment the exhausted budget refuses. The physical thread count
    /// never exceeds `workers + cap`, and a released provider drains the
    /// abandoned threads so capacity recovers.
    #[tokio::test]
    async fn stuck_workers_are_quarantined_replaced_and_capped_by_the_circuit() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let spawned_before = EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst);
        let threads_before = live_thread_count();
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            2,
            4,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(40),
                quarantine_grace: Duration::from_millis(40),
                max_abandoned: 4,
            },
        );
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Running,
            "no shutdown has run yet"
        );
        assert_eq!(executor.stats().circuit, EvidenceCircuitState::Closed);

        // Wedge both workers with providers that block their OS thread.
        for _ in 0..2 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(GatedBlockingProvider { gate: gate.clone() }),
                session(),
                query(),
                Duration::from_millis(20),
            )
            .await;
            assert!(
                matches!(outcome.status, EvidencePollStatus::TimedOut { .. }),
                "a synchronously blocked poll times out typed: {:?}",
                outcome.status
            );
        }
        assert_eq!(gate.entered(), 2, "both workers reached the provider");

        // Hard retirement deadline exceeded -> QUARANTINED (no early
        // abandonment: the grace has not expired).
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.workers_quarantined, 2, "{stats:?}");
        assert_eq!(stats.workers_healthy, 0, "{stats:?}");
        assert_eq!(
            stats.runtime_abandoned, 0,
            "quarantine never abandons early: {stats:?}"
        );
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");

        // Grace expiry -> the blocked physical threads are ABANDONED and
        // their logical slots replaced: evidence stays available.
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 2,
            "one abandoned physical thread per wedged worker: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 2, "{stats:?}");
        assert_eq!(stats.workers, 2, "replacement logical slots: {stats:?}");
        assert_eq!(stats.workers_healthy, 2, "{stats:?}");
        assert_eq!(stats.workers_quarantined, 0, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "under the cap the circuit stays closed: {stats:?}"
        );
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst) - spawned_before,
            4,
            "2 original + 2 replacement worker threads"
        );
        let served = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served.status,
            EvidencePollStatus::Served,
            "replacement slots keep evidence available while stuck threads never drain"
        );

        // Wedge the replacements too: the runtime abandoned count reaches
        // the absolute cap. That is NOT an open circuit: no stuck worker is
        // awaiting an abandonment the budget refuses, and healthy slots keep
        // serving.
        for _ in 0..2 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(GatedBlockingProvider { gate: gate.clone() }),
                session(),
                query(),
                Duration::from_millis(20),
            )
            .await;
            assert!(matches!(
                outcome.status,
                EvidencePollStatus::TimedOut { .. }
            ));
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "the absolute runtime cap is reached: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.workers, 2,
            "the two replacement logical slots are restored AT the cap: {stats:?}"
        );
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "at the cap, no stuck worker needs abandonment yet: {stats:?}"
        );
        let served_at_cap = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served_at_cap.status,
            EvidencePollStatus::Served,
            "healthy replacements serve even at runtime_abandoned == cap"
        );

        // Wedge ONE replacement: its grace expires while the runtime budget
        // is exhausted, so it CANNOT be abandoned — the degraded/open
        // circuit, naming the blocked stuck worker. Healthy slots still
        // serve.
        let outcome = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(
            outcome.status,
            EvidencePollStatus::TimedOut { .. }
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "no fifth physical thread may be abandoned: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 4,
                cap: 4,
                blocked: 1
            },
            "{stats:?}"
        );
        assert_eq!(
            stats.workers, 2,
            "the stuck slot is not silently dropped: {stats:?}"
        );
        assert_eq!(stats.workers_quarantined, 1, "{stats:?}");
        let served_degraded = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served_degraded.status,
            EvidencePollStatus::Served,
            "healthy replacements keep serving while the stuck slot waits"
        );

        // Physical growth is bounded by workers + the absolute runtime cap.
        let live_now = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        assert!(
            live_now <= live_before + 2 + 4,
            "live evidence threads must stay within workers + cap: before={live_before} now={live_now}"
        );
        if let Some(before) = threads_before {
            let after = settled_thread_count(before, 2 + 4 + 64).await;
            assert!(
                after <= before + 2 + 4 + 64,
                "OS thread growth must stay bounded (workers + cap + generous slack for parallel tests): before={before} after={after}"
            );
        }

        // RECOVERY: the stuck provider returns (test seam) -> the abandoned
        // threads drain -> the circuit closes -> capacity is restored.
        gate.release_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().runtime_abandoned > 0 {
            assert!(Instant::now() < deadline, "abandoned threads never drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");
        assert_eq!(stats.workers, 2, "capacity restored: {stats:?}");
        assert_eq!(stats.workers_healthy, 2, "{stats:?}");
        assert_eq!(
            stats.runtime_abandoned_total, 4,
            "the abandonment history stays observable: {stats:?}"
        );
        let recovered = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            recovered.status,
            EvidencePollStatus::Served,
            "recovered capacity serves evidence again"
        );
        // A drained pool shuts down cleanly, and the disposition persists.
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        drop(executor);
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "no evidence worker may outlive a clean shutdown"
        );
    }

    /// P1 regression (the EXACT production configuration: 4 workers with a
    /// runtime abandonment cap of 4): retiring all four originals must not
    /// starve evidence. Replacement logical slots are restored even at
    /// `runtime_abandoned == cap`, submit keeps serving, and the degraded
    /// circuit opens only when a stuck worker NEEDS an abandonment the
    /// exhausted budget refuses. Physical evidence threads stay within the
    /// documented `workers + cap` bound.
    #[tokio::test]
    async fn production_config_serves_evidence_after_all_four_originals_are_retired() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            EVIDENCE_EXECUTOR_WORKERS,
            EVIDENCE_EXECUTOR_QUEUE_CAPACITY,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(100),
                quarantine_grace: Duration::from_millis(100),
                max_abandoned: EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS,
            },
        );
        assert_eq!(executor.worker_count(), EVIDENCE_EXECUTOR_WORKERS);

        // Wedge all four originals.
        for _ in 0..EVIDENCE_EXECUTOR_WORKERS {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(GatedBlockingProvider { gate: gate.clone() }),
                session(),
                query(),
                Duration::from_millis(5),
            )
            .await;
            assert!(
                matches!(outcome.status, EvidencePollStatus::TimedOut { .. }),
                "a wedged poll times out typed: {:?}",
                outcome.status
            );
        }
        assert_eq!(gate.entered(), 4, "all four originals reached the provider");
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        assert_eq!(
            executor.stats().workers_quarantined,
            4,
            "{:?}",
            executor.stats()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "all four originals retired: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.workers, 4,
            "four REPLACEMENT logical slots at the cap: {stats:?}"
        );
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "at the cap no stuck worker needs abandonment yet: {stats:?}"
        );

        // The cap must NOT reject healthy work: a normal provider is Served.
        let served = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served.status,
            EvidencePollStatus::Served,
            "replacements keep serving at runtime_abandoned == cap"
        );

        // Wedge ONE replacement: the exhausted runtime budget refuses the
        // fifth physical abandonment and the degraded circuit state opens.
        let outcome = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(5),
        )
        .await;
        assert!(matches!(
            outcome.status,
            EvidencePollStatus::TimedOut { .. }
        ));
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "no fifth physical thread may be abandoned: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 4,
                cap: 4,
                blocked: 1
            },
            "{stats:?}"
        );
        assert_eq!(
            stats.workers, 4,
            "the stuck slot is not silently dropped: {stats:?}"
        );
        assert_eq!(stats.workers_quarantined, 1, "{stats:?}");
        let served = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served.status,
            EvidencePollStatus::Served,
            "healthy replacements keep serving while the stuck slot waits"
        );

        // Documented physical bound: workers + cap = 8 evidence threads.
        let live_now = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        assert!(
            live_now <= live_before + EVIDENCE_EXECUTOR_WORKERS + EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS,
            "physical evidence threads must stay within workers + cap: before={live_before} now={live_now}"
        );

        // Drain: the gate releases the wedged providers (originals + the
        // stuck replacement) -> abandoned threads exit, circuit closes.
        gate.release_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().runtime_abandoned > 0 {
            assert!(Instant::now() < deadline, "abandoned threads never drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");
        assert_eq!(stats.workers, 4, "capacity stays restored: {stats:?}");
        assert_eq!(stats.workers_healthy, 4, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned, 0, "{stats:?}");
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        drop(executor);
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "no evidence worker may outlive a clean shutdown"
        );
    }

    /// P1 regression: a quarantine belongs to the JOB that timed out, not to
    /// the worker id. Job A exceeds the retirement deadline and is
    /// quarantined, then RECOVERS before the grace; the same worker starts
    /// healthy job B. When A's OLD grace expires, B must NOT be retired and
    /// no abandonment may be charged.
    #[tokio::test]
    async fn recovered_job_quarantine_never_retires_the_next_job_on_the_same_worker() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate_a = Arc::new(BlockGate::default());
        let gate_b = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            1,
            2,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(30),
                quarantine_grace: Duration::from_millis(200),
                max_abandoned: 2,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);

        // Job A: wedges the only worker past its retirement deadline.
        let a_task = tokio::spawn(poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider {
                gate: gate_a.clone(),
            }),
            session(),
            query(),
            Duration::from_secs(5),
        ));
        while gate_a.entered() == 0 {
            assert!(
                Instant::now() < deadline,
                "job A never reached the provider"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.workers_quarantined, 1, "{stats:?}");
        assert_eq!(stats.runtime_abandoned, 0, "{stats:?}");

        // Job B is queued while A is still executing (submit runs maintain,
        // which must keep A's quarantine: A is still the executing job).
        let b_task = tokio::spawn(poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider {
                gate: gate_b.clone(),
            }),
            session(),
            query(),
            Duration::from_secs(5),
        ));
        // A recovers BEFORE its grace expires; the same worker starts B.
        gate_a.release_all();
        while gate_b.entered() == 0 {
            assert!(
                Instant::now() < deadline,
                "job B never reached the provider"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Let A's OLD grace expire while B executes on the same worker.
        tokio::time::sleep(Duration::from_millis(250)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 0,
            "B must not inherit A's expired quarantine: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 0, "{stats:?}");
        assert_eq!(stats.workers, 1, "the worker survives: {stats:?}");
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");

        // B (still blocked) may carry its OWN fresh quarantine; releasing it
        // lets the worker serve both recovered jobs.
        gate_b.release_all();
        let b_outcome = b_task.await.unwrap();
        assert_eq!(
            b_outcome.status,
            EvidencePollStatus::Served,
            "job B was never retired"
        );
        let a_outcome = a_task.await.unwrap();
        assert_eq!(
            a_outcome.status,
            EvidencePollStatus::Served,
            "job A recovered in time"
        );
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.workers_quarantined, 0, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 0, "{stats:?}");
        assert_eq!(executor.worker_count(), 1);
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        drop(executor);
        assert_eq!(EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst), live_before);
    }

    /// P2: runtime retirement and shutdown abandonment are DIFFERENT budgets.
    /// Only runtime abandonment is subject to `max_abandoned`; shutdown
    /// abandonment is bounded by the fixed worker count. The stats expose
    /// both, and the public invariant `runtime_abandoned <= max_abandoned`
    /// stays true even with a shutdown abandonment on top.
    #[tokio::test]
    async fn shutdown_abandonment_is_outside_the_runtime_budget() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            2,
            4,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(20),
                quarantine_grace: Duration::from_millis(20),
                max_abandoned: 1,
            },
        );
        // Runtime retirement: one wedged worker is quarantined, then
        // abandoned (the whole runtime budget).
        let wedged = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(wedged.status, EvidencePollStatus::TimedOut { .. }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 1, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned, 0, "{stats:?}");

        // Wedge the remaining owned worker: its grace expires while the
        // runtime budget is exhausted -> blocked/degraded circuit.
        let wedged = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(wedged.status, EvidencePollStatus::TimedOut { .. }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 1,
                cap: 1,
                blocked: 1
            },
            "{stats:?}"
        );

        // Shutdown abandons the blocked slot OUTSIDE the runtime budget: the
        // invariant is not violated and both counters are observable.
        let disposition = executor.shutdown(Duration::from_millis(50));
        assert_eq!(
            disposition,
            EvidenceExecutorShutdownState::Abandoned { workers: 2 },
            "1 runtime + 1 shutdown abandoned thread"
        );
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned, 1, "{stats:?}");
        assert!(
            stats.runtime_abandoned <= stats.max_abandoned,
            "only runtime abandonment is subject to max_abandoned: {stats:?}"
        );
        assert_eq!(stats.max_abandoned, 1, "{stats:?}");

        // Both stuck providers return: the two abandoned physical threads
        // drain and each counter drops against its own budget.
        gate.release_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stats = executor.stats();
            if stats.runtime_abandoned == 0 && stats.shutdown_abandoned == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "abandoned threads never drained: {stats:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned_total, 1, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned_total, 1, "{stats:?}");
        assert_eq!(executor.worker_count(), 0);
        drop(executor);
        assert_eq!(EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst), live_before);
    }

    /// P2 regression: admission gates on RUNNABLE OWNED slots, never on
    /// `running` (physical threads including detached ones). A pool whose
    /// logical slots are all retired must refuse typed even while a detached
    /// thread is still alive — otherwise the poll would only wait for the
    /// caller's budget to fire.
    #[tokio::test]
    async fn submit_refuses_typed_when_only_detached_workers_survive() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            1,
            2,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(20),
                quarantine_grace: Duration::from_millis(20),
                max_abandoned: 2,
            },
        );
        // Wedge the only worker so it registers as executing...
        let wedged = tokio::spawn(poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_secs(5),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while gate.entered() == 0 {
            assert!(Instant::now() < deadline, "the wedged poll never started");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // ...then detach it directly: no logical slot remains owned, while
        // the physical thread is still alive (`running > 0`).
        let (worker_id, job_id) = {
            let executing = executor
                .shared
                .executing
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let (job_id, job) = executing
                .iter()
                .next()
                .expect("the wedged job is executing");
            (job.worker_id, *job_id)
        };
        assert_eq!(
            executor.abandon_worker(worker_id, job_id),
            AbandonOutcome::Abandoned
        );
        assert_eq!(executor.worker_count(), 0, "no owned logical slot remains");
        assert!(
            executor.shared.running.load(Ordering::SeqCst) > 0,
            "the detached physical thread is still alive"
        );
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        // Simulate the OS refusing replacement threads: the pool stays at
        // zero owned slots while the detached thread survives.
        executor.restore_refused.store(true, Ordering::SeqCst);

        let enqueued_before = executor.stats().enqueued;
        let refused = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(50),
        )
        .await;
        match &refused.status {
            EvidencePollStatus::NotSpawned { message } => {
                assert!(message.contains("no runnable worker"), "{message}");
            }
            other => panic!("a detached-only pool must refuse typed, got {other:?}"),
        }
        assert!(refused.status.is_degraded());
        assert_eq!(
            executor.stats().enqueued,
            enqueued_before,
            "no job may be queued behind a detached-only pool"
        );
        assert_eq!(
            executor
                .shared
                .queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
            0
        );
        executor.restore_refused.store(false, Ordering::SeqCst);

        // Release the detached provider: its thread drains.
        gate.release_all();
        let outcome = wedged.await.unwrap();
        assert_eq!(outcome.status, EvidencePollStatus::Served);
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().runtime_abandoned > 0 {
            assert!(
                Instant::now() < deadline,
                "the detached thread never drained"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(executor);
        assert_eq!(EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst), live_before);
    }

    /// P1/P2 terminal behavior: a provider that NEVER returns leaves the
    /// executor DEGRADED — the runtime abandonment budget is exhausted while
    /// a stuck worker still needs abandonment, so the circuit is OPEN; the
    /// process is never killed, and a poll that cannot be admitted is
    /// refused typed (naming the exhausted budget). Shutdown abandons the
    /// blocked slot OUTSIDE the runtime budget and the persisted disposition
    /// reports the honest total on every call (never the old
    /// `false`-then-`true` lie).
    #[tokio::test]
    async fn forever_blocked_provider_opens_the_circuit_and_shutdown_stays_abandoned() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let executor = EvidenceExecutor::start_with_policy(
            1,
            2,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(20),
                quarantine_grace: Duration::from_millis(20),
                max_abandoned: 2,
            },
        );
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Running
        );

        // Wedge the single worker, then its first replacement: two runtime
        // abandonments reach the absolute cap.
        for round in 1..=2usize {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(SyncBlockingProvider),
                session(),
                query(),
                Duration::from_millis(10),
            )
            .await;
            assert!(matches!(
                outcome.status,
                EvidencePollStatus::TimedOut { .. }
            ));
            // The abandonment lands on a maintenance tick after the
            // retirement deadline; wait bounded instead of assuming a fixed
            // schedule under load.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                executor.maintain();
                if executor.stats().runtime_abandoned >= round {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "runtime abandonment {round} did not land within the bounded wait: {:?}",
                    executor.stats()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 2, "the absolute cap: {stats:?}");
        assert_eq!(
            stats.workers, 1,
            "the replacement slot is restored: {stats:?}"
        );
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "at the cap no stuck worker needs abandonment yet: {stats:?}"
        );

        // Wedge the second replacement: its grace expires while the runtime
        // budget is exhausted, so the slot cannot be reclaimed — the circuit
        // is OPEN and the degradation is typed.
        let outcome = poll_on_executor(
            executor.clone(),
            Arc::new(SyncBlockingProvider),
            session(),
            query(),
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(
            outcome.status,
            EvidencePollStatus::TimedOut { .. }
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 2, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 2,
                cap: 2,
                blocked: 1
            },
            "{stats:?}"
        );

        // Fill the bounded queue (the only owned worker is stuck): the next
        // poll cannot be admitted and is refused with the typed circuit
        // status, whose message names the exhausted abandonment budget.
        let enqueued_before = executor.stats().enqueued;
        let mut queued = Vec::new();
        for _ in 0..2 {
            queued.push(tokio::spawn(poll_on_executor(
                executor.clone(),
                Arc::new(SyncBlockingProvider),
                session(),
                query(),
                Duration::from_secs(30),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().enqueued < enqueued_before + 2 {
            assert!(Instant::now() < deadline, "queued polls never enqueued");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let refused = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(50),
        )
        .await;
        match &refused.status {
            EvidencePollStatus::CircuitOpen {
                abandoned,
                cap,
                message,
            } => {
                assert_eq!((*abandoned, *cap), (2, 2));
                assert!(
                    message.contains("budget is exhausted"),
                    "the refusal must name the exhausted abandonment budget: {message}"
                );
            }
            other => {
                panic!("a full queue on the open circuit must be refused typed, got {other:?}")
            }
        }
        assert!(refused.status.is_degraded());

        // Shutdown: the blocked slot is abandoned OUTSIDE the runtime
        // budget; the terminal disposition names the honest total (2 runtime
        // + 1 shutdown) and REPEATS identically.
        let first = executor.shutdown(Duration::from_millis(50));
        assert_eq!(
            first,
            EvidenceExecutorShutdownState::Abandoned { workers: 3 },
            "2 runtime + 1 shutdown abandoned physical threads"
        );
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 2, "{stats:?}");
        assert!(
            stats.runtime_abandoned <= stats.max_abandoned,
            "the public runtime invariant holds: {stats:?}"
        );
        assert_eq!(stats.shutdown_abandoned, 1, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 2, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned_total, 1, "{stats:?}");
        assert_eq!(executor.shutdown_state(), first);
        let second = executor.shutdown(Duration::from_millis(50));
        assert_eq!(
            second, first,
            "repeated shutdown must report the SAME terminal disposition"
        );
        assert_eq!(executor.worker_count(), 0);
        for poll in queued {
            match poll.await.unwrap().status {
                EvidencePollStatus::NotSpawned { .. } => {}
                other => panic!("a shutdown-cancelled poll must be typed, got {other:?}"),
            }
        }
        drop(executor);
        // The three forever-blocked threads cannot be killed from safe Rust:
        // they stay alive in this test process, bounded by workers + cap —
        // exactly the documented terminal behavior.
    }

    /// P2: the shutdown disposition is `Running` before the first call, then
    /// `Clean` on every call — never a value that flips to `true` merely
    /// because the join handles were forgotten.
    #[tokio::test]
    async fn shutdown_disposition_is_running_then_persistently_clean() {
        let _guard = HEAVY_TESTS.lock().await;
        let executor = EvidenceExecutor::start(2, 4);
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Running
        );
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        assert_eq!(
            executor.shutdown(Duration::from_millis(1)),
            EvidenceExecutorShutdownState::Clean,
            "the persisted disposition is returned without waiting again"
        );
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Clean
        );
        assert_eq!(executor.worker_count(), 0);
        drop(executor);
    }
}

/// P2 structural proof: the legacy `Option`-shaped off-turn wrapper is
/// DELETED, not merely unused. The typed `run_off_turn_thread_outcome` is the
/// only bridge, so a panic or an unschedulable bridge can never collapse
/// into `None` again.
#[cfg(test)]
mod off_turn_wrapper_is_deleted {
    #[test]
    fn production_sources_never_reference_the_option_wrapper() {
        // Assembled at runtime so this test's own source can never satisfy
        // (or accidentally trip) the needle it searches for.
        let needle = ["run_off_turn", "thread("].concat();
        let mut sources = String::new();
        for file in [
            "lib.rs",
            "runtime/mod.rs",
            "runtime/turn/mod.rs",
            "runtime/turn/drive.rs",
            "runtime/turn/queue.rs",
            "runtime/turn/state.rs",
            "runtime/request.rs",
            "runtime/media.rs",
            "runtime/retrieval.rs",
            "runtime/routing.rs",
            "runtime/provider_loop.rs",
            "runtime/tool_loop.rs",
            "runtime/retry.rs",
            "runtime/settlement.rs",
            "runtime/compaction.rs",
            "runtime/verification_attribution.rs",
            "tool.rs",
            "tool_json.rs",
            "loop_detect.rs",
            "stall.rs",
        ] {
            let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
            sources.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        }
        assert!(
            !sources.contains(&needle),
            "the Option-shaped off-turn wrapper must stay deleted: every production caller maps the typed outcome to an explicit evidence degradation"
        );
    }
}

// ---------------------------------------------------------------- service

/// VerificationService unit coverage (P0-9/10): scripted backend mapping,
/// disabled semantics, policy budgets and the REAL async executor path.
#[cfg(test)]
mod verification_service_tests {
    use crate::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_verify::exec::{
        CheckCategory, CheckKind, CheckSpec, VerificationContext, VerificationPolicy,
    };

    fn ctx_in(dir: &std::path::Path) -> VerificationContext {
        VerificationContext {
            session_id: 7,
            task_id: 9,
            operation_id: 11,
            workspace_id: 3,
            worktree_id: 1,
            root: dir.to_path_buf(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
            cancellation: CancellationToken::new(),
        }
    }

    fn quick_spec(id: &str, program: &str, args: &[&str]) -> CheckSpec {
        CheckSpec::new(
            id,
            CheckKind::Compile,
            CheckCategory::Quick,
            program,
            args.iter().copied(),
            true,
        )
    }

    #[tokio::test]
    async fn scripted_backend_maps_ok_to_passed_and_err_to_failed_with_command_text() {
        let calls: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let service = VerificationService::fake(move |cmd: &str| {
            calls2.lock().unwrap().push(cmd.to_string());
            if cmd.starts_with("bad") {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let passed = service
            .execute(&quick_spec("a", "cargo", &["check"]), &ctx)
            .await;
        assert_eq!(passed.status, CheckRunStatus::Passed);
        assert_eq!(passed.exit, Some(0));
        assert!(passed.finished_ms >= passed.started_ms);
        let failed = service
            .execute(&quick_spec("b", "bad", &["tool"]), &ctx)
            .await;
        assert_eq!(failed.status, CheckRunStatus::Failed);
        assert_eq!(failed.exit, None);
        assert_eq!(failed.summary.as_deref(), Some("boom"));
        // The scripted backend saw the canonical argv join, never a shell.
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["cargo check".to_string(), "bad tool".to_string()]
        );
    }

    #[tokio::test]
    async fn fake_ok_passes_every_check_and_disabled_reports_unconfigured() {
        let ok = VerificationService::fake_ok();
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let out = ok
            .execute(&quick_spec("rust_check", "cargo", &["check"]), &ctx)
            .await;
        assert_eq!(out.status, CheckRunStatus::Passed);
        assert_eq!(out.exit, Some(0));
        assert!(!ok.is_disabled());

        let off = VerificationService::disabled();
        assert!(off.is_disabled());
        assert_eq!(off.policy(), VerificationPolicy::disabled());
        // Zero-budget policy fails closed: nothing may run inline.
        let mut full = quick_spec("full", "cmake", &["--build", "."]);
        full.category = CheckCategory::Full;
        assert!(matches!(
            off.budget_for(&full),
            BudgetDecision::RunAsTaskOwnedOperation
        ));
        // The disabled backend is scripted: it can never persist jobs and
        // its zero budget fails closed under the unit cap.
        assert!(!off.can_persist_jobs());
        assert!(off.policy().unit_max.is_zero());
    }

    #[test]
    fn budget_decisions_follow_policy_not_a_universal_cap() {
        let service = VerificationService::fake_ok();
        assert_eq!(
            service.budget_for(&quick_spec("c", "cargo", &["check"])),
            BudgetDecision::RunInline(std::time::Duration::from_secs(60))
        );
        let mut test_spec = CheckSpec::new(
            "t",
            CheckKind::Test,
            CheckCategory::Unit,
            "cargo",
            ["test", "--lib"],
            true,
        );
        assert_eq!(
            service.budget_for(&test_spec),
            BudgetDecision::RunInline(std::time::Duration::from_secs(600))
        );
        test_spec.category = CheckCategory::Full;
        assert!(matches!(
            service.budget_for(&test_spec),
            BudgetDecision::RunAsTaskOwnedOperation
        ));
        // Scripted command backends (test seams) cannot persist jobs; the
        // real supervisor-backed executor can (audit P0-5/26).
        assert!(!service.can_persist_jobs());
    }

    #[tokio::test]
    async fn real_executor_runs_typed_argv_in_the_context_root() {
        let service = VerificationService::new(
            Arc::new(
                faktor_verify::exec::AsyncCheckExecutor::try_shared()
                    .expect("standalone supervisor"),
            ),
            VerificationPolicy::default(),
        );
        assert!(!service.is_disabled());
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let out = service
            .execute(&quick_spec("echo", "echo", &["root-marker"]), &ctx)
            .await;
        assert_eq!(out.status, CheckRunStatus::Passed, "{out:?}");
        assert_eq!(out.exit, Some(0));
        assert!(
            out.summary
                .as_deref()
                .unwrap_or_default()
                .contains("root-marker"),
            "real executor captured the child stdout: {out:?}"
        );
    }

    #[tokio::test]
    async fn real_executor_unavailable_when_the_program_is_missing() {
        let service = VerificationService::new(
            Arc::new(
                faktor_verify::exec::AsyncCheckExecutor::try_shared()
                    .expect("standalone supervisor"),
            ),
            VerificationPolicy::default(),
        );
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let out = service
            .execute(
                &quick_spec("ghost", "/nonexistent-tool-for-tests", &[]),
                &ctx,
            )
            .await;
        assert_eq!(out.status, CheckRunStatus::Unavailable);
        assert!(out
            .summary
            .as_deref()
            .unwrap_or_default()
            .contains("not found"));
    }
}

// ---------------------------------------------------------------- policy
// economics (P0-82/15/28): the production policy's stability consult and
// telemetry outcome records, tested adversarially against the real
// RouterService.

#[cfg(test)]
mod economic_policy_tests {
    use crate::*;
    use faktor_core::model::{
        MicroUsdPerToken, ModelDescriptor, ModelEconomics, ModelSource, RateLimitState,
    };

    fn desc(provider: &str, model: &str, input_price: u64) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: 100_000,
            max_output: 8192,
            tools: true,
            parallel_tools: true,
            reasoning: true,
            thinking: true,
            vision: false,
            structured_output: true,
            embeddings: false,
            streaming: true,
            economics: ModelEconomics {
                input_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(input_price),
                output_price_per_mtok: MicroUsdPerToken::from(1),
                cache_read_price_per_mtok: MicroUsdPerToken::from(input_price / 5),
                cache_write_price_per_mtok: MicroUsdPerToken::from(input_price / 2),
                estimated_latency_ms: 300,
                tool_reliability: 90,
                reasoning_reliability: 90,
                coding_reliability: 90,
                context_reliability: 90,
                availability: 100,
                rate_limit_state: RateLimitState::Healthy,
            },
            source: ModelSource::ProviderCatalog,
        }
    }

    fn economy(pair: Vec<ModelDescriptor>) -> Arc<EconomicRoutingPolicy> {
        EconomicRoutingPolicy::new(
            Arc::new(faktor_router::RouterService::new(pair)),
            RoutingMode::Economy,
        )
    }

    /// Deterministic content digest for the stability series (router tests
    /// use the same FNV construction; identical bytes must hash identically).
    fn tp(id: u64, bytes: &[u8]) -> TurnPrefix {
        let mut h = [0u8; 32];
        let mut acc = 0xcbf29ce484222325u64;
        for &b in bytes {
            acc ^= u64::from(b);
            acc = acc.wrapping_mul(0x100000001b3);
        }
        h[..8].copy_from_slice(&acc.to_le_bytes());
        h[8..16].copy_from_slice(&acc.wrapping_mul(31).to_le_bytes());
        h[16..24].copy_from_slice(&acc.wrapping_mul(97).to_le_bytes());
        h[24..].copy_from_slice(&acc.wrapping_mul(211).to_le_bytes());
        TurnPrefix::new(id, h, bytes.len() as u32)
    }

    fn req() -> faktor_router::RouteRequest {
        faktor_router::RouteRequest {
            context_tokens: 100,
            estimated_output_tokens: 10,
            ..Default::default()
        }
    }

    /// (a) A session recording stability < floor prices its decision with
    /// the churn penalty: same candidates, stability 1.0 vs 0.3 — the
    /// chosen candidate is the same but the DECISION reflects the penalty
    /// (scaled cost + audit), deterministic, and the read-failure case
    /// (no rows) routes without any penalty and never errors the turn.
    #[test]
    fn session_stability_below_floor_inflates_the_decision_and_no_rows_never_penalize() {
        let policy = economy(vec![
            desc("cheap", "cx", 1),  // 100 tokens x 1 + 10 x 1 = 110 micro
            desc("robust", "rx", 2), // 210 micro
        ]);
        let plain = policy.route(&req()).unwrap();
        assert_eq!(
            (plain.provider.as_str(), plain.model.as_str()),
            ("cheap", "cx")
        );
        assert_eq!(plain.estimated_cost_micro, 110);

        // Stability 1.0: byte-identical prefixes — no penalty, decision
        // identical to the plain route.
        let stable_bytes = vec![b's'; 40];
        let stable = [
            tp(1, &stable_bytes),
            tp(2, &stable_bytes),
            tp(3, &stable_bytes),
        ];
        let healthy = policy
            .route_with_session_stability(&req(), Some(&stable))
            .unwrap();
        assert_eq!(healthy, plain, "stable history: no penalty");

        // Stability 0.3: growth 40 -> 130 scores 40/130 = 0.308 < 0.8, so
        // the decision carries the churn premium: cost 110 -> ceil(110 x
        // 1.1538) = 127 and the audit names stability + penalty.
        let churny = [tp(1, &stable_bytes), tp(2, &stable_bytes), {
            let mut t = tp(
                3,
                b"stable-prefix-bytes-grown-longer-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            );
            t.prefix_tokens = 130;
            t
        }];
        let churned = policy
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert_eq!(
            (churned.provider.as_str(), churned.model.as_str()),
            ("cheap", "cx"),
            "the churn penalty scales the decision, it never silently swaps a candidate"
        );
        assert_eq!(churned.estimated_cost_micro, 127);
        assert!(
            churned.reasoning.contains("prefix_stability=0.308"),
            "{}",
            churned.reasoning
        );
        assert!(
            churned.reasoning.contains("churn_penalty=0.1538"),
            "{}",
            churned.reasoning
        );
        // Deterministic: identical history -> identical decision.
        let again = policy
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert_eq!(churned, again);
        // Stability read failure (no rows / None / empty): identical to the
        // plain route — no penalty, Ok, never an error on the turn.
        assert_eq!(
            policy.route_with_session_stability(&req(), None).unwrap(),
            plain
        );
        assert_eq!(
            policy
                .route_with_session_stability(&req(), Some(&[]))
                .unwrap(),
            plain
        );
    }

    /// (b) Telemetry outcome records: N settled calls through the policy
    /// update the wrapped RouterService reliability priors ONLY for the
    /// failing (provider, model, phase) instance pair, latency rides the
    /// records, and a rate-limited outcome cooldowns only that provider.
    #[test]
    fn settled_call_outcomes_update_priors_only_for_the_failing_instance_pair() {
        let svc = Arc::new(faktor_router::RouterService::new(vec![
            desc("a", "am", 1),
            desc("b", "bm", 1),
        ]));
        let policy = EconomicRoutingPolicy::new(svc.clone(), RoutingMode::Economy);
        assert_eq!(
            svc.telemetry
                .success_estimate("a", "am", RouterPhase::Implement),
            0.8
        );
        assert_eq!(
            svc.telemetry
                .success_estimate("b", "bm", RouterPhase::Implement),
            0.8
        );
        // N settled calls against (a, am): 9 failures + 1 success, with
        // latency. (b, bm) and every other phase stay untouched.
        for _ in 0..9 {
            policy.record_call_outcome(&SettledCallOutcome {
                provider: "a".into(),
                model: "am".into(),
                phase: RouterPhase::Implement,
                success: false,
                retried: false,
                rate_limited: false,
                latency_ms: 400,
                verified: None,
            });
        }
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "a".into(),
            model: "am".into(),
            phase: RouterPhase::Implement,
            success: true,
            retried: true,
            rate_limited: false,
            latency_ms: 200,
            verified: None,
        });
        let a_after = svc
            .telemetry
            .success_estimate("a", "am", RouterPhase::Implement);
        assert!(
            a_after < 0.8 && a_after > 0.0,
            "(a, am) reliability prior must decay: {a_after}"
        );
        assert_eq!(
            svc.telemetry
                .success_estimate("b", "bm", RouterPhase::Implement),
            0.8,
            "untouched pair keeps its prior exactly"
        );
        assert_eq!(
            svc.telemetry
                .success_estimate("a", "am", RouterPhase::Review),
            0.8,
            "untouched phase keeps its prior exactly"
        );
        let avg = svc
            .telemetry
            .avg_latency_ms("a", "am", RouterPhase::Implement);
        assert!(avg > 200.0 && avg < 400.0, "latency EWMA: {avg}");
        // The priors actually CHANGE routing: the failing pair loses the
        // next route of a comparable request.
        let d = policy.route(&req()).unwrap();
        assert_ne!(
            (d.provider.as_str(), d.model.as_str()),
            ("a", "am"),
            "the decayed pair must lose the next route: {}",
            d.reasoning
        );
        // Rate-limit outcome: cooldown for the limited provider only.
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "b".into(),
            model: "bm".into(),
            phase: RouterPhase::Implement,
            success: false,
            retried: false,
            rate_limited: true,
            latency_ms: 500,
            verified: None,
        });
        assert!(svc.telemetry.cooldown_active("b"));
        assert!(
            !svc.telemetry.cooldown_active("a"),
            "only the limiter cools down"
        );
    }

    /// Pinned mode: the stability consult keeps the pin's decision (fail
    /// closed) but still prices the churn premium into the estimate.
    #[test]
    fn pinned_stability_consult_keeps_the_pin_and_prices_churn() {
        // The pin is the CHEAPEST candidate (validation must let it win the
        // router's own evaluation or the pin is denied); the stability
        // premium then rides the pinned decision's cost.
        let pair = vec![desc("a", "am", 2), desc("b", "bm", 1)];
        let svc = Arc::new(faktor_router::RouterService::new(pair));
        let policy = EconomicRoutingPolicy::new(
            svc,
            RoutingMode::Pinned {
                provider: "b".into(),
                model: "bm".into(),
            },
        );
        let stable = [tp(1, b"same-bytes"), tp(2, b"same-bytes")];
        let d = policy
            .route_with_session_stability(&req(), Some(&stable))
            .unwrap();
        assert_eq!((d.provider.as_str(), d.model.as_str()), ("b", "bm"));
        assert_eq!(d.estimated_cost_micro, 110, "no penalty when stable");
        let churny = [tp(1, b"same-bytes"), {
            let mut t = tp(2, b"rewritten-to-a-different-prefix-bytes");
            t.prefix_tokens = 60;
            t
        }];
        let d2 = policy
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert_eq!(
            (d2.provider.as_str(), d2.model.as_str()),
            ("b", "bm"),
            "pin holds"
        );
        assert!(
            d2.estimated_cost_micro > 110,
            "churn premium must ride the pinned estimate: {}",
            d2.estimated_cost_micro
        );
        assert!(d2.reasoning.contains("churn_penalty="));
        // Passthrough pin: stability data changes nothing.
        let svc = Arc::new(faktor_router::RouterService::new(vec![desc("a", "am", 1)]));
        let passthrough = EconomicRoutingPolicy::new(
            svc,
            RoutingMode::Pinned {
                provider: String::new(),
                model: String::new(),
            },
        );
        let d3 = passthrough
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert!(d3.provider.is_empty() && d3.model.is_empty());
    }
}

/// Verified-outcome wiring coverage (audit items 13/14/L): the policy's
/// `record_call_outcome` verified entries land in the SAME store-backed
/// registry every route consult reads — appended once per explicit signal,
/// keyed by the FULL (provider, model, phase, task_class, risk_bucket) key,
/// durable across store reopens, and never learned from telemetry-only
/// feeds ("the model said done" is not a verified success).
#[cfg(test)]
mod verified_outcome_wiring_tests {
    use crate::*;
    use faktor_core::model::{MicroUsdPerToken, ModelDescriptor, ModelEconomics, ModelSource};
    use faktor_router::OutcomeStore;

    fn desc(provider: &str, model: &str, input: u64, output: u64, rel: u8) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: 512_000,
            max_output: 64_000,
            tools: true,
            parallel_tools: true,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: ModelEconomics {
                input_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(input),
                output_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(output),
                coding_reliability: rel,
                tool_reliability: rel,
                reasoning_reliability: rel,
                context_reliability: rel,
                ..Default::default()
            },
            source: ModelSource::ProviderCatalog,
        }
    }

    fn implement_req(tokens_in: u64, tokens_out: u64, floor: u8) -> faktor_router::RouteRequest {
        faktor_router::RouteRequest {
            phase: RouterPhase::Implement,
            required_capabilities: vec!["tools".into(), "streaming".into()],
            context_tokens: tokens_in,
            estimated_output_tokens: tokens_out,
            quality_floor: floor,
            task_budget_remaining_micro: 0,
            latency_preference_ms: None,
            ..Default::default()
        }
    }

    #[test]
    fn verified_entries_append_once_fully_keyed_and_telemetry_only_feeds_never_learn() {
        let dir = tempfile::tempdir().unwrap();
        let manager = faktor_session::SessionManager::open(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
        )
        .unwrap();
        let store = manager.store();
        let outcomes: Arc<dyn OutcomeStore> = Arc::new(StoreOutcomeStore::new(store.clone()));
        let policy = EconomicRoutingPolicy::new(
            Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
                vec![desc("fake", "m", 1, 3, 82)],
                std::collections::HashMap::new(),
                outcomes,
            )),
            RoutingMode::Economy,
        );
        // Telemetry-only feed (no verified signal — e.g. the settle sites
        // before the gate): the registry learns NOTHING.
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "fake".into(),
            model: "m".into(),
            phase: RouterPhase::Implement,
            success: true,
            retried: false,
            rate_limited: false,
            latency_ms: 100,
            verified: None,
        });
        assert!(
            store
                .model_outcome_stats_get(
                    "fake",
                    "m",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                )
                .unwrap()
                .is_none(),
            "the model said done — without the verified signal no sample may be learned"
        );
        // A genuine verified-success signal lands ONE success sample under
        // the FULL key, in the durable store.
        let v = VerifiedCallAttribution {
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
            verified_success: true,
            rework_cost_micro: u64::MAX,
            rework_turns: u64::MAX,
        };
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "fake".into(),
            model: "m".into(),
            phase: RouterPhase::Implement,
            success: true,
            retried: false,
            rate_limited: false,
            latency_ms: 200,
            verified: Some(v),
        });
        let row = store
            .model_outcome_stats_get(
                "fake",
                "m",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
            )
            .unwrap()
            .expect("the verified signal must reach the store");
        assert_eq!(row.successes_first_pass, 1);
        assert_eq!(row.failures_first_pass, 0);
        assert_eq!(
            row.rework_cost_micro_sum, 0,
            "a verified first-pass success never carries rework — hostile success numbers are ignored"
        );
        assert_eq!(row.sample_count, 1);
        // A DIFFERENT class/risk bucket stays untouched (full-key writes).
        assert!(store
            .model_outcome_stats_get(
                "fake",
                "m",
                RouterPhase::Implement,
                TaskClass::Hard,
                RiskBucket::High,
            )
            .unwrap()
            .is_none());
        // A failed-verification attribution records a FAILURE sample with
        // its rework under ITS key and never a success.
        let failed = VerifiedCallAttribution {
            task_class: TaskClass::Hard,
            risk_bucket: RiskBucket::High,
            verified_success: false,
            rework_cost_micro: 900_000,
            rework_turns: 2,
        };
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "fake".into(),
            model: "m".into(),
            phase: RouterPhase::Review,
            success: true,
            retried: false,
            rate_limited: false,
            latency_ms: 300,
            verified: Some(failed),
        });
        let row = store
            .model_outcome_stats_get(
                "fake",
                "m",
                RouterPhase::Review,
                TaskClass::Hard,
                RiskBucket::High,
            )
            .unwrap()
            .unwrap();
        assert_eq!(row.successes_first_pass, 0, "no success may be learned");
        assert_eq!(row.failures_first_pass, 1);
        assert_eq!(row.rework_cost_micro_sum, 900_000);
        assert_eq!(row.rework_turns_sum, 2);
        assert_eq!(row.sample_count, 1);
        // The store-backed phase consult folds the class/risk buckets.
        let folded = store
            .model_outcome_stats_phase("fake", "m", RouterPhase::Implement)
            .unwrap()
            .expect("Implement samples exist");
        assert_eq!(folded.successes_first_pass, 1);
        assert_eq!(folded.failures_first_pass, 0);
        let review_folded = store
            .model_outcome_stats_phase("fake", "m", RouterPhase::Review)
            .unwrap()
            .unwrap();
        assert_eq!(review_folded.failures_first_pass, 1);
    }

    #[test]
    fn store_backed_outcomes_serve_routing_after_reopen_and_failures_flip_cheap_to_strong() {
        // Routing after reopen reflects the RECORDED stats: with an empty
        // registry the $4/$30 candidate wins on price; three failed-
        // verification samples recorded against it (cheap's Implement /
        // Medium / Low key) survive a store reopen and flip the decision to
        // the $10/$25 candidate whose conservative expected cost is now
        // below the failure-history estimate. Mirrors the wave-B4 memory
        // registry tests at the router level, through the durable impl.
        let dir = tempfile::tempdir().unwrap();
        let cheap = desc("cheap", "fast", 4, 30, 95);
        let strong = desc("strong", "big", 10, 25, 95);
        let req = implement_req(10_000, 2_000, 60);
        let decide = |policy: &Arc<EconomicRoutingPolicy>| {
            let d = policy.route(&req).unwrap();
            (d.provider.clone(), d.model.clone())
        };
        let first_choice;
        {
            let manager = faktor_session::SessionManager::open(
                dir.path().join("store"),
                dir.path().join("cas"),
                true,
            )
            .unwrap();
            let store = manager.store();
            let policy = EconomicRoutingPolicy::new(
                Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
                    vec![cheap.clone(), strong.clone()],
                    std::collections::HashMap::new(),
                    Arc::new(StoreOutcomeStore::new(store.clone())),
                )),
                RoutingMode::Economy,
            );
            first_choice = decide(&policy);
            assert_eq!(
                first_choice,
                ("cheap".to_string(), "fast".to_string()),
                "with no verified history the cheaper candidate wins"
            );
            // Three failed-verification gates on cheap's settled calls.
            for _ in 0..3 {
                policy.record_call_outcome(&SettledCallOutcome {
                    provider: "cheap".into(),
                    model: "fast".into(),
                    phase: RouterPhase::Implement,
                    success: true,
                    retried: false,
                    rate_limited: false,
                    latency_ms: 250,
                    verified: Some(VerifiedCallAttribution {
                        task_class: TaskClass::Medium,
                        risk_bucket: RiskBucket::Low,
                        verified_success: false,
                        rework_cost_micro: 960_000,
                        rework_turns: 1,
                    }),
                });
            }
            let row = store
                .model_outcome_stats_get(
                    "cheap",
                    "fast",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                )
                .unwrap()
                .unwrap();
            assert_eq!(row.failures_first_pass, 3);
            assert_eq!(row.rework_cost_micro_sum, 3 * 960_000);
            // Crash + reopen: the samples live in the store.
        }
        let manager = faktor_session::SessionManager::open(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
        )
        .unwrap();
        let store = manager.store();
        let reopened = EconomicRoutingPolicy::new(
            Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
                vec![cheap.clone(), strong.clone()],
                std::collections::HashMap::new(),
                Arc::new(StoreOutcomeStore::new(store.clone())),
            )),
            RoutingMode::Economy,
        );
        let (provider, model) = decide(&reopened);
        assert_eq!(
            (provider.as_str(), model.as_str()),
            ("strong", "big"),
            "the recorded failure history must flip the route away from the cheap candidate"
        );
        let folded = store
            .model_outcome_stats_phase("cheap", "fast", RouterPhase::Implement)
            .unwrap()
            .expect("recorded stats survive the reopen");
        assert_eq!(folded.sample_count, 3);
    }
}

#[cfg(test)]
mod attempt_accounting_tests {
    use crate::*;
    use faktor_core::id::TaskId;
    use faktor_core::model::PricingSnapshot;
    use faktor_core::op::ModelCallAttempt;
    use faktor_session::{BudgetAuthority, BudgetError, BudgetView};
    use std::pin::Pin;

    /// Records every authority call behind a NoopBudget — the guard test
    /// proves a misordered refund/uncertain NEVER reaches the authority.
    struct CountingBudget {
        refund_calls: Arc<std::sync::atomic::AtomicUsize>,
        uncertain_calls: Arc<std::sync::atomic::AtomicUsize>,
        settle_calls: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingBudget {
        fn new() -> Self {
            Self {
                refund_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                uncertain_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                settle_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl BudgetAuthority for CountingBudget {
        fn reserve(
            &self,
            _s: SessionId,
            _t: TaskId,
            _op: faktor_core::id::OpId,
            _pred: u64,
            _snap: Option<PricingSnapshot>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<faktor_session::ReservationId, BudgetError>>
                    + Send,
            >,
        > {
            Box::pin(async { Ok(faktor_session::ReservationId::NOOP) })
        }
        fn reserve_attempt(
            &self,
            _s: SessionId,
            _t: TaskId,
            _a: ModelCallAttempt,
            _pred: u64,
            _snap: Option<PricingSnapshot>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<faktor_session::ReservationId, BudgetError>>
                    + Send,
            >,
        > {
            Box::pin(async { Ok(faktor_session::ReservationId::NOOP) })
        }
        fn mark_dispatched(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BudgetError>> + Send>> {
            let c = self.dispatch_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
        fn mark_uncertain(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
            _reason: String,
            _request_id: Option<String>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BudgetError>> + Send>> {
            let c = self.uncertain_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
        fn settle_usage(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
            _a: u64,
            _b: u64,
            _c: u64,
            _d: u64,
            _e: Option<u64>,
            _f: Option<String>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<u64>, BudgetError>> + Send>>
        {
            let c = self.settle_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            })
        }
        fn refund(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BudgetError>> + Send>> {
            let c = self.refund_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
        fn session_budget_view(
            &self,
            _s: SessionId,
            _t: TaskId,
        ) -> Result<BudgetView, BudgetError> {
            Ok(BudgetView {
                max_cost_micro: None,
                spent_cost_micro: 0,
                open_reserved_micro: 0,
                open_reservations: 0,
                uncertain_reserved_micro: 0,
                uncertain_reservations: 0,
                settled_count: 0,
            })
        }
        fn recover_after_restart(&self) {}
        fn reconcile_uncertain(
            &self,
            _s: SessionId,
            _t: TaskId,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<faktor_store::CostReconcileReport, BudgetError>,
                    > + Send,
            >,
        > {
            Box::pin(async { Ok(Default::default()) })
        }
        fn finalize_uncertain(
            &self,
            _s: SessionId,
            _t: TaskId,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<faktor_store::CostFinalizeReport, BudgetError>,
                    > + Send,
            >,
        > {
            Box::pin(async { Ok(Default::default()) })
        }
    }

    fn budget() -> CountingBudget {
        CountingBudget::new()
    }

    #[tokio::test]
    async fn refund_after_dispatch_is_refused_locally_and_never_reaches_the_authority() {
        // The five-runtime-site bug shape: refund AFTER mark_dispatched must
        // be impossible — the machine refuses locally with the ledger's own
        // typed error BEFORE the authority is touched.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(
            authority.clone(),
            SessionId::new(1),
            Some(faktor_session::ReservationId::new(7)),
        );
        acct.mark_dispatched().await.unwrap();
        assert!(acct.dispatched() && acct.is_open());
        let err = acct.fail_before_dispatch().await.unwrap_err();
        assert!(
            matches!(
                err,
                faktor_session::BudgetError::CannotRefundDispatched { .. }
            ),
            "{err:?}"
        );
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(acct.is_open(), "the refused refund changes nothing");
        // The legal terminal for a dispatched attempt: UNCERTAIN — exactly
        // one authority call, machine closed.
        acct.fail_after_dispatch("provider_error", None)
            .await
            .unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn uncertain_before_dispatch_is_refused_the_attempt_must_refund() {
        // A never-dispatched failure REFUNDS; marking it UNCERTAIN would
        // charge an estimate for a request that provably never left.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(
            authority.clone(),
            SessionId::new(1),
            Some(faktor_session::ReservationId::new(8)),
        );
        let err = acct
            .fail_after_dispatch("never_dispatched", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        acct.fail_before_dispatch().await.unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn settle_before_dispatch_and_double_terminal_calls_are_guarded() {
        // The machine lets money move exactly once and only in order:
        // settle requires a dispatched open attempt; a second terminal call
        // on a closed machine is refused without touching the authority.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(
            authority.clone(),
            SessionId::new(1),
            Some(faktor_session::ReservationId::new(9)),
        );
        assert!(matches!(
            acct.settle_usage(1, 0, 0, 1, None, None).await.unwrap_err(),
            faktor_session::BudgetError::NotOpen { status, .. } if status == "reserved"
        ));
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        acct.mark_dispatched().await.unwrap();
        acct.settle_usage(100, 0, 0, 10, None, None).await.unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let err = acct.settle_usage(1, 0, 0, 1, None, None).await.unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        let err = acct
            .fail_after_dispatch("double_terminal", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        let err = acct.fail_before_dispatch().await.unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn a_machine_without_a_reservation_moves_nothing() {
        // Unbudgeted calls: no money exists to move — dispatch is a silent
        // no-op, the refund path releases nothing, and settle/uncertain
        // (which require a dispatched attempt with a reservation) stay
        // refused without touching the authority.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(authority.clone(), SessionId::new(1), None);
        acct.mark_dispatched().await.unwrap();
        assert!(
            !acct.dispatched(),
            "nothing was dispatched (no reservation)"
        );
        assert!(matches!(
            acct.settle_usage(1, 0, 0, 1, None, None).await.unwrap_err(),
            faktor_session::BudgetError::NotOpen { status, .. } if status == "reserved"
        ));
        assert!(matches!(
            acct.fail_after_dispatch("x", None).await.unwrap_err(),
            faktor_session::BudgetError::NotOpen { .. }
        ));
        acct.fail_before_dispatch().await.unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}

#[cfg(test)]
mod model_call_intent_tests {
    use crate::*;

    #[test]
    fn quality_floors_are_hard_and_never_lowered() {
        // Implement/Review/Debug route on a HARD 60 floor; the compaction
        // summarizer is ADAPTIVE (60 target, 50 minimum) because its output
        // cannot certify anything; route_request applies the minimum
        // verbatim (the policy decides nothing above it).
        assert_eq!(ModelCallIntent::implement_main().quality_floor(), 60);
        assert_eq!(ModelCallIntent::debug().quality_floor(), 60);
        assert_eq!(ModelCallIntent::review().quality_floor(), 60);
        let compact = ModelCallIntent::compact();
        assert_eq!(compact.quality_floor(), 50, "adaptive minimum");
        assert_eq!(compact.quality_target(), 60, "adaptive target");
        assert!(matches!(
            compact.quality,
            QualityRequirement::Adaptive {
                minimum: 50,
                target: 60
            }
        ));
        let adaptive = ModelCallIntent {
            phase: RouterPhase::Implement,
            required_capabilities: vec![],
            quality: QualityRequirement::Adaptive {
                target: 85,
                minimum: 70,
            },
            expected_output_tokens: 2048,
            semantic_risk: 0,
            output_trust: OutputTrust::Implementation,
        };
        assert_eq!(adaptive.quality.minimum(), 70);
        assert_eq!(adaptive.quality.target(), 85);
        assert_eq!(adaptive.quality_floor(), 70);
        assert_eq!(adaptive.quality_target(), 85);
    }

    #[test]
    fn output_trust_is_explicit_and_compaction_can_never_certify() {
        // Item 26: every intent carries what its output may be trusted to
        // BE. Compaction is a context-compression summary: it may stand in
        // for transcript context but it certifies no completion and
        // replaces no immutable task fact/ledger row; a title is ephemeral;
        // implement/review keep their completion-capable classes.
        assert_eq!(
            ModelCallIntent::implement_main().output_trust(),
            OutputTrust::Implementation
        );
        assert_eq!(
            ModelCallIntent::debug().output_trust(),
            OutputTrust::Implementation
        );
        assert_eq!(
            ModelCallIntent::review().output_trust(),
            OutputTrust::VerificationOpinion
        );
        assert_eq!(
            ModelCallIntent::compact().output_trust(),
            OutputTrust::ContextCompression
        );
        assert_eq!(
            ModelCallIntent::title().output_trust(),
            OutputTrust::Ephemeral
        );
        assert!(!ModelCallIntent::compact().certifies_completion());
        assert!(!ModelCallIntent::title().certifies_completion());
        assert!(ModelCallIntent::implement_main().certifies_completion());
        assert!(ModelCallIntent::review().certifies_completion());
        assert!(!OutputTrust::ContextCompression.certifies_completion());
        assert!(!OutputTrust::Ephemeral.certifies_completion());
        assert!(!OutputTrust::ContextCompression.may_replace_durable_facts());
        assert!(!OutputTrust::Ephemeral.may_replace_durable_facts());
        assert!(OutputTrust::VerificationOpinion.may_replace_durable_facts());
        assert_eq!(
            OutputTrust::ContextCompression.provenance_tag(),
            "context_compression"
        );
    }

    #[test]
    fn model_output_trust_gates_the_correctness_critical_writers() {
        // Audit item 3 (negative coverage): a ContextCompression summary and
        // an Ephemeral title can take NEITHER the completion-fact NOR the
        // verification-evidence admission; an Implementation output may
        // author completion facts (existing behavior) but never verification
        // evidence, and a VerificationOpinion may author both.
        let compaction = ModelOutput::new("summary", OutputTrust::ContextCompression, 7);
        let ephemeral = ModelOutput::new("title", OutputTrust::Ephemeral, 8);
        let implementation = ModelOutput::new("patch", OutputTrust::Implementation, 9);
        let opinion = ModelOutput::new("verdict", OutputTrust::VerificationOpinion, 10);

        for refused in [&compaction, &ephemeral] {
            let err = refused.completion_fact().unwrap_err();
            assert_eq!(err.trust, refused.trust);
            assert!(err.to_string().contains(refused.trust.provenance_tag()));
            assert!(refused.verification_evidence().is_err());
        }
        assert!(
            implementation.verification_evidence().is_err(),
            "an implementation output is verified, never authored as evidence"
        );
        assert!(
            implementation.completion_fact().is_ok(),
            "existing Implementation behavior is unchanged"
        );
        assert!(opinion.completion_fact().is_ok());
        assert!(opinion.verification_evidence().is_ok());
        // The capability gate used by every durable fact writer: durable
        // projections pass, model text is checked before anything moves.
        assert!(FactSource::Durable.check_completion_fact().is_ok());
        assert!(FactSource::Model(&implementation)
            .check_completion_fact()
            .is_ok());
        assert!(FactSource::Model(&compaction)
            .check_completion_fact()
            .is_err());
        assert!(FactSource::Model(&ephemeral)
            .check_completion_fact()
            .is_err());
        // The refusal maps to a typed permission error, never a panic.
        let err = FactSource::refusal_error(&compaction.completion_fact().unwrap_err());
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
        // Provenance tags round-trip; unknown tags refuse.
        for trust in [
            OutputTrust::Ephemeral,
            OutputTrust::ContextCompression,
            OutputTrust::Implementation,
            OutputTrust::VerificationOpinion,
        ] {
            assert_eq!(
                OutputTrust::from_provenance_tag(trust.provenance_tag()),
                Some(trust)
            );
        }
        assert_eq!(OutputTrust::from_provenance_tag("made_up"), None);
    }

    #[test]
    fn route_request_carries_the_real_planned_dimensions_not_a_guess() {
        let intent = ModelCallIntent::implement_main();
        let req = intent.route_request(12_345, 7_000, 0);
        assert_eq!(req.context_tokens, 12_345, "the plan's real input estimate");
        assert_eq!(req.estimated_output_tokens, 7_000, "the real output cap");
        assert_eq!(req.quality_floor, 60);
        assert_eq!(
            req.quality_target, None,
            "a HARD requirement sends no preference tier"
        );
        assert_ne!(req.context_tokens, 16_384, "no hard-coded pre-plan guess");
        assert_ne!(req.estimated_output_tokens, 2048, "no hard-coded 2048");
        assert_eq!(intent.phase, RouterPhase::Implement);
        // The adaptive distinction really rides the request (audit item 2):
        // compaction sends its 60 target above its 50 minimum.
        let compact = ModelCallIntent::compact().route_request(1_000, 100, 0);
        assert_eq!(compact.quality_floor, 50);
        assert_eq!(compact.quality_target, Some(60));
        // Review stays hard: no target.
        assert_eq!(
            ModelCallIntent::review()
                .route_request(1_000, 100, 0)
                .quality_target,
            None
        );
    }
}

#[cfg(test)]
mod hard_quality_floor_tests {
    use crate::*;
    use faktor_core::model::ModelDescriptor;
    use faktor_router::RouterService;

    fn candidate(quality: u8) -> ModelDescriptor {
        ModelDescriptor {
            provider: "p".into(),
            model: "m".into(),
            context: 128_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: false,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: faktor_core::model::ModelEconomics {
                coding_reliability: quality,
                tool_reliability: quality,
                reasoning_reliability: quality,
                context_reliability: quality,
                ..Default::default()
            },
            source: faktor_core::model::ModelSource::ProviderCatalog,
        }
    }

    #[test]
    fn hard_60_with_best_available_50_is_a_no_capable_model_refusal() {
        // Audit (e): quality floors are HARD. The old logic lowered the
        // requested floor toward the best available candidate; the fix
        // refuses typed: requested hard 60 with best available 50 =>
        // NoCapableModel — and a 90-quality candidate serves the SAME
        // request.
        let policy = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(50)])),
            RoutingMode::Economy,
        );
        let req = ModelCallIntent::implement_main().route_request(4_000, 2_048, 0);
        assert!(
            matches!(
                policy.route(&req),
                Err(RouteFailure::NoCapableModel)
            ),
            "hard 60 with a best available 50 must be a typed NoCapableModel, never a lowered floor"
        );
        let policy2 = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(90)])),
            RoutingMode::Economy,
        );
        assert!(
            policy2.route(&req).is_ok(),
            "an above-floor candidate serves"
        );
        // Balanced raises to its band; MaximumQuality never probes below.
        let bal = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(70)])),
            RoutingMode::Balanced,
        );
        assert!(matches!(bal.route(&req), Err(RouteFailure::NoCapableModel)));
        let maxq = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(55)])),
            RoutingMode::MaximumQuality,
        );
        assert!(matches!(
            maxq.route(&req),
            Err(RouteFailure::NoCapableModel)
        ));
    }
}

#[cfg(test)]
mod efficiency_flags_tests {
    use crate::*;
    use faktor_context::information::FailurePrior;
    use faktor_context::selection::ContextCandidate;

    /// A hostile-value prior standing in for the learning crate's handle:
    /// the value is irrelevant to the gate test.
    struct AlwaysDouble;

    impl FailurePrior for AlwaysDouble {
        fn omission_risk(&self, _candidate: &ContextCandidate) -> f64 {
            2.0
        }
    }

    #[test]
    fn efficiency_flags_default_all_off() {
        let flags = EfficiencyFlags::default();
        assert!(!flags.failure_learning);
        assert!(!flags.ccr);
        assert!(!flags.typed_handoff);
        assert!(!flags.semantic_context);
        assert!(!flags.rework_routing);
    }

    /// The gate: the prior is handed to the planner ONLY when the parsed
    /// `failure_learning` flag is on AND a handle exists. Every other
    /// combination yields `None` (baseline/parity path).
    #[test]
    fn prior_is_applied_only_when_the_flag_is_on_and_a_handle_exists() {
        let prior = AlwaysDouble;
        let handle: Option<&(dyn FailurePrior + Send + Sync)> = Some(&prior);
        let off = EfficiencyFlags::default();
        assert!(off.context_prior(handle).is_none(), "off => no prior");
        let on = EfficiencyFlags {
            failure_learning: true,
            ..Default::default()
        };
        assert!(on.context_prior(handle).is_some(), "on + handle => prior");
        assert!(
            on.context_prior(None).is_none(),
            "on + no handle => no prior"
        );
        // Running through the concrete trait object never invokes the
        // prior while the flag is off (the handle is not even observed).
        let gated = off.context_prior(handle);
        assert!(gated.is_none());
    }
}

#[cfg(test)]
mod compact_adaptive_quality_tests {
    use crate::*;
    use faktor_core::model::{
        MicroUsdPerMillionTokens, ModelCapabilities, ModelEconomics, ModelSource, PriceQuote,
        PricingSnapshot, PricingState, QualityAuthority, QualityMetric, QualityStatement,
    };
    use faktor_router::{EmptyOutcomeStore, RouteCandidate, RouterService};

    fn candidate(provider: &str, model: &str, quality: u8, price: u64) -> RouteCandidate {
        let caps = ModelCapabilities {
            context: 128_000,
            max_output: 16_000,
            tools: true,
            streaming: true,
            ..Default::default()
        };
        let economics = ModelEconomics {
            tool_reliability: quality,
            reasoning_reliability: quality,
            coding_reliability: quality,
            context_reliability: quality,
            estimated_latency_ms: 300,
            ..Default::default()
        };
        let descriptor = faktor_core::model::ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: caps.context as u64,
            max_output: caps.max_output as u64,
            tools: caps.tools,
            parallel_tools: caps.parallel_tools,
            reasoning: caps.reasoning,
            thinking: caps.thinking,
            vision: caps.vision,
            structured_output: caps.json_schema,
            embeddings: caps.embeddings,
            streaming: caps.streaming,
            economics,
            source: ModelSource::UserOverride,
        };
        let pricing = PricingState::Known(PricingSnapshot::exact(
            PriceQuote {
                input: MicroUsdPerMillionTokens(price),
                output: MicroUsdPerMillionTokens(price),
                cache_read: MicroUsdPerMillionTokens(0),
                cache_write: MicroUsdPerMillionTokens(0),
            },
            1,
            "test".into(),
        ));
        // A DECLARED prior (UserConfigured authority) on every dimension:
        // the row is authorized by its owner, not by a ConservativeUnknown
        // placeholder.
        let authority = QualityAuthority::UserConfigured {
            source: format!("providers.{provider}.quality"),
            version: "faktor-user-quality-v1".into(),
        };
        let statement = QualityStatement {
            tool: QualityMetric::new(quality, authority.clone()),
            reasoning: QualityMetric::new(quality, authority.clone()),
            coding: QualityMetric::new(quality, authority.clone()),
            context: QualityMetric::new(quality, authority),
        };
        RouteCandidate::with_quality(descriptor, pricing, statement)
    }

    #[test]
    fn compact_adaptive_band_prefers_the_target_then_relaxes_and_cannot_certify() {
        // A strong-but-expensive 90 model and a cheap declared-50 model.
        let strong = candidate("strong", "big", 90, 30_000_000);
        let cheap = candidate("weak", "small", 50, 100_000);
        let both = Arc::new(RouterService::with_route_candidates(
            vec![strong, cheap.clone()],
            Arc::new(EmptyOutcomeStore),
        ));
        let policy = EconomicRoutingPolicy::new(both, RoutingMode::Economy);
        // Hard Implement floor 60: the 50-quality model is refused, so the
        // expensive 90 model serves (the hard floor is unchanged).
        let implement = ModelCallIntent::implement_main().route_request(1_000, 100, 0);
        assert_eq!(policy.route(&implement).unwrap().model, "big");
        let compact = ModelCallIntent::compact();
        assert_eq!(compact.quality_floor(), 50);
        assert_eq!(compact.quality_target(), 60);
        let req = compact.route_request(1_000, 100, 0);
        // Audit item 2: the adaptive request PREFERS its 60 target over the
        // cheaper declared-50 model — the target candidate wins outright.
        assert_eq!(
            policy.route(&req).unwrap().model,
            "big",
            "the target tier must win while a target candidate survives"
        );
        // With ONLY the declared-50 model registered, the adaptive request
        // relaxes into [50, 60): a lower-quality model may summarize.
        let relaxed = Arc::new(RouterService::with_route_candidates(
            vec![cheap],
            Arc::new(EmptyOutcomeStore),
        ));
        let relaxed_policy = EconomicRoutingPolicy::new(relaxed, RoutingMode::Economy);
        assert_eq!(relaxed_policy.route(&req).unwrap().model, "small");
        // ... and the compaction output can never certify completion nor
        // replace immutable task facts/ledger rows.
        assert!(!compact.certifies_completion());
        assert!(!compact.output_trust().may_replace_durable_facts());
        assert_eq!(
            compact.output_trust().provenance_tag(),
            "context_compression"
        );
    }
}
