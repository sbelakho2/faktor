//! TaskExecutor settlement: integration preparation, landing and post-run settlement (mechanically split from `task_executor`).

use super::*;

impl TaskExecutor {
    // ------------------------------------------------- post-run settlement (P1)

    /// The ONE post-run settlement, shared by the single-session shadowed
    /// path (today's `after_shadowed_drive`: the drive owns verification +
    /// the completion gate, settlement runs the contract steps and the
    /// shadow finalize — byte-identical ordering preserved) and the
    /// orchestrated path (aggregate root verification, contract steps, the
    /// `complete_verified_task` gate, then finalize with explicit-merge-only
    /// proposals). Every decision reads durable rows, so a crashed executor
    /// re-runs the SAME pass on reopen and converges without double side
    /// effects (the step runner's replay semantics).
    pub async fn settle_run(
        self: &Arc<Self>,
        run: RunSettlement,
    ) -> Result<SettlementOutcome, ExecError> {
        match run {
            RunSettlement::InSession { parent, run_id } => {
                self.settle_in_session(parent, run_id).await
            }
            RunSettlement::Orchestrated { parent, run_id } => {
                self.settle_orchestrated(parent, run_id).await
            }
        }
    }

    /// The remote-run completion gate (additive; classification documented in
    /// [`crate::remote_completion`]).
    ///
    /// A landed remote result for a run this executor placed remotely is
    /// classified:
    ///
    /// - a SUCCEEDED, self-verified, read-only result (`produced_digest: None`)
    ///   settles the parent run through the SAME post-run pass local runs use
    ///   ([`Self::settle_run`]) — the worker's claim is admissible ONLY for
    ///   this class;
    /// - everything else (a produced/mutated tree, a missing self-verification
    ///   claim, or a failed outcome) returns the typed
    ///   [`crate::remote_completion::RemoteCompletionOutcome::OriginVerificationRequired`]
    ///   and settles NOTHING: the origin must verify the produced work through
    ///   its own verification pipeline before any completion (fail closed).
    ///
    /// Lease loss/supersession is owned by the worker plane's bounded requeue
    /// policy: a result from a lost lease is refused before it ever lands, so
    /// it can never reach this gate. A malformed completion is a typed
    /// refusal before any durable read.
    pub async fn complete_remote_run(
        self: &Arc<Self>,
        completion: crate::remote_completion::RemoteRunCompletion,
    ) -> Result<crate::remote_completion::RemoteCompletionOutcome, ExecError> {
        use crate::remote_completion::{
            RemoteCompletionClass, RemoteCompletionOutcome, RemoteRunCompletion,
        };
        completion.validate().map_err(ExecError::Malformed)?;
        let class = completion
            .classify()
            .map_err(|reason| ExecError::Malformed(format!("remote completion: {reason}")))?;
        if class == RemoteCompletionClass::OriginVerificationRequired {
            tracing::warn!(
                session = completion.parent.raw(),
                job_id = %completion.job_id,
                run_id = %completion.run_id,
                generation = completion.generation,
                reason = %completion.origin_reason(),
                "a landed remote result requires origin verification; nothing is settled from the claim"
            );
            return Ok(RemoteCompletionOutcome::OriginVerificationRequired {
                class,
                job_id: completion.job_id.clone(),
                run_id: completion.run_id.clone(),
                reason: completion.origin_reason(),
            });
        }
        let RemoteRunCompletion {
            parent,
            run_id,
            kind,
            ..
        } = completion;
        let settlement = match kind.as_str() {
            "in_session" => {
                self.settle_run(RunSettlement::InSession {
                    parent,
                    run_id: run_id.clone(),
                })
                .await?
            }
            "orchestrated" => {
                self.settle_run(RunSettlement::Orchestrated {
                    parent,
                    run_id: run_id.clone(),
                })
                .await?
            }
            // `validate` already refused every other kind.
            other => {
                return Err(ExecError::Malformed(format!(
                    "unknown remote run kind {other:?}"
                )))
            }
        };
        Ok(RemoteCompletionOutcome::Settled { class, settlement })
    }

    /// Re-settle the runs of `parent` whose EXACT root verification attempt
    /// just became terminal (the daemon verification executor calls this
    /// after it resolves jobs). Without it a run parked on a pending attempt
    /// would never consume its result. Additive and convergent: an actively
    /// driven run is skipped (its drive settles itself), a run with open jobs
    /// or without a persisted root attempt is skipped, and `settle_run` on an
    /// already-settled run is a deterministic no-op.
    pub async fn settle_resolved_verifications(
        self: &Arc<Self>,
        parent: SessionId,
    ) -> Result<(), ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle
            .task_id()
            .map_err(|e| ExecError::Internal(format!("task id read: {e}")))?;
        // FIX 2: the verification-fact scan is an explicit durable read — a
        // store failure is an error (never "no runs to settle") and a
        // corrupt fact page refuses typed.
        let runs: Vec<String> = match handle.memory_facts() {
            Ok(facts) => facts
                .into_iter()
                .filter(|(kind, key, _)| kind == ROOT_ATTEMPT_FACT_KIND && key.starts_with("root:"))
                .filter_map(|(_, key, _)| key.strip_prefix("root:").map(str::to_string))
                .filter(|run| !run.is_empty())
                .collect(),
            Err(e) => {
                return Err(ExecError::from(classify_session_read(
                    "verification-attempt facts of the post-run settlement",
                    e,
                )))
            }
        };
        for run_id in runs {
            // A poisoned active-run lock must REFUSE the settlement pass:
            // treating it as "active" would silently skip work and treating
            // it as "empty" would double-drive a run.
            let active = match self.active_guard() {
                Ok(guard) => guard.contains_key(&run_id),
                Err(e) => return Err(ExecError::from(e)),
            };
            if active {
                continue;
            }
            // FIX 2: open verification jobs are read explicitly. A failed
            // read must NEVER collapse to "no open jobs" (which would let
            // the settlement race a still-open verification). A corrupt job
            // row refuses typed.
            let open = match handle.open_verification_jobs(task_id.raw()) {
                Ok(jobs) => jobs,
                Err(e) => {
                    return Err(ExecError::from(classify_session_read(
                        "open verification jobs of the post-run settlement",
                        e.into(),
                    )))
                }
            };
            if !open.is_empty() {
                continue;
            }
            if let Err(e) = self
                .settle_run(RunSettlement::Orchestrated {
                    parent,
                    run_id: run_id.clone(),
                })
                .await
            {
                eprintln!("post-executor settlement of run {run_id} failed: {e}");
            }
        }
        Ok(())
    }

    /// The in-session arm of [`Self::settle_run`] — the SAME ordering the
    /// orchestrated arm enforces:
    ///
    /// - a session carrying a LIVE managed shadow settles through
    ///   prepare(candidate from the immutable shadow run base) -> verify
    ///   (candidate) -> land(owner, transactional) -> completion steps
    ///   against the landed proof -> `complete_verified_task`. The drive's
    ///   shadow-world verification is never the permission to complete:
    ///   `VerifiedComplete` is the CONSEQUENCE of a successful owner landing;
    /// - any other in-session run (read-only, a retired shadow) keeps the
    ///   drive's own deterministic verification + gate byte-identically:
    ///   this arm runs the accepted contract's requested steps (fail-closed
    ///   on the durable verification fact) and the shadow lifecycle settle.
    ///
    /// A step failure is logged, never escalated: the durable rows record
    /// the typed outcome and a later settlement retries.
    pub(super) async fn settle_in_session(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: String,
    ) -> Result<SettlementOutcome, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle.task_id()?;
        if let Some(shadows) = self.shadows() {
            if let Some(row) = shadows.active_shadow(parent)? {
                if row.state.is_live() {
                    return self
                        .settle_shadowed_in_session(parent, run_id, &handle, task_id, row)
                        .await;
                }
            }
        }
        // Proof-validated steps: only a durable PASSED record at the task's
        // CURRENT revision authorizes any side effect. No passing proof (or
        // only stale-revision proof) runs nothing — the durable fact is
        // reporting state, never authorization.
        let steps = match self.latest_passing_proof(&handle, task_id)? {
            Some(proof) => match self.run_completion_steps(parent, proof).await {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("completion-step execution failed for session {parent}: {e}");
                    None
                }
            },
            None => {
                // FIX 2: the contract read is explicit. A corrupt contract
                // refuses the settlement typed; a store failure is an error;
                // only a genuinely absent/default contract is "nothing to
                // report".
                let has_contract = match handle.completion_contract(task_id) {
                    Ok(Some((_, contract))) => !contract.is_default(),
                    Ok(None) => false,
                    Err(faktor_session::TaskError::CorruptDurableState { what, detail }) => {
                        return Err(ExecError::from(DurableStateError::CorruptDurableState {
                            what,
                            detail,
                        }))
                    }
                    Err(e) => {
                        return Err(ExecError::from(classify_session_read(
                            "completion contract of the in-session settlement",
                            e.into(),
                        )))
                    }
                };
                if has_contract {
                    eprintln!(
                        "completion steps for session {parent} are refused: no durable PASSED verification record at the current revision authorizes them; nothing is committed, pushed or opened"
                    );
                }
                None
            }
        };
        let finalize = self.finalize_shadow_run(parent)?;
        let task_state = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?
            .map(|t| t.state);
        let completed = task_state == Some(TaskState::VerifiedComplete);
        Ok(SettlementOutcome {
            run_id,
            orchestrated: false,
            complete: task_state.is_some_and(|s| s.is_terminal()),
            verified: verification_passed(&handle),
            completed,
            verification: None,
            steps,
            merge_proposals: Vec::new(),
            finalize,
        })
    }

    /// The single-item SHADOW arm of [`Self::settle_run`]: a one-child
    /// instance of the normal run-base/candidate/integration architecture.
    ///
    /// 1. **prepare** ([`Self::prepare_shadow_integration`]): the shadow's
    ///    candidate (its change set bound to the shadow's immutable run base)
    ///    is staged against the durable generation; the OWNER is untouched;
    /// 2. **verify** ([`Self::verify_prepared_integration`]) over the
    ///    CANDIDATE root (the shadow tree: base + the one child's changes),
    ///    creating the candidate-bound proof record;
    /// 3. **land** ([`Self::land_verified_integration`]): the transactional
    ///    owner landing (record-first per-path decisions + rollback blobs,
    ///    per-path CAS, whole-root equality). A conflict rolls every applied
    ///    path back — the owner is byte-identical to pre-landing;
    /// 4. **completion steps against the landed proof**, then the durable
    ///    completion gate (`complete_verified_task`);
    /// 5. **retire** the shadow row (record-first `Integrated` + directory
    ///    removal) ONLY after the owner holds the verified candidate.
    ///
    /// A landing conflict retains the shadow as `IntegrationBlocked` and
    /// leaves the task NON-TERMINAL; a later settlement retries (the bounded
    /// watcher, the next run's deterministic settle, or an explicit
    /// re-settle). The shadow-world certification the drive ran can never
    /// complete the task: while a managed shadow is live the session
    /// completion gate refuses an unbound proof.
    pub(super) async fn settle_shadowed_in_session(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: String,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        row: ShadowRow,
    ) -> Result<SettlementOutcome, ExecError> {
        let shadows = self
            .shadows()
            .ok_or_else(|| ExecError::Internal("shadow settlement requires the service".into()))?;
        let mut outcome = SettlementOutcome {
            run_id: run_id.clone(),
            orchestrated: false,
            complete: false,
            verified: false,
            completed: false,
            verification: None,
            steps: None,
            merge_proposals: Vec::new(),
            finalize: None,
        };
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?;
        let Some(task) = task else {
            return Ok(outcome);
        };
        match task.state {
            TaskState::Failed | TaskState::Cancelled => {
                shadows
                    .discard(parent)
                    .map_err(|e| ExecError::from_shadow("shadow discard", e))?;
                outcome.complete = true;
                outcome.finalize = Some(ShadowFinalize {
                    action: ShadowFinalizeAction::Discarded,
                    merged: Vec::new(),
                    rejected: Vec::new(),
                    conflicts: Vec::new(),
                });
                return Ok(outcome);
            }
            TaskState::VerifiedComplete => {
                // Crash window: the owner landing succeeded and the task was
                // completed, but the shadow row was not retired. The landing
                // transaction recovery is idempotent; nothing is re-verified
                // against a moved owner.
                let prepared =
                    self.prepare_shadow_integration(handle, parent, &run_id, task_id, &row)?;
                let proof = self
                    .recover_shadow_landing(handle, task_id, &prepared)
                    .await?;
                let Some(proof) = proof else {
                    outcome.finalize = Some(retained_shadow_finalize());
                    return Ok(outcome);
                };
                let landed = self.land_verified_integration(handle, &proof)?;
                outcome.verification = Some(proof.record());
                outcome.verified = true;
                shadows
                    .mark_integrated(parent)
                    .map_err(|e| ExecError::from_shadow("shadow retirement", e))?;
                outcome.completed = true;
                outcome.complete = true;
                outcome.finalize = Some(ShadowFinalize {
                    action: ShadowFinalizeAction::Integrated,
                    merged: landed.prepared.changed.iter().map(PathBuf::from).collect(),
                    rejected: Vec::new(),
                    conflicts: Vec::new(),
                });
                return Ok(outcome);
            }
            _ => {}
        }
        if !self.route_root_to_verifying(handle, task_id)? {
            outcome.finalize = Some(retained_shadow_finalize());
            return Ok(outcome);
        }
        // (1) PREPARE from the immutable shadow run base.
        let prepared = self.prepare_shadow_integration(handle, parent, &run_id, task_id, &row)?;
        self.check_settlement_seam(CrashSeam::AfterCandidatePrepared)?;
        // (2) VERIFY the CANDIDATE (the shadow tree), under the run's no-op
        // policy: an empty change set completes only through the reviewer's
        // no-op proof (or the explicit Allowed disposition). The criteria are
        // the task row already loaded above — never a second read whose
        // absence could silently degrade to "no criteria".
        let criteria = task.acceptance_criteria.clone();
        let no_op = run_no_op_disposition(handle, &run_id)?;
        let Some(proof) = self
            .verify_prepared_integration(handle, task_id, &criteria, &prepared, no_op)
            .await?
        else {
            outcome.finalize = Some(retained_shadow_finalize());
            return Ok(outcome);
        };
        outcome.verification = Some(proof.record());
        outcome.verified = true;
        // (3) LAND the verified candidate transactionally.
        let landed = match self.land_verified_integration(handle, &proof) {
            Ok(landed) => landed,
            Err(ExecError::IntegrationConflict(reason)) => {
                // The owner is byte-identical to pre-landing (every applied
                // path rolled back); the shadow is retained and the task
                // stays non-terminal until the drift is resolved.
                shadows
                    .mark_integration_blocked(parent)
                    .map_err(|e| ExecError::from_shadow("shadow block", e))?;
                outcome.finalize = Some(ShadowFinalize {
                    action: ShadowFinalizeAction::IntegrationBlocked,
                    merged: Vec::new(),
                    rejected: Vec::new(),
                    conflicts: vec![(prepared.owner_root.clone(), reason)],
                });
                return Ok(outcome);
            }
            Err(e) => {
                // A hard failure (drift, txn write, injected crash seam): the
                // shadow stays live and a later deterministic settle resumes
                // from the durable transaction phase.
                return Err(e);
            }
        };
        // (4) The accepted contract's steps against the LANDED proof.
        outcome.steps = match self
            .run_completion_steps_against_proof(parent, &landed)
            .await
        {
            Ok(report) => report,
            Err(e) => {
                eprintln!("completion-step execution failed for shadowed run {run_id}: {e}");
                None
            }
        };
        self.check_settlement_seam(CrashSeam::BeforeTaskCompletion)?;
        let mut completed = false;
        match handle.completion_contract_gate(task_id) {
            Ok(CompletionContractGate::Satisfied) => {
                let revision = handle
                    .task_revision(task_id)
                    .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
                match handle.complete_verified_task(task_id, revision, proof.record()) {
                    Ok(_) => completed = true,
                    Err(e) => {
                        eprintln!("shadowed completion gate refused for {run_id}: {e}");
                    }
                }
            }
            Ok(CompletionContractGate::Refused(e)) => {
                // The run stays non-terminal and a later settlement retries.
                eprintln!("shadowed run {run_id} contract gate: {e}");
            }
            Err(e) => {
                return Err(ExecError::Internal(format!(
                    "completion contract gate read for {run_id}: {e}"
                )));
            }
        }
        outcome.completed = completed;
        if completed {
            // (5) Retire the shadow ONLY after the owner holds the verified
            // candidate and the task is durably complete.
            shadows
                .mark_integrated(parent)
                .map_err(|e| ExecError::from_shadow("shadow retirement", e))?;
            outcome.complete = true;
            outcome.finalize = Some(ShadowFinalize {
                action: ShadowFinalizeAction::Integrated,
                merged: landed.prepared.changed.iter().map(PathBuf::from).collect(),
                rejected: Vec::new(),
                conflicts: Vec::new(),
            });
        } else {
            outcome.finalize = Some(retained_shadow_finalize());
        }
        Ok(outcome)
    }

    /// Recover the candidate-bound proof of an ALREADY LANDED shadow
    /// integration (the crash window between completion and shadow
    /// retirement): the landing transaction phase decides. A `Landed`
    /// transaction with the same verified candidate is finalized idempotently
    /// and a proof record is created/reused over the candidate snapshot.
    /// Everything else keeps the shadow retained.
    pub(super) async fn recover_shadow_landing(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        prepared: &PreparedRunIntegration,
    ) -> Result<Option<VerifiedRunIntegration>, ExecError> {
        let Some(txn) = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        else {
            return Ok(None);
        };
        if txn.phase != faktor_session::ledger::IntegrationTxnPhase::Landed
            || txn.verified_candidate_snapshot != prepared.candidate_snapshot
        {
            return Ok(None);
        }
        // FIX 2: the task row is an explicit read — a genuinely MISSING row
        // is the not-found policy (nothing to recover), a corrupt/failed
        // read is an error, never "no criteria".
        let criteria = match handle
            .get_task(task_id)
            .map_err(|e| ExecError::from(classify_session_read("root task row read", e)))?
        {
            Some(task) => task.acceptance_criteria,
            None => return Ok(None),
        };
        let run = faktor_agent::IntegratedRootVerification {
            status: VerificationStatus::Passed,
            checks: Vec::new(),
            criteria: Vec::new(),
            changed: prepared.changed.clone(),
            summary: "already-landed shadow integration recovery".into(),
            review_model_identity: None,
        };
        let no_op_rule = prepared.changed.is_empty();
        let composed = if no_op_rule {
            compose_no_op_root_verification_status(&run.checks, &run.criteria)
        } else {
            compose_root_verification_status(&run.checks, &run.criteria)
        };
        let composed = merge_verification_status(run.status, composed);
        if composed != VerificationStatus::Passed {
            return Ok(None);
        }
        let (record, basis_digest) = self
            .find_or_create_root_verification_record(
                handle,
                task_id,
                &criteria,
                &prepared.candidate_snapshot,
                &run,
                prepared,
                composed,
            )
            .await?;
        VerifiedRunIntegration::from_composed_verdict(
            prepared.clone(),
            record,
            &run.checks,
            &run.criteria,
            no_op_rule,
            composed,
            basis_digest,
        )
        .ok_or_else(|| {
            ExecError::Internal(
                "already-landed shadow recovery could not construct its landing proof".into(),
            )
        })
        .map(Some)
    }

    /// The orchestrated arm of [`Self::settle_run`] — the contract the
    /// multi-item path was missing:
    ///
    /// 1. **aggregate/root deterministic verification**: every SPAWN item of
    ///    the durable plan must have a `Done` child; the RUN (not any
    ///    candidate) is then verified against the tournament-style derived
    ///    check set over the parent's acceptance criteria (one deterministic
    ///    check per criterion, in order) and a passing durable record for
    ///    the root task's CURRENT revision is created (reused on replay);
    /// 2. **completion steps**: the run's accepted contract executes against
    ///    the run's integration root — never a child root;
    /// 3. **completion gate**: `complete_verified_task` consumes the
    ///    aggregate record once every requested step row is `Succeeded`;
    /// 4. **finalize**: no child candidate root is ever merged/committed;
    ///    the isolated roots are returned as explicit-merge proposals only.
    ///
    /// A run that is not fully Done, or whose task row is terminal, is a
    /// deterministic no-op. Errors from the step runner are logged (the
    /// durable rows carry the typed outcome); the function itself converges.
    /// The orchestrated arm of [`Self::settle_run`] — prepare -> verify
    /// (candidate) -> land (owner) -> completion steps -> completion gate:
    ///
    /// 1. **prepare** ([`Self::prepare_run_integration`]): the run's
    ///    IMMUTABLE base is copied into a candidate root, every Done
    ///    mutating child stages its change set against the same generation,
    ///    the children are precomposed (convergent-or-conflict) and applied
    ///    to the candidate. The OWNER is never touched;
    /// 2. **verify** ([`Self::verify_prepared_integration`]): the shared
    ///    deterministic verification runs over the CANDIDATE root and a
    ///    passing record bound to the candidate snapshot is created;
    /// 3. **land** ([`Self::land_verified_integration`]): the transactional
    ///    owner landing (record-first decisions + rollback blobs, per-path
    ///    CAS applies, whole-root equality check);
    /// 4. **completion steps against the proof** and the durable completion
    ///    gate (`complete_verified_task`).
    ///
    /// A run that is not fully Done, or whose task row is terminal, is a
    /// deterministic no-op. Errors from the step runner are logged (the
    /// durable rows carry the typed outcome); preparation/landing refusals
    /// propagate typed.
    pub(super) async fn settle_orchestrated(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: String,
    ) -> Result<SettlementOutcome, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let rows = OrchestratorRuntime::registry_rows(self.session.clone(), parent, &run_id)?;
        // The durable plan's SPAWN item set (the assignment contract): read
        // from the persisted plan row, never from memory. FIX 2: the plan row
        // is an explicit durable read — a MISSING row is the not-found
        // policy (nothing this settlement may name), while a PRESENT row that
        // is undecodable or lacks its `specs` is corrupt durable state and
        // refuses the settlement typed (never "no plan").
        let plan_specs: Option<Vec<ChildSpec>> = match parent_facts(&handle)?
            .into_iter()
            .find(|(kind, key, _)| kind == PLAN_ROW_KIND && key == &run_id)
        {
            None => None,
            Some((_, _, value)) => {
                let parsed: serde_json::Value = serde_json::from_str(&value).map_err(|e| {
                    ExecError::from(DurableStateError::CorruptDurableState {
                        what: format!("plan row of run {run_id}"),
                        detail: format!("stored plan row is not decodable JSON: {e}"),
                    })
                })?;
                let specs = parsed.get("specs").cloned().ok_or_else(|| {
                    ExecError::from(DurableStateError::CorruptDurableState {
                        what: format!("plan row of run {run_id}"),
                        detail: "stored plan row carries no `specs` array".into(),
                    })
                })?;
                let specs: Vec<ChildSpec> = serde_json::from_value(specs).map_err(|e| {
                    ExecError::from(DurableStateError::CorruptDurableState {
                        what: format!("plan row of run {run_id}"),
                        detail: format!("stored plan row `specs` is not a ChildSpec array: {e}"),
                    })
                })?;
                Some(specs)
            }
        };
        let Some(plan_specs) = plan_specs else {
            // No durable plan row: nothing this settlement may name.
            return Ok(SettlementOutcome {
                run_id: run_id.clone(),
                orchestrated: true,
                complete: false,
                verified: false,
                completed: false,
                verification: None,
                steps: None,
                merge_proposals: Vec::new(),
                finalize: None,
            });
        };
        let spawn_items: Vec<String> = plan_specs
            .into_iter()
            .filter(|s| s.spawn)
            .map(|s| s.item_id)
            .collect();
        let merge_proposals: Vec<String> = rows
            .iter()
            .filter(|r| {
                matches!(
                    r.ownership,
                    faktor_session::child::ChildOwnership::IsolatedWorktree
                        | faktor_session::child::ChildOwnership::ExclusivePaths
                )
            })
            .map(|r| r.child_id.clone())
            .collect();
        // A live shadow re-points the session's file consumers; an
        // orchestrated settlement must never land over it. (A new run is
        // already refused by `start_task`; this guards reopened/replayed
        // states.)
        if let Some(shadows) = self.shadows() {
            if let Some(row) = shadows.active_shadow(parent)? {
                if row.state.is_live() {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} carries a live shadow {}; settle the shadowed run before settling an orchestrated run",
                        row.shadow_id
                    )));
                }
            }
        }
        let finalize = None;
        // Point 8: a tournament run lands through THIS SAME pipeline once
        // its durable decision names a winner; until then it keeps the
        // explicit-merge proposal behavior (no auto integration).
        let tournament = crate::tournament::Tournament::reopen(&handle)
            .map_err(tournament_exec_error)?
            .into_iter()
            .find(|t| t.run_family == run_id);
        let tournament_decided = tournament
            .as_ref()
            .is_some_and(|t| t.state == crate::tournament::TournamentState::Decided);
        let tournament_aborted = tournament
            .as_ref()
            .is_some_and(|t| t.state == crate::tournament::TournamentState::Aborted);
        let winner: Option<String> = tournament
            .as_ref()
            .filter(|_| tournament_decided)
            .and_then(|t| t.winner.clone());
        let eligible: Vec<&crate::runtime::ChildRuntime> = if let Some(winner) = &winner {
            rows.iter().filter(|r| &r.child_id == winner).collect()
        } else {
            rows.iter()
                .filter(|r| {
                    matches!(
                        r.ownership,
                        faktor_session::child::ChildOwnership::IsolatedWorktree
                            | faktor_session::child::ChildOwnership::ExclusivePaths
                    ) && spawn_items.iter().any(|i| i == &r.item_id)
                })
                .collect()
        };
        let all_done = if tournament_decided {
            !eligible.is_empty() && eligible.iter().all(|r| r.state == ChildState::Done)
        } else {
            spawn_items.iter().all(|item| {
                rows.iter()
                    .any(|r| &r.item_id == item && r.state == ChildState::Done)
            })
        };
        let mut outcome = SettlementOutcome {
            run_id: run_id.clone(),
            orchestrated: true,
            complete: all_done,
            verified: false,
            completed: false,
            verification: None,
            steps: None,
            merge_proposals,
            finalize,
        };
        let task_id = handle.task_id()?;
        // Point 7: the root task row is created unconditionally at start;
        // a legacy run whose row is missing is SEEDED here — the settlement
        // never returns early merely because no row exists.
        let task = match handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
        {
            Some(t) => t,
            None => {
                let plan = self.orchestrator.plan_row(parent, &run_id)?.plan;
                let now = handle.now_ms();
                handle
                    .create_task(faktor_session::Task {
                        task_id,
                        session_id: parent,
                        goal: truncate_bytes(&plan.goal, MAX_TASK_GOAL_BYTES),
                        acceptance_criteria: Vec::new(),
                        plan: Vec::new(),
                        attachments: Vec::new(),
                        budget: TaskBudget::default(),
                        state: TaskState::Pending,
                        created_ms: now,
                        updated_ms: now,
                    })
                    .map_err(|e| ExecError::Internal(format!("root task row seed: {e}")))?;
                handle
                    .get_task(task_id)
                    .map_err(|e| ExecError::Internal(format!("root task row re-read: {e}")))?
                    .ok_or_else(|| {
                        ExecError::Internal("root task row vanished after seeding".into())
                    })?
            }
        };
        if task.state == TaskState::VerifiedComplete {
            outcome.completed = true;
            outcome.verified = true;
            return Ok(outcome);
        }
        if !all_done || task.state.is_terminal() {
            return Ok(outcome);
        }
        if tournament.is_some() && !tournament_decided {
            // An open/aborted tournament never auto-integrates.
            if tournament_aborted {
                outcome.complete = false;
            }
            return Ok(outcome);
        }
        if !self.route_root_to_verifying(&handle, task_id)? {
            return Ok(outcome);
        }
        // The criteria are the durable row already loaded above: a state
        // transition never changes them, and a second read could only
        // degrade a vanished row to "no criteria".
        let criteria = task.acceptance_criteria.clone();
        // (1) PREPARE: compose the candidate from the immutable run base.
        // The owner is byte-untouched by this phase.
        let candidate_ids: Vec<String> = eligible.iter().map(|r| r.child_id.clone()).collect();
        let prepared =
            self.prepare_run_integration(&handle, parent, &run_id, task_id, &candidate_ids)?;
        self.check_settlement_seam(CrashSeam::AfterCandidatePrepared)?;
        // (2) VERIFY the CANDIDATE (never the owner) under the run's durable
        // no-op policy: an empty aggregate change set completes only through
        // the independent reviewer's no-op criterion proof (or the explicit
        // Allowed disposition; Refused never completes).
        let no_op = run_no_op_disposition(&handle, &run_id)?;
        let Some(proof) = self
            .verify_prepared_integration(&handle, task_id, &criteria, &prepared, no_op)
            .await?
        else {
            return Ok(outcome);
        };
        outcome.verification = Some(proof.record());
        outcome.verified = true;
        // (3) LAND the verified candidate transactionally.
        let landed = self.land_verified_integration(&handle, &proof)?;
        // (4) The accepted contract's steps against the landed proof.
        outcome.steps = match self
            .run_completion_steps_against_proof(parent, &landed)
            .await
        {
            Ok(report) => report,
            Err(e) => {
                eprintln!("completion-step execution failed for orchestrated run {run_id}: {e}");
                None
            }
        };
        self.check_settlement_seam(CrashSeam::BeforeTaskCompletion)?;
        // (5) Completion gate: the durable contract gate must be satisfied
        // (all requested step rows Succeeded) before the record is consumed.
        match handle.completion_contract_gate(task_id) {
            Ok(CompletionContractGate::Satisfied) => {
                let revision = handle
                    .task_revision(task_id)
                    .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
                match handle.complete_verified_task(task_id, revision, proof.record()) {
                    Ok(_) => {
                        outcome.completed = true;
                    }
                    Err(e) => {
                        eprintln!(
                            "root completion gate refused for orchestrated run {run_id}: {e}"
                        );
                    }
                }
            }
            Ok(CompletionContractGate::Refused(e)) => {
                // A missing/failed/skipped step row: the run stays
                // non-terminal and a later settlement retries.
                eprintln!("orchestrated run {run_id} contract gate: {e}");
            }
            Err(e) => {
                return Err(ExecError::Internal(format!(
                    "completion contract gate read for run {run_id}: {e}"
                )));
            }
        }
        Ok(outcome)
    }

    /// PREPARE phase (point 1): build the [`PreparedRunIntegration`]
    /// candidate of one run from its IMMUTABLE base — never from the live
    /// owner. Every Done isolated child stages against the recorded run
    /// generation, the whole set is precomposed (convergent-or-conflict,
    /// order-independent) and applied once per path into the candidate.
    /// Idempotent: a replay over the same durable state rebuilds the
    /// byte-identical candidate (CAS applies are idempotent).
    pub fn prepare_run_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        parent: SessionId,
        run_id: &str,
        task_id: TaskId,
        candidate_ids: &[String],
    ) -> Result<PreparedRunIntegration, ExecError> {
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        let owner_root = self.owner_root_of(parent, handle)?;
        let rb = durable_read_value(
            format!("run base of run {run_id}"),
            handle.ledger_run_base_read(run_id),
        )
        .map_err(ExecError::from)?
        .ok_or_else(|| {
            ExecError::IntegrationConflict(format!(
                "orchestrated run {run_id} has no recorded run base; refusing to stage against the live owner"
            ))
        })?;
        let base_root = PathBuf::from(&rb.root);
        let base_snapshot = digest(&base_root)?;
        if base_snapshot != rb.snapshot_hash {
            return Err(ExecError::WorkspaceDrift(format!(
                "run base of {run_id} digests to {base_snapshot} but the durable record says {}; the generation moved",
                rb.snapshot_hash
            )));
        }
        let plan = self.orchestrator.plan_row(parent, run_id)?;
        let run_exec_dir = plan.isolated_root.join(run_id);
        std::fs::create_dir_all(&run_exec_dir)
            .map_err(|e| ExecError::Internal(format!("run exec dir {run_exec_dir:?}: {e}")))?;
        let candidate_root = run_exec_dir.join("candidate");
        std::fs::create_dir_all(&candidate_root)
            .map_err(|e| ExecError::Internal(format!("candidate root {candidate_root:?}: {e}")))?;
        let mut candidate_snapshot = digest(&candidate_root)?;
        if candidate_snapshot != rb.snapshot_hash {
            // (Re)build the candidate from the run base: remove residue,
            // copy the immutable generation, re-digest. A candidate that
            // still cannot reproduce the base is a typed drift refusal.
            std::fs::remove_dir_all(&candidate_root).map_err(|e| {
                ExecError::Internal(format!("candidate reset {}: {e}", candidate_root.display()))
            })?;
            std::fs::create_dir_all(&candidate_root).map_err(|e| {
                ExecError::Internal(format!("candidate dir {}: {e}", candidate_root.display()))
            })?;
            faktor_fs::tree_manifest::copy_tree_manifest(
                &base_root,
                &candidate_root,
                MAX_RUN_BASE_ENTRIES,
                MAX_RUN_BASE_BYTES,
                RUN_BASE_SKIP_DIRS,
            )
            .map_err(|e| {
                ExecError::WorkspaceDrift(format!(
                    "candidate run-base copy of {}: {e}",
                    base_root.display()
                ))
            })?;
            candidate_snapshot = digest(&candidate_root)?;
            if candidate_snapshot != rb.snapshot_hash {
                return Err(ExecError::WorkspaceDrift(format!(
                    "candidate of {run_id} digests to {candidate_snapshot} after copying the run base {}; refusing to verify a drifted candidate",
                    rb.snapshot_hash
                )));
            }
        }
        // Stage every eligible child against the SAME generation.
        let mut ids: Vec<String> = candidate_ids.to_vec();
        ids.sort();
        ids.dedup();
        let rows = OrchestratorRuntime::registry_rows(self.session.clone(), parent, run_id)?;
        let mut staged_sets: Vec<crate::runtime::merge::ChangeSet> = Vec::new();
        let mut staged: Vec<PreparedChildChangeSet> = Vec::new();
        let mut sources: Vec<faktor_session::IntegrationSourceRow> = Vec::new();
        for child_id in &ids {
            let child = rows
                .iter()
                .find(|r| &r.child_id == child_id)
                .ok_or_else(|| ExecError::NotFound(format!("child {child_id}")))?;
            if child.state != ChildState::Done {
                return Err(ExecError::InvalidState(format!(
                    "cannot integrate non-Done isolated child {} (state {:?})",
                    child.child_id, child.state
                )));
            }
            if !matches!(
                child.ownership,
                faktor_session::child::ChildOwnership::IsolatedWorktree
                    | faktor_session::child::ChildOwnership::ExclusivePaths
            ) {
                continue;
            }
            let cs = self.orchestrator.stage_child_changes(child_id)?;
            let child_root = self.orchestrator.child_worktree_dir(child)?;
            let candidate_root_hash = digest(&child_root)?;
            sources.push(faktor_session::IntegrationSourceRow {
                child_id: child.child_id.clone(),
                change_set_id: cs.id(),
                candidate_root_hash: candidate_root_hash.clone(),
            });
            staged_sets.push(cs.clone());
            staged.push(PreparedChildChangeSet {
                child_id: child.child_id.clone(),
                child_root,
                change_set: cs,
                candidate_root_hash,
            });
        }
        sources.sort_by(|a, b| a.child_id.cmp(&b.child_id));
        // Precompose: convergent children apply ONCE; any divergence (or
        // delete-vs-modify) is a typed conflict before any owner mutation.
        let composed = crate::runtime::merge::compose_child_changes(&staged_sets)?;
        for change in &composed {
            self.apply_composed_path_to_candidate(&candidate_root, &staged, change)?;
        }
        candidate_snapshot = digest(&candidate_root)?;
        let changed: Vec<String> = composed
            .iter()
            .map(|c| c.path.to_string_lossy().into_owned())
            .collect();
        let sources_digest = if sources.is_empty() {
            String::new()
        } else {
            let mut fields = Fields::new().uint(sources.len() as u64);
            for source in &sources {
                fields = fields
                    .text(&source.child_id)
                    .text(&source.change_set_id)
                    .text(&source.candidate_root_hash);
            }
            authority_digest_hex(DOMAIN_INTEGRATION_SOURCES, 1, fields)
        };
        Ok(PreparedRunIntegration {
            run_id: run_id.to_string(),
            task_id,
            owner_root,
            base_root,
            candidate_root,
            base_snapshot,
            candidate_snapshot,
            changed,
            sources,
            sources_digest,
            staged,
        })
    }

    /// Apply ONE composed per-path decision into the candidate root, using
    /// the deterministic representative child for the source content.
    pub(super) fn apply_composed_path_to_candidate(
        &self,
        candidate_root: &std::path::Path,
        staged: &[PreparedChildChangeSet],
        change: &crate::runtime::merge::ComposedPathChange,
    ) -> Result<(), ExecError> {
        match &change.child {
            Some(candidate) => {
                let source = staged
                    .iter()
                    .filter(|s| change.sources.contains(&s.child_id))
                    .min_by(|a, b| a.child_id.cmp(&b.child_id))
                    .ok_or_else(|| {
                        ExecError::Internal(format!(
                            "composed path {:?} names no staged source",
                            change.path
                        ))
                    })?;
                crate::runtime::merge::apply_manifest_entry(
                    candidate_root,
                    &change.path,
                    &source.child_root,
                    &change.path,
                    candidate,
                    change.base.as_ref(),
                )
                .map_err(|e| {
                    ExecError::from_fs(
                        &format!("candidate composition of {:?}", change.path),
                        candidate_root,
                        e,
                    )
                })?;
            }
            None => {
                let base = change.base.as_ref().ok_or_else(|| {
                    ExecError::Internal(format!(
                        "composed deletion {:?} has no base anchor",
                        change.path
                    ))
                })?;
                crate::runtime::merge::delete_manifest_entry(candidate_root, &change.path, base)
                    .map_err(|e| {
                        ExecError::from_fs(
                            &format!("candidate composition of {:?}", change.path),
                            candidate_root,
                            e,
                        )
                    })?;
            }
        }
        Ok(())
    }

    /// PREPARE phase of one single-item SHADOW run (the one-child instance
    /// of [`Self::prepare_run_integration`]): the shadow's staged change set
    /// is bound to the shadow's IMMUTABLE run base (`RunBaseRecord` recorded
    /// at `begin_shadow` under the shadow id) and the shadow tree is the
    /// CANDIDATE (base + the one child's changes, verified byte-identical to
    /// the staged child content). The owner is never a staging source; the
    /// OWNER is only the materialized base under the whole-root equality
    /// check every landing performs first, and each per-path rollback blob
    /// is re-verified against its recorded anchor before the transaction is
    /// recorded (see [`Self::build_path_decisions`]).
    pub fn prepare_shadow_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        parent: SessionId,
        run_id: &str,
        task_id: TaskId,
        row: &ShadowRow,
    ) -> Result<PreparedRunIntegration, ExecError> {
        let shadows = self.shadows().ok_or_else(|| {
            ExecError::Internal("shadow integration requires the shadow service".into())
        })?;
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        let owner_root = PathBuf::from(&row.base_root).canonicalize().map_err(|e| {
            ExecError::NotFound(format!(
                "shadow integration target {} vanished since begin_shadow: {e}",
                row.base_root
            ))
        })?;
        let candidate_root = PathBuf::from(&row.root).canonicalize().map_err(|e| {
            ExecError::NotFound(format!(
                "shadow root {} is gone; discard() and begin_shadow() again: {e}",
                row.root
            ))
        })?;
        let run_base = durable_read_value(
            format!("run base of shadow {}", row.shadow_id),
            handle.ledger_run_base_read(&row.shadow_id),
        )
        .map_err(ExecError::from)?
        .ok_or_else(|| {
            ExecError::IntegrationConflict(format!(
                "shadow {} has no recorded run base; refusing to integrate an unanchored candidate",
                row.shadow_id
            ))
        })?;
        let cs = shadows.stage_change_set(parent)?;
        if cs.run_base_snapshot.as_deref() != Some(run_base.snapshot_hash.as_str()) {
            return Err(ExecError::IntegrationConflict(format!(
                "change set {} of shadow {} binds run base {:?} but the shadow carries {}; a stale change set is never integrated",
                cs.id(),
                row.shadow_id,
                cs.run_base_snapshot,
                run_base.snapshot_hash
            )));
        }
        let candidate_snapshot = digest(&candidate_root)?;
        let changed: Vec<String> = cs
            .files
            .iter()
            .map(|entry| entry.path.to_string_lossy().into_owned())
            .collect();
        let source = faktor_session::IntegrationSourceRow {
            child_id: "shadow".to_string(),
            change_set_id: cs.id(),
            candidate_root_hash: candidate_snapshot.clone(),
        };
        let sources_digest = authority_digest_hex(
            DOMAIN_INTEGRATION_SOURCES,
            1,
            Fields::new()
                .uint(1)
                .text("shadow")
                .text(&cs.id())
                .text(&candidate_snapshot),
        );
        let staged = vec![PreparedChildChangeSet {
            child_id: "shadow".to_string(),
            child_root: candidate_root.clone(),
            change_set: cs,
            candidate_root_hash: candidate_snapshot.clone(),
        }];
        Ok(PreparedRunIntegration {
            run_id: run_id.to_string(),
            task_id,
            owner_root: owner_root.clone(),
            // Materialized base: at decision time the owner root is proven
            // byte-identical to the recorded run base before the first
            // durable decision row or write, and every blob read is
            // re-verified against its recorded base hash.
            base_root: owner_root,
            candidate_root,
            base_snapshot: run_base.snapshot_hash,
            candidate_snapshot,
            changed,
            sources: vec![source],
            sources_digest,
            staged,
        })
    }

    /// VERIFY phase (points 1/2/6): run the shared deterministic
    /// verification over the CANDIDATE root, persist the durable fact and a
    /// verification record bound to the CANDIDATE snapshot. `Ok(None)` for
    /// a failed/unavailable run (completion stays refused); a passing run
    /// returns the proof the landing phase consumes.
    ///
    /// The P0 no-op policy of an EMPTY aggregate change set is applied HERE:
    /// `Refused` never verifies (no record, completion stays refused);
    /// `Allowed` (an explicitly investigative task) mints the empty no-op
    /// record without a criterion proof; `RequiresCriterionProof` routes
    /// through [`faktor_agent::AgentRuntime::verify_integrated_root`]'s
    /// reviewer-proved no-op path — no reviewer verdict is a typed refusal,
    /// never a synthetic pass over an empty suite.
    pub async fn verify_prepared_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        criteria: &[String],
        prepared: &PreparedRunIntegration,
        no_op: NoOpDisposition,
    ) -> Result<Option<VerifiedRunIntegration>, ExecError> {
        let token = CancellationToken::new();
        let run = if prepared.changed.is_empty() && no_op == NoOpDisposition::Refused {
            persist_root_verification_fact(handle, "pending", &[], &[])?;
            eprintln!(
                "orchestrated run {} produced an empty aggregate change set and its no-op policy is refused; completion stays blocked",
                prepared.run_id
            );
            return Ok(None);
        } else if prepared.changed.is_empty() && no_op == NoOpDisposition::Allowed {
            faktor_agent::IntegratedRootVerification {
                status: VerificationStatus::Passed,
                checks: Vec::new(),
                criteria: Vec::new(),
                changed: Vec::new(),
                summary:
                    "empty aggregate change set allowed without criterion proof by the run's no-op policy"
                        .into(),
                // An `Allowed` no-op is an explicitly investigative task and
                // dispatches NO review-model call: no reviewer identity may
                // be claimed.
                review_model_identity: None,
            }
        } else if self
            .orchestrator
            .agent()
            .deps()
            .verification
            .can_persist_jobs()
        {
            // Attempt-based root verification (P0-5/26 production wiring):
            // cheap checks run inline, expensive checks are enqueued as
            // durable jobs, and the run consumes the EXACT attempt recorded
            // under its durable `verification_attempt/root:{run}` fact. A
            // pending attempt leaves the run unverified — the daemon
            // verification executor (or a later settlement) resolves it, and
            // only THAT attempt may certify (a newer attempt supersedes the
            // old one structurally; late results are typed-refused).
            let attempt_op = match read_root_attempt_op(handle, &prepared.run_id)? {
                Some(op) => op,
                None => {
                    let op = self
                        .session
                        .try_next_op_id()
                        .map_err(|e| ExecError::from(faktor_core::Error::from(e)))?
                        .raw();
                    persist_root_attempt_op(handle, &prepared.run_id, op)?;
                    op
                }
            };
            match self
                .orchestrator
                .agent()
                .verify_integrated_root_attempt(
                    handle,
                    &prepared.candidate_root,
                    &prepared.changed,
                    criteria,
                    &token,
                    attempt_op,
                )
                .await
            {
                Ok(outcome) => {
                    if outcome.attempt_op != attempt_op {
                        // A superseded attempt was replaced by a fresh one:
                        // the run now consumes the FRESH attempt only.
                        persist_root_attempt_op(handle, &prepared.run_id, outcome.attempt_op)?;
                    }
                    if outcome.pending {
                        persist_root_verification_fact(handle, "pending", &[], &prepared.changed)?;
                        eprintln!(
                            "root verification attempt {} of orchestrated run {} is still executing; completion waits",
                            outcome.attempt_op, prepared.run_id
                        );
                        return Ok(None);
                    }
                    match outcome.verification {
                        Some(run) => run,
                        None => {
                            persist_root_verification_fact(
                                handle,
                                "pending",
                                &[],
                                &prepared.changed,
                            )?;
                            return Ok(None);
                        }
                    }
                }
                Err(e) => {
                    persist_root_verification_fact(handle, "pending", &[], &prepared.changed)?;
                    eprintln!(
                        "root verification unavailable for orchestrated run {}: {e}; run stays unverified",
                        prepared.run_id
                    );
                    return Ok(None);
                }
            }
        } else {
            match self
                .orchestrator
                .agent()
                .verify_integrated_root(
                    handle,
                    &prepared.candidate_root,
                    &prepared.changed,
                    criteria,
                    &token,
                )
                .await
            {
                Ok(run) => run,
                Err(e) => {
                    persist_root_verification_fact(handle, "pending", &[], &prepared.changed)?;
                    eprintln!(
                        "root verification unavailable for orchestrated run {}: {e}; run stays unverified",
                        prepared.run_id
                    );
                    return Ok(None);
                }
            }
        };
        // The landing AUTHORITY is the COMPOSED verdict, never the run's
        // check-side status alone: green checks never authorize landing over
        // a failed or unavailable required criterion. The agent's check-side
        // status is folded in because it also witnesses required checks that
        // produced no execution row at all (missing evidence, never a pass).
        let no_op_rule = prepared.changed.is_empty();
        let composed = if no_op_rule {
            compose_no_op_root_verification_status(&run.checks, &run.criteria)
        } else {
            compose_root_verification_status(&run.checks, &run.criteria)
        };
        let composed = merge_verification_status(run.status, composed);
        let (record, basis_digest) = self
            .find_or_create_root_verification_record(
                handle,
                task_id,
                criteria,
                &prepared.candidate_snapshot,
                &run,
                prepared,
                composed,
            )
            .await?;
        persist_root_verification_fact(
            handle,
            verification_status_tag(composed),
            &run.checks,
            &prepared.changed,
        )?;
        if composed != VerificationStatus::Passed {
            return Ok(None);
        }
        self.check_settlement_seam(CrashSeam::AfterPreparedVerification)?;
        let proof = VerifiedRunIntegration::from_composed_verdict(
            prepared.clone(),
            record,
            &run.checks,
            &run.criteria,
            no_op_rule,
            composed,
            basis_digest,
        )
        .ok_or_else(|| {
            ExecError::Internal(format!(
                "composed verdict of run {} is Passed but no landing proof could be constructed",
                prepared.run_id
            ))
        })?;
        Ok(Some(proof))
    }

    /// LAND phase (points 5/6): transactionally apply the VERIFIED
    /// candidate into the owner. Record-first decisions + rollback blobs,
    /// owner-equals-run-base recheck, per-path CAS applies, fresh whole-root
    /// equality against the verified candidate, then the finalized
    /// integration record. Never called before a passing verification.
    pub fn land_verified_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        proof: &VerifiedRunIntegration,
    ) -> Result<LandedRunIntegration, ExecError> {
        // The constructor only ever yields a proof whose COMPOSED verdict is
        // `Passed`; re-assert it here so a future refactor can never reach
        // the first owner write with a non-passing composed status.
        if proof.composed_status() != VerificationStatus::Passed {
            return Err(ExecError::Internal(format!(
                "landing refused: run {} carries a non-passing ({:?}) composed root verdict",
                proof.prepared().run_id,
                proof.composed_status()
            )));
        }
        let prepared = proof.prepared();
        let owner_root = &prepared.owner_root;
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        // Crash recovery: inspect the durable transaction of this run and
        // deterministically finish landing or roll back.
        if let Some(mut txn) = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        {
            let same_candidate = txn.verified_candidate_snapshot == prepared.candidate_snapshot;
            match txn.phase {
                faktor_session::ledger::IntegrationTxnPhase::Landed if same_candidate => {
                    let landed = digest(owner_root)?;
                    if landed != prepared.candidate_snapshot {
                        return Err(ExecError::WorkspaceDrift(format!(
                            "owner root {} digests to {landed} but the Landed transaction binds {}",
                            owner_root.display(),
                            prepared.candidate_snapshot
                        )));
                    }
                    self.finalize_integration_record(
                        handle,
                        prepared,
                        &landed,
                        proof.proof_basis_digest(),
                    )?;
                    return Ok(LandedRunIntegration {
                        prepared: prepared.clone(),
                        record: proof.record(),
                        landed_snapshot: landed,
                    });
                }
                faktor_session::ledger::IntegrationTxnPhase::Landing if same_candidate => {
                    return self.resume_landing(handle, proof, &mut txn);
                }
                faktor_session::ledger::IntegrationTxnPhase::RollingBack if same_candidate => {
                    self.rollback_integration(handle, &mut txn)?;
                    return Err(ExecError::IntegrationConflict(format!(
                        "integration transaction of run {} was rolled back; the run must re-settle",
                        prepared.run_id
                    )));
                }
                faktor_session::ledger::IntegrationTxnPhase::RolledBack if same_candidate => {
                    // The rolled-back attempt is finished; fall through to a
                    // FRESH attempt (owner may have been restored).
                }
                _ => {}
            }
        }
        // Fresh attempt: the WHOLE owner root must still equal the run base
        // BEFORE the first durable decision row or owner write.
        let owner_now = digest(owner_root)?;
        if owner_now != prepared.base_snapshot {
            let reason = format!(
                "owner root {} digests to {owner_now} but the run base is {}; a drifted owner blocks landing before any write",
                owner_root.display(),
                prepared.base_snapshot
            );
            self.record_blocked_txn(handle, prepared, &reason)?;
            return Err(ExecError::IntegrationConflict(reason));
        }
        let decisions = self.build_path_decisions(prepared)?;
        let mut txn = faktor_session::ledger::IntegrationTxnRow {
            run_id: prepared.run_id.clone(),
            task_id: prepared.task_id.raw(),
            owner_root: owner_root.to_string_lossy().into_owned(),
            candidate_root: prepared.candidate_root.to_string_lossy().into_owned(),
            run_base_snapshot: prepared.base_snapshot.clone(),
            verified_candidate_snapshot: prepared.candidate_snapshot.clone(),
            sources_digest: prepared.sources_digest.clone(),
            phase: faktor_session::ledger::IntegrationTxnPhase::Prepared,
            paths: decisions,
            path_count: prepared.changed.len() as u64,
            applied_count: 0,
            conflicts: Vec::new(),
            at_ms: handle.now_ms(),
        };
        // Record-first: every decision + rollback blob (already in the CAS)
        // is durable BEFORE any owner write.
        handle
            .ledger_integration_txn_set(&txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::Verified;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(&txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        // Recheck the owner under the durable decisions; a drift here still
        // lands nothing.
        let owner_now = digest(owner_root)?;
        if owner_now != prepared.base_snapshot {
            let reason = format!(
                "owner root {} moved to {owner_now} after the landing decisions were recorded (run base {})",
                owner_root.display(),
                prepared.base_snapshot
            );
            txn.phase = faktor_session::ledger::IntegrationTxnPhase::Blocked;
            txn.conflicts.push(truncate_bytes(&reason, 256));
            txn.at_ms = handle.now_ms();
            handle
                .ledger_integration_txn_set(&txn)
                .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
            return Err(ExecError::IntegrationConflict(reason));
        }
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::Landing;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(&txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        self.check_settlement_seam(CrashSeam::AfterIntegrationTxnRecord)?;
        self.apply_landing_loop(handle, proof, &mut txn)
    }

    /// Finish a durable `Landing` transaction: re-apply every Pending path
    /// (CAS-idempotent) and reconcile a conflict into a rollback.
    pub(super) fn resume_landing(
        &self,
        handle: &faktor_session::SessionHandle,
        proof: &VerifiedRunIntegration,
        txn: &mut faktor_session::ledger::IntegrationTxnRow,
    ) -> Result<LandedRunIntegration, ExecError> {
        self.apply_landing_loop(handle, proof, txn)
    }

    /// The per-path apply loop of a landing transaction. Every applied path
    /// is journaled immediately; a conflict enters the rollback path (the
    /// owner is never left half-landed without a durable recovery row).
    pub(super) fn apply_landing_loop(
        &self,
        handle: &faktor_session::SessionHandle,
        proof: &VerifiedRunIntegration,
        txn: &mut faktor_session::ledger::IntegrationTxnRow,
    ) -> Result<LandedRunIntegration, ExecError> {
        let prepared = proof.prepared();
        let owner_root = PathBuf::from(&txn.owner_root);
        for i in 0..txn.paths.len() {
            if txn.paths[i].state != faktor_session::ledger::IntegrationPathTxnState::Pending {
                continue;
            }
            let path_txn = txn.paths[i].clone();
            match self.apply_landing_path(&owner_root, &prepared.candidate_root, &path_txn) {
                Ok(()) => {
                    txn.paths[i].state = faktor_session::ledger::IntegrationPathTxnState::Applied;
                    txn.applied_count = txn
                        .paths
                        .iter()
                        .filter(|p| {
                            p.state == faktor_session::ledger::IntegrationPathTxnState::Applied
                        })
                        .count() as u64;
                    txn.at_ms = handle.now_ms();
                    handle
                        .ledger_integration_txn_set(txn)
                        .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
                }
                Err(detail) => {
                    txn.paths[i].state = faktor_session::ledger::IntegrationPathTxnState::Conflict;
                    let line = format!("{}: {detail}", txn.paths[i].path);
                    txn.conflicts.push(truncate_bytes(&line, 256));
                    txn.at_ms = handle.now_ms();
                    handle
                        .ledger_integration_txn_set(txn)
                        .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
                    self.record_blocked_integration(handle, prepared, &txn.conflicts)?;
                    self.rollback_integration(handle, txn)?;
                    return Err(ExecError::IntegrationConflict(format!(
                        "owner landing of run {} conflicted at {}; the applied paths were rolled back",
                        prepared.run_id, txn.paths[i].path
                    )));
                }
            }
            self.check_settlement_seam(CrashSeam::IntegrationApply { after: i + 1 })?;
        }
        // Final equality (point 6): the landed owner root must EXACTLY
        // equal the verified candidate snapshot — never "merge returned ok".
        let landed = root_manifest_digest(&owner_root).map_err(|e| {
            ExecError::WorkspaceDrift(format!(
                "landed root snapshot of {}: {e}",
                owner_root.display()
            ))
        })?;
        if landed != prepared.candidate_snapshot {
            let reason = format!(
                "landed owner root {} digests to {landed} but the verified candidate is {}",
                owner_root.display(),
                prepared.candidate_snapshot
            );
            self.rollback_integration(handle, txn)?;
            txn.conflicts.push(truncate_bytes(&reason, 256));
            return Err(ExecError::WorkspaceDrift(reason));
        }
        self.check_settlement_seam(CrashSeam::AfterFinalOwnerSnapshot)?;
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::Landed;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        self.finalize_integration_record(handle, prepared, &landed, proof.proof_basis_digest())?;
        Ok(LandedRunIntegration {
            prepared: prepared.clone(),
            record: proof.record(),
            landed_snapshot: landed,
        })
    }

    /// Apply ONE per-path landing decision to the owner through the canonical
    /// entry-state CAS primitive: the owner's live `(kind, mode, payload /
    /// literal target)` must still equal the recorded base state, the
    /// candidate bytes are re-verified against the recorded candidate state,
    /// and the whole kind/mode/content/target transition is atomic. A legacy
    /// (byte-only) decision row is a typed refusal — never landed from.
    pub(super) fn apply_landing_path(
        &self,
        owner_root: &std::path::Path,
        candidate_root: &std::path::Path,
        path_txn: &faktor_session::ledger::IntegrationPathTxn,
    ) -> Result<(), String> {
        use faktor_fs::entry_state::EntryState;
        if !path_txn.canonical_ready() {
            return Err(format!(
                "landing decision of {:?} is not a canonical entry-state row (legacy byte-only material); refusing to land it",
                path_txn.path
            ));
        }
        let rel = std::path::Path::new(&path_txn.path);
        if path_txn.candidate_state == EntryState::Absent {
            crate::runtime::merge::delete_manifest_entry(owner_root, rel, &path_txn.base_state)
                .map(|_| ())
                .map_err(|e| e.message)
        } else {
            let payload = crate::runtime::merge::read_literal_payload(
                candidate_root,
                rel,
                &path_txn.candidate_state,
            )
            .map_err(|e| e.message)?;
            if !path_txn.candidate_state.material_matches(&payload) {
                return Err(format!(
                    "candidate {} drifted since staging; nothing was landed",
                    path_txn.path
                ));
            }
            faktor_fs::entry_state::apply_tree_entry_cas(
                owner_root,
                rel,
                &path_txn.base_state,
                &path_txn.candidate_state,
                &payload,
            )
            .map(|_| ())
            .map_err(|e| e.message)
        }
    }

    /// Build the record-first per-path decisions of a fresh landing in the
    /// CANONICAL entry-state vocabulary: `base_state` + `candidate_state`
    /// (kind/mode/payload / literal target) and the rollback CAS material
    /// (the base regular payload blob in the CAS, the base symlink literal
    /// target inline). A base path that is a special file the canonical
    /// vocabulary cannot represent is a typed refusal before anything is
    /// recorded.
    pub(super) fn build_path_decisions(
        &self,
        prepared: &PreparedRunIntegration,
    ) -> Result<Vec<faktor_session::ledger::IntegrationPathTxn>, ExecError> {
        use faktor_fs::entry_state::EntryState;
        let base_rows = crate::runtime::merge::canonical_manifest_rows(
            &prepared.base_root,
            MAX_RUN_BASE_ENTRIES,
        )?;
        let candidate_rows = crate::runtime::merge::canonical_manifest_rows(
            &prepared.candidate_root,
            MAX_RUN_BASE_ENTRIES,
        )?;
        let base_map: std::collections::HashMap<PathBuf, EntryState> =
            base_rows.into_iter().collect();
        let candidate_map: std::collections::HashMap<PathBuf, EntryState> =
            candidate_rows.into_iter().collect();
        let mut decisions = Vec::with_capacity(prepared.changed.len());
        for rel in &prepared.changed {
            let path = PathBuf::from(rel);
            let base_state = base_map.get(&path).cloned().unwrap_or(EntryState::Absent);
            let candidate_state = candidate_map
                .get(&path)
                .cloned()
                .unwrap_or(EntryState::Absent);
            let (rollback_blob, rollback_link_target) = match &base_state {
                EntryState::Absent => (None, None),
                EntryState::Regular { payload, .. } => {
                    let bytes = crate::runtime::merge::read_literal_payload(
                        &prepared.base_root,
                        &path,
                        &base_state,
                    )
                    .map_err(|e| {
                        ExecError::WorkspaceDrift(format!(
                            "run base content of {rel} no longer matches its recorded state: {e}"
                        ))
                    })?;
                    let stored = self
                        .session
                        .cas()
                        .put_bounded(&bytes, faktor_fs::MAX_MERGE_FILE_BYTES as usize)
                        .map_err(|e| ExecError::Internal(format!("run base blob of {rel}: {e}")))?;
                    // The materialized base must still match the recorded
                    // anchor: a drifted generation is a typed refusal, never
                    // a rollback blob for the wrong content.
                    if stored != *payload {
                        return Err(ExecError::WorkspaceDrift(format!(
                            "run base content of {rel} no longer matches its recorded anchor (expected {}, found {}); the generation moved",
                            payload.to_hex(),
                            stored.to_hex()
                        )));
                    }
                    (Some(stored.to_hex()), None)
                }
                EntryState::Symlink { target, .. } => (None, Some(target.clone())),
            };
            let decision = faktor_session::ledger::IntegrationPathTxn {
                path: rel.clone(),
                base_state,
                candidate_state,
                rollback_blob,
                rollback_link_target,
                canonical: true,
                state: faktor_session::ledger::IntegrationPathTxnState::Pending,
            };
            if !decision.canonical_ready() {
                return Err(ExecError::Internal(format!(
                    "landing decision of {rel} is not canonical-ready after construction"
                )));
            }
            decisions.push(decision);
        }
        Ok(decisions)
    }
}
