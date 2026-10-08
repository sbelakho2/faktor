//! TaskExecutor integration: rollback, blocked txns, verification records, cancel paths (mechanically split from `task_executor`).

use super::*;

impl TaskExecutor {
    /// Roll back every Applied path of a landing transaction: restore the
    /// base content ONLY while the owner path still holds OUR written
    /// candidate state; a later user edit is never overwritten (the path is
    /// marked [`faktor_session::ledger::IntegrationPathTxnState::RollbackConflict`]).
    pub(super) fn rollback_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        txn: &mut faktor_session::ledger::IntegrationTxnRow,
    ) -> Result<(), ExecError> {
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::RollingBack;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        let owner_root = PathBuf::from(&txn.owner_root);
        for i in (0..txn.paths.len()).rev() {
            if txn.paths[i].state != faktor_session::ledger::IntegrationPathTxnState::Applied {
                continue;
            }
            let path_txn = txn.paths[i].clone();
            match self.restore_base_path(&owner_root, &path_txn) {
                Ok(()) => {
                    txn.paths[i].state = faktor_session::ledger::IntegrationPathTxnState::RolledBack
                }
                Err(detail) => {
                    txn.paths[i].state =
                        faktor_session::ledger::IntegrationPathTxnState::RollbackConflict;
                    let line = format!("{}: {detail}", path_txn.path);
                    txn.conflicts.push(truncate_bytes(&line, 256));
                }
            }
            txn.at_ms = handle.now_ms();
            handle
                .ledger_integration_txn_set(txn)
                .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
            self.check_settlement_seam(CrashSeam::DuringRollback)?;
        }
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::RolledBack;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        Ok(())
    }

    /// Restore ONE applied path to its exact base state through the
    /// canonical restore CAS: the live state must still be the transaction's
    /// written candidate state (a later user edit is a typed Conflict and is
    /// preserved), and the exact kind/mode/target is restored from the
    /// recorded rollback material.
    pub(super) fn restore_base_path(
        &self,
        owner_root: &std::path::Path,
        path_txn: &faktor_session::ledger::IntegrationPathTxn,
    ) -> Result<(), String> {
        use faktor_fs::entry_state::EntryState;
        if !path_txn.canonical_ready() {
            return Err(format!(
                "rollback decision of {:?} is not a canonical entry-state row; refusing to restore from it",
                path_txn.path
            ));
        }
        let rel = std::path::Path::new(&path_txn.path);
        let material = match &path_txn.base_state {
            EntryState::Absent => Vec::new(),
            EntryState::Regular { payload, .. } => {
                let blob = path_txn
                    .rollback_blob
                    .as_deref()
                    .ok_or_else(|| "regular rollback has no base blob".to_string())?;
                let hash = FileHash::from_hex(blob)
                    .ok_or_else(|| format!("hostile rollback blob {blob:?}"))?;
                if hash != *payload {
                    return Err(
                        "rollback blob digest does not match the recorded base state".to_string(),
                    );
                }
                self.session
                    .cas()
                    .get_bounded(hash, faktor_fs::MAX_MERGE_FILE_BYTES as usize)
                    .map_err(|e| format!("rollback blob read: {e}"))?
                    .ok_or_else(|| "rollback blob missing from the CAS".to_string())?
            }
            EntryState::Symlink { target, .. } => target.clone(),
        };
        faktor_fs::entry_state::restore_tree_entry_cas(
            owner_root,
            rel,
            &path_txn.candidate_state,
            &path_txn.base_state,
            &material,
        )
        .map(|_| ())
        .map_err(|e| e.message)
    }

    /// Record a Blocked landing attempt (owner drift before the first
    /// write): a durable in-flight integration record with the typed reason
    /// and the staged sources, so nothing about the run is lost.
    pub(super) fn record_blocked_txn(
        &self,
        handle: &faktor_session::SessionHandle,
        prepared: &PreparedRunIntegration,
        reason: &str,
    ) -> Result<(), ExecError> {
        self.record_blocked_integration(handle, prepared, &[truncate_bytes(reason, 256)])
    }

    pub(super) fn record_blocked_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        prepared: &PreparedRunIntegration,
        conflicts: &[String],
    ) -> Result<(), ExecError> {
        let files_digest = changed_files_authority_digest(&prepared.changed);
        // A blocked record names its landing transaction when one exists
        // (the deterministic txn identity), and the exact run base /
        // candidate it refused to land. NOTHING is derived from
        // `sources.first()` anymore: the deprecated `base_revision` stays
        // `None`.
        let integration_txn_id = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        .map(|txn| txn.txn_id());
        let row = faktor_session::IntegrationRecordRow {
            run_id: prepared.run_id.clone(),
            task_id: prepared.task_id.raw(),
            base_revision: None,
            base_snapshot: Some(prepared.base_snapshot.clone()),
            run_base_snapshot: Some(prepared.base_snapshot.clone()),
            candidate_snapshot: Some(prepared.candidate_snapshot.clone()),
            landed_snapshot: None,
            proof_basis_digest: None,
            integration_txn_id,
            final_root: prepared.owner_root.to_string_lossy().into_owned(),
            final_snapshot_hash: String::new(),
            integrated_files: prepared
                .changed
                .iter()
                .take(faktor_session::MAX_INTEGRATION_FILES)
                .cloned()
                .collect(),
            integrated_file_count: prepared.changed.len() as u64,
            integrated_files_digest: files_digest,
            conflicts: conflicts
                .iter()
                .take(faktor_session::MAX_INTEGRATION_CONFLICTS)
                .cloned()
                .collect(),
            conflict_count: conflicts.len() as u64,
            sources: prepared
                .sources
                .iter()
                .take(faktor_session::MAX_INTEGRATION_SOURCES)
                .cloned()
                .collect(),
            source_count: prepared.sources.len() as u64,
            sources_digest: prepared.sources_digest.clone(),
            at_ms: handle.now_ms(),
        };
        handle
            .ledger_integration_record_set(&row)
            .map(|_| ())
            .map_err(|e| ExecError::Internal(format!("integration record write: {e}")))
    }

    /// Finalize the integration record with the FRESH whole-root digest
    /// taken after the landing (point 6). Idempotent for an already
    /// finalized identical landing.
    pub(super) fn finalize_integration_record(
        &self,
        handle: &faktor_session::SessionHandle,
        prepared: &PreparedRunIntegration,
        landed: &str,
        proof_basis_digest: &str,
    ) -> Result<(), ExecError> {
        if let Some(existing) = handle
            .ledger_integration_record_for_task(prepared.task_id.raw())
            .map_err(|e| ExecError::Internal(format!("integration record read: {e}")))?
        {
            if existing.final_snapshot_hash == landed
                && existing.sources_digest == prepared.sources_digest
                && existing.run_id == prepared.run_id
            {
                return Ok(());
            }
        }
        let files_digest = changed_files_authority_digest(&prepared.changed);
        // The deterministic identity of the landing transaction that
        // produced this snapshot (record-first rows always exist by the
        // time an integration is finalized; a `None` here is only possible
        // for a direct caller that never landed through a txn).
        let integration_txn_id = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        .map(|txn| txn.txn_id());
        let row = faktor_session::IntegrationRecordRow {
            run_id: prepared.run_id.clone(),
            task_id: prepared.task_id.raw(),
            // The deprecated overloaded field is never derived or populated
            // (the explicit identity fields below carry the real values).
            base_revision: None,
            base_snapshot: Some(prepared.base_snapshot.clone()),
            run_base_snapshot: Some(prepared.base_snapshot.clone()),
            candidate_snapshot: Some(prepared.candidate_snapshot.clone()),
            landed_snapshot: Some(landed.to_string()),
            proof_basis_digest: Some(proof_basis_digest.to_string()),
            integration_txn_id,
            final_root: prepared.owner_root.to_string_lossy().into_owned(),
            final_snapshot_hash: landed.to_string(),
            integrated_files: prepared
                .changed
                .iter()
                .take(faktor_session::MAX_INTEGRATION_FILES)
                .cloned()
                .collect(),
            integrated_file_count: prepared.changed.len() as u64,
            integrated_files_digest: files_digest,
            conflicts: Vec::new(),
            conflict_count: 0,
            sources: prepared
                .sources
                .iter()
                .take(faktor_session::MAX_INTEGRATION_SOURCES)
                .cloned()
                .collect(),
            source_count: prepared.sources.len() as u64,
            sources_digest: prepared.sources_digest.clone(),
            at_ms: handle.now_ms(),
        };
        handle
            .ledger_integration_record_set(&row)
            .map(|_| ())
            .map_err(|e| ExecError::Internal(format!("integration record write: {e}")))
    }

    /// Find-or-create the root verification record of one orchestrated run at
    /// the CURRENT revision, bound to the final integration snapshot
    /// (`tree_hash`), covering every acceptance criterion and carrying the
    /// COMPOSED verdict `status` (never the check-side status alone). A
    /// matching record is reused ONLY while its persisted proof basis is
    /// byte-identical to the current basis
    /// ([`SessionHandle::verification_record_reusable`]); a record with a
    /// different basis (a changed candidate snapshot, check set, task contract
    /// or revision) — or a legacy record without a basis — is NEVER reused
    /// and a fresh record is written. The fresh record lands WITH its
    /// environment fingerprint (carrying the proof-basis digest) and its
    /// candidate-proof reference, so the next settlement can consult the
    /// basis of a record this call created.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn find_or_create_root_verification_record(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        criteria: &[String],
        final_snapshot: &str,
        run: &faktor_agent::IntegratedRootVerification,
        prepared: &PreparedRunIntegration,
        status: VerificationStatus,
    ) -> Result<(VerificationRecordId, String), ExecError> {
        let revision = handle
            .task_revision(task_id)
            .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
        let covers = |r: &faktor_session::VerificationRecord| {
            criteria.iter().all(|c| {
                r.criteria
                    .iter()
                    .any(|cv| cv.criterion_key == *c && cv.passed)
            })
        };
        let basis = self
            .root_verification_proof_basis(handle, task_id, prepared, run)
            .await?;
        let basis_digest = canonical_proof_basis_digest(&basis);
        let mut candidates: Vec<faktor_session::VerificationRecord> = handle
            .list_verification_records(task_id)
            .map_err(|e| ExecError::Internal(format!("verification record list: {e}")))?
            .into_iter()
            .filter(|r| {
                r.status == status
                    && r.revision == revision
                    && r.tree_hash.as_deref() == Some(final_snapshot)
                    && (status != VerificationStatus::Passed || covers(r))
            })
            .collect();
        // Newest first: a newer basis-bound record is preferred, and a
        // divergent older record can never shadow it.
        candidates.sort_by_key(|r| std::cmp::Reverse(r.record_id));
        for existing in &candidates {
            match canonical_proof_reuse(handle, existing.record_id, &basis_digest)? {
                ProofReuse::Allowed => return Ok((existing.record_id, basis_digest)),
                ProofReuse::Refused { reason } => {
                    eprintln!(
                        "root verification record {} of task {task_id} is not reusable: {reason}; consulting older records / writing a fresh basis-bound record",
                        existing.record_id
                    );
                }
            }
        }
        let fingerprint = root_verification_fingerprint(handle, task_id, &basis)?;
        let candidate_proof_ref = root_verification_candidate_proof(
            handle,
            task_id,
            revision,
            final_snapshot,
            prepared,
            run,
        )?;
        let record = handle
            .create_verification_record_with_evidence(
                task_id,
                Some(final_snapshot.to_string()),
                run.criteria.clone(),
                run.checks.clone(),
                Vec::new(),
                Vec::new(),
                None,
                status,
                handle.now_ms(),
                Some(fingerprint),
                Some(candidate_proof_ref),
            )
            .map_err(|e| ExecError::Internal(format!("root verification record write: {e}")))?;
        Ok((record, basis_digest))
    }

    /// Resolve the CURRENT instruction basis of the session's durable
    /// workspace for the proof basis (A fail-closed). The tree is read
    /// TWICE through the resolver's (root, epoch) cache: a changed epoch
    /// between the reads is an unstable tree, and EVERY store/resolver
    /// failure is typed — never collapsed into "no applicable instructions".
    pub(super) fn resolve_instruction_basis(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Result<InstructionBasis, InstructionBasisError> {
        let row = handle.row().map_err(|e| match e.kind {
            faktor_core::ErrorKind::Malformed => InstructionBasisError::CorruptWorkspaceRelation {
                what: "session row".into(),
                detail: e.message,
            },
            _ => InstructionBasisError::StoreUnavailable {
                what: "session row".into(),
                detail: e.message,
            },
        })?;
        if row.workspace_id.raw() == 0 {
            return Err(InstructionBasisError::CorruptWorkspaceRelation {
                what: "session row".into(),
                detail: "workspace id 0 can never own an instruction tree".into(),
            });
        }
        let resolver = &self.agent.deps().instructions_resolver;
        let read = |pinned: Option<faktor_instructions::InstructionEpoch>| {
            resolver
                .resolve(row.workspace_id.raw(), pinned)
                .map_err(classify_rules_load_error)
        };
        match read(None)? {
            faktor_instructions::LoadedInstructions::Empty => {
                // The absence is only a valid basis while it is STABLE: a
                // second read that suddenly resolves a tree is a moving
                // world, not a no-instructions workspace.
                match read(None)? {
                    faktor_instructions::LoadedInstructions::Empty => {
                        Ok(InstructionBasis::NoApplicableInstructions)
                    }
                    faktor_instructions::LoadedInstructions::Loaded(_) => {
                        Err(InstructionBasisError::UnstableInstructionTree(
                            "the first read resolved no tree but the second resolved one".into(),
                        ))
                    }
                }
            }
            faktor_instructions::LoadedInstructions::Loaded(first) => {
                let faktor_instructions::LoadedInstructions::Loaded(second) = read(None)? else {
                    return Err(InstructionBasisError::UnstableInstructionTree(
                        "the first read resolved a tree but the second resolved none".into(),
                    ));
                };
                if first.epoch() != second.epoch() {
                    return Err(InstructionBasisError::UnstableInstructionTree(format!(
                        "the rule-tree epoch moved from {} to {} between the two reads of one basis",
                        first.epoch(),
                        second.epoch()
                    )));
                }
                Ok(InstructionBasis::Resolved {
                    epoch: first.epoch().as_u64(),
                })
            }
        }
    }

    /// The canonical proof basis of one orchestrated root verification: the
    /// exact task/revision/contract, candidate snapshot, integration sources,
    /// changed-file evidence, ordered check set, verifier version, PROBED
    /// tool versions (`rustc`/`cargo`/node/package managers + every check
    /// program + the custom verifier binary's digest), the verification
    /// environment projection (allowlisted fields, target triple, PATH-
    /// resolved executable identities), the resolved instruction epoch, the
    /// reviewer identity/model/payload digest and every immutable evidence
    /// digest contributing to a criterion PASS — the exact components a
    /// reusable record must match byte-for-byte. Built through the same
    /// bounded [`ProofBasis`] shape the session layer digests
    /// (`verification_record_reusable` compares the digests), and rebuilt
    /// immediately before every reuse consult so a tool/binary/environment
    /// change invalidates reuse in production, not only in synthetic tests.
    pub(super) async fn root_verification_proof_basis(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        prepared: &PreparedRunIntegration,
        run: &faktor_agent::IntegratedRootVerification,
    ) -> Result<ProofBasis, ExecError> {
        let revision = handle
            .task_revision(task_id)
            .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
            .ok_or_else(|| ExecError::Internal(format!("root task {task_id} missing")))?;
        let criteria: Vec<ProofBasisCriterion> = task
            .criteria()
            .into_iter()
            .take(faktor_session::task::MAX_PROOF_BASIS_ENTRIES)
            .map(|criterion| ProofBasisCriterion {
                criterion_id: criterion.id.to_string(),
                binding_digest: criterion.binding.as_ref().map(|b| b.content_digest()),
            })
            .collect();
        let checks: Vec<ProofBasisCheck> = run
            .checks
            .iter()
            .take(faktor_session::task::MAX_PROOF_BASIS_ENTRIES)
            .map(|check| ProofBasisCheck {
                check_id: check.check.clone(),
                program: check.program.clone(),
                args: check.args.clone(),
            })
            .collect();
        let changed_files_digest = changed_files_authority_digest(&prepared.changed);
        // Probes through the daemon's ONE supervisor (bounded, deadline-
        // enforced). A missing supervisor degrades every probe to an
        // explicit marker; the lists are NEVER silently empty.
        let check_programs: Vec<String> = run
            .checks
            .iter()
            .map(|check| check.program.clone())
            .collect();
        let report = crate::proof_probe::probe_proof_basis(
            self.agent.deps().supervisor.as_ref(),
            &check_programs,
        )
        .await;
        // The CURRENT resolved instruction generation (the epoch IS the
        // resolved rule-tree digest). FIX (A): the basis is FAIL-CLOSED —
        // a store failure, a corrupt workspace relation, an unreadable rule
        // file, an unstable tree or a resolver error REFUSES the proof
        // (creation AND reuse); only a stable read that genuinely resolves
        // no tree yields the epoch-less NoApplicableInstructions basis.
        let instruction_basis = self.resolve_instruction_basis(handle).map_err(|e| {
            ExecError::Internal(format!(
                "root verification proof basis refused: instruction basis: {e}"
            ))
        })?;
        let reviewer_digest = reviewer_proof_basis_digest(run);
        let evidence_digests = criterion_pass_evidence_digests(prepared, run);
        Ok(ProofBasis {
            task_id: task_id.raw(),
            task_revision: revision.raw(),
            task_contract_digest: acceptance_criteria_authority_digest(&task.acceptance_criteria),
            candidate_snapshot: prepared.candidate_snapshot.clone(),
            integration_sources_digest: prepared.sources_digest.clone(),
            changed_files_digest,
            checks,
            verification_impl_version: faktor_agent::runtime::VERIFICATION_IMPL_VERSION.to_string(),
            tool_versions: report.tools,
            env_projection: report.env_projection,
            instruction_epoch: instruction_basis.epoch(),
            criteria,
            reviewer_digest,
            evidence_digests,
        })
    }

    /// Drive the root task row across the machine's legal edges to
    /// `Verifying` (re-reading the revision before every edge). `Ok(false)`
    /// when the row cannot legally reach `Verifying` from its state
    /// (Blocked/terminal). Exact same edges the agent's gate driving uses.
    pub(super) fn route_root_to_verifying(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
    ) -> Result<bool, ExecError> {
        for _ in 0..8 {
            let task = handle
                .get_task(task_id)
                .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
                .ok_or_else(|| ExecError::Internal(format!("root task {task_id} missing")))?;
            let transition = match task.state {
                TaskState::Pending => TaskTransition::StartRunning,
                TaskState::Planning => TaskTransition::PlanComplete,
                TaskState::Running => TaskTransition::RequestVerification,
                TaskState::Waiting => TaskTransition::ResumeFromWaiting,
                TaskState::NeedsVerification => TaskTransition::StartVerification,
                TaskState::Verifying => return Ok(true),
                // Blocked is not silently unblocked here: the block is a
                // durable decision someone must resolve.
                TaskState::Blocked | TaskState::VerifiedComplete => return Ok(false),
                TaskState::Failed | TaskState::Cancelled => return Ok(false),
            };
            let revision = handle
                .task_revision(task_id)
                .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
            handle
                .transition_task(task_id, revision, transition, None)
                .map_err(|e| {
                    ExecError::Conflict(format!(
                        "root task {task_id} could not be routed to Verifying: {e}"
                    ))
                })?;
        }
        Err(ExecError::Internal(format!(
            "root task {task_id} routing to Verifying exceeded its bounded edge count"
        )))
    }

    // ------------------------------------------------- shadow helpers (P0-48)

    /// The registered worktree root of the session (the durable owner root
    /// `owner_root_of` mirrors the orchestrated path: workspace/worktree
    /// rows only, never a guessed path).
    pub(super) fn owner_root_of(
        &self,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
    ) -> Result<PathBuf, ExecError> {
        let row = handle.row()?;
        let wts = self
            .session
            .worktrees_of(row.workspace_id)?
            .into_iter()
            .filter(|w| (w.id as u64) == row.worktree_id.raw())
            .map(|w| PathBuf::from(w.path))
            .collect::<Vec<_>>();
        wts.first().cloned().ok_or_else(|| {
            ExecError::Conflict(format!(
                "session {parent} has no registered worktree row; shadowed mutating runs need a real owner worktree"
            ))
        })
    }

    /// Deterministic settlement of a durable shadow left by an interrupted
    /// drive BEFORE a new run begins:
    /// - shadow + `Failed`/`Cancelled` task row → discard;
    /// - shadow + `VerifiedComplete` task row → the owner landing must be
    ///   re-driven by the async settlement pipeline; a new run is refused
    ///   (typed Conflict) until the shadow retires;
    /// - shadow + non-terminal task row WITHOUT a shadow-world proof and no
    ///   live turn → the crashed drive's partial shadow is garbage → discard;
    /// - shadow + non-terminal task row WITH a shadow-world proof (the drive
    ///   certified the shadow but integration has not run) → the async
    ///   settlement owns the landing; a new run is refused (typed Conflict);
    /// - shadow + live turn → the previous drive is still running; a second
    ///   run cannot begin (typed Conflict).
    pub(super) fn settle_existing_shadow(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
    ) -> Result<(), ExecError> {
        let shadows = self.shadows.as_ref().ok_or_else(|| {
            ExecError::Internal("shadow settlement requires the shadow service".into())
        })?;
        let Some(row) = shadows.active_shadow(parent)? else {
            return Ok(());
        };
        if !row.state.is_live() {
            return Ok(());
        }
        let task_id = handle.task_id()?;
        let state = handle.get_task(task_id)?.map(|t| t.state);
        match state {
            Some(TaskState::Failed) | Some(TaskState::Cancelled) => {
                shadows.discard(parent)?;
            }
            Some(TaskState::VerifiedComplete) => {
                return Err(ExecError::Conflict(format!(
                    "session {parent} carries the verified shadow {} whose owner integration must be re-settled before a new run starts",
                    row.shadow_id
                )));
            }
            Some(_) => {
                // Non-terminal task row: is the interrupted drive still
                // LIVE on the session? A durable ACTIVE turn record is the
                // precise marker (the crashed drive's record stays active;
                // an operator abort resolves it). With no live record the
                // crashed drive's partial shadow is garbage UNLESS the drive
                // certified the shadow world (a passing record at the
                // current revision): that proof marks a recoverable run
                // whose owner integration is still pending.
                let mid_turn = self
                    .session
                    .store()
                    .active_turn_record(parent)
                    .map(|r| r.is_some())
                    .map_err(|e| {
                        ExecError::from(classify_session_read(
                            "active turn record of the shadowed session",
                            faktor_core::Error::from(faktor_session::SessionError::from(e)),
                        ))
                    })?;
                if mid_turn {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} has a live shadow {} and an active drive; the interrupted run must be resumed or cancelled before a new shadowed task starts",
                        row.shadow_id
                    )));
                }
                // FIX 2: both reads feed a gate decision — a store failure is
                // an error, never "the shadow world was not proven".
                let revision = handle.task_revision(task_id).map_err(|e| {
                    ExecError::from(classify_session_read(
                        "task revision of the interrupted shadow run",
                        e.into(),
                    ))
                })?;
                let records = handle.list_verification_records(task_id).map_err(|e| {
                    ExecError::from(classify_session_read(
                        "verification records of the interrupted shadow run",
                        e.into(),
                    ))
                })?;
                let shadow_world_proven = records.iter().any(|record| {
                    record.status == VerificationStatus::Passed && record.revision == revision
                });
                if shadow_world_proven {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} carries the certifiable shadow {} whose owner integration is still pending; re-settle the run before starting a new one",
                        row.shadow_id
                    )));
                }
                // Applied-but-unintegrated writes are NOT garbage: the
                // durable tool ledger still says `applied`, so discarding the
                // shadow silently destroyed them (a later doctor showed a
                // clean tree). Retain and surface a typed obligation.
                // P2-VERIFY: this read gates a DISCARD. A store failure or a
                // corrupt observation row is a typed refusal — never "no
                // unintegrated changes" (which silently destroyed applied
                // writes).
                let facts = handle.memory_facts().map_err(|e| {
                    ExecError::from(classify_session_read(
                        "memory facts of the interrupted shadow run",
                        e,
                    ))
                })?;
                let mut unintegrated = false;
                for (kind, key, value) in &facts {
                    if kind != "verification" || !key.contains("root:") {
                        continue;
                    }
                    let parsed =
                        serde_json::from_str::<serde_json::Value>(value).map_err(|e| {
                            ExecError::Malformed(format!(
                                "verification observation {kind}/{key} of shadow {} is undecodable ({e}); refusing to discard the shadow",
                                row.shadow_id
                            ))
                        })?;
                    let changed = parsed
                        .get("changed")
                        .and_then(|c| c.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(false);
                    if changed {
                        unintegrated = true;
                        break;
                    }
                }
                if unintegrated {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} carries applied-but-unintegrated changes in shadow {}; re-run settlement or explicitly discard the shadow before a new run",
                        row.shadow_id
                    )));
                }
                shadows.discard(parent)?;
            }
            None => {
                shadows.discard(parent)?;
            }
        }
        Ok(())
    }

    /// Post-drive hook of a shadowed run (spawned with the detached drive):
    /// settle the run through the ONE pipeline (a live shadow integrates
    /// prepare -> verify -> land -> steps -> complete). The runner only
    /// records step outcomes; completion goes through the durable gate.
    ///
    /// When the drive ended BEFORE the owner integration could run (the
    /// verifier may still certify in the background, or an integration
    /// conflict awaits the user's drift resolution), a BOUNDED watcher
    /// re-runs the decision on the next poll instead of leaving the shadow
    /// live forever.
    pub(super) async fn after_shadowed_drive(self: &Arc<Self>, parent: SessionId) {
        let run_id = format!("tx-session-{}", parent.raw());
        match self
            .settle_run(RunSettlement::InSession { parent, run_id })
            .await
        {
            Ok(outcome) => {
                if matches!(
                    outcome.finalize.as_ref().map(|f| &f.action),
                    Some(ShadowFinalizeAction::Retained)
                        | Some(ShadowFinalizeAction::IntegrationBlocked)
                ) {
                    self.watch_shadow_settle(parent);
                }
            }
            Err(e) => eprintln!("shadowed-run settlement failed for session {parent}: {e}"),
        }
    }

    /// Settle-path queue authority (boundary race): a turn of `parent` just
    /// finished or was cancelled, and its durable queue head must never be
    /// left without a runner. When the session still carries a non-terminal
    /// queue row, ensure a runner under THIS registry:
    ///
    /// - a LIVE runner is ARMED for one more bounded pass by the runtime's
    ///   per-session gate (never a second concurrent drive — the head is
    ///   claimed exactly once);
    /// - otherwise a fresh registry-owned runner is spawned. The spawned
    ///   future is cheap when a runner turns out to be live (it only arms).
    ///
    /// A shut-down registry refuses the spawn; the durable row IS the
    /// runnable marker and keeps it pending for
    /// [`Self::recover_pending_queues`] on the next executor. Bounded by
    /// construction: a kick never polls, it only starts/arms one runner
    /// whose own wait is bounded by the turn budget.
    pub(super) fn kick_pending_queue(self: &Arc<Self>, parent: SessionId) {
        // A store read error is NEVER treated as "no pending queue" (audited):
        // the kick is not skipped while a durable head may be waiting. An
        // unreadable head (or handle) is logged typed, recorded as a durable
        // retry marker on the session, and the runner is spawned anyway — the
        // runner itself is idempotent (it arms a live runner or exits on an
        // empty queue), so an over-kick costs nothing while a skipped kick
        // would strand a queued prompt until the next restart.
        let pending = match self.session.get_session(parent) {
            Ok(Some(handle)) => match handle.queued_prompt_count() {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(
                        session = %parent,
                        error = %e.message,
                        "queue settle-path kick could not read the durable queue head; treating \
                         it as NON-EMPTY (the kick is never skipped on a read error): {e}"
                    );
                    self.note_queue_kick_read_failure(&handle, &e);
                    1
                }
            },
            Ok(None) => 0,
            Err(e) => {
                tracing::error!(
                    session = %parent,
                    error = %e.message,
                    "queue settle-path kick could not open the session; treating the head as \
                     NON-EMPTY and kicking anyway: {e}"
                );
                1
            }
        };
        if pending == 0 {
            return;
        }
        let agent = self.agent.clone();
        let spawned = self
            .drives
            .spawn(format!("tx-queue-{}", parent.raw()), async move {
                agent.run_session_queue(parent).await;
            });
        if !spawned {
            tracing::warn!(
                session = %parent,
                "queue head pending after its turn settled but the drive registry is \
                 shut down; the durable row stays pending for recovery"
            );
        }
    }

    /// Durable retry marker of a queue-read failure on the settle-path kick:
    /// a `CrashDetected` self-transition naming the site, so the operator
    /// surface can see that a kick decision was made on an unreadable head
    /// (the kick itself still happened). Best-effort by construction.
    pub(super) fn note_queue_kick_read_failure(
        &self,
        handle: &faktor_session::SessionHandle,
        err: &faktor_core::Error,
    ) {
        const SITE: &str = "task_executor.kick_pending_queue.read_head";
        let state = match handle.state() {
            Ok(state) => state,
            Err(e) => {
                tracing::error!(
                    session = %handle.id(),
                    "queue-kick read-failure audit skipped: session state unreadable: {e}"
                );
                return;
            }
        };
        let payload = serde_json::json!({
            "durable_write_failure": {
                "site": SITE,
                "error": err.message.chars().take(1024).collect::<String>(),
            }
        });
        if let Err(e) = handle.force_append_event(
            faktor_core::event::EventKind::CrashDetected,
            state,
            None,
            Some(payload),
        ) {
            tracing::error!(
                session = %handle.id(),
                "queue-kick read-failure audit could not be journaled: {e}"
            );
        }
    }

    /// Startup/relaunch queue recovery (boundary race): every session whose
    /// durable queue still carries a non-terminal row gets a runner under
    /// the drive registry — no new submit required. The durable rows ARE
    /// the runnable marker (`sessions_with_pending_queues`); idempotent, a
    /// live runner is armed, never duplicated, and a shut-down registry
    /// leaves the rows pending for the next executor.
    pub fn recover_pending_queues(self: &Arc<Self>) {
        let sessions = match self.session.store().sessions_with_pending_queues() {
            Ok(sessions) => sessions,
            Err(e) => {
                // Loud and typed: a scan failure means NO session could be
                // kicked this boot; the durable rows stay pending for the
                // next recovery and are never reported as drained.
                tracing::error!(
                    error = %e,
                    "queue recovery scan failed; every durable queue row stays pending for the \
                     next recovery: {e}"
                );
                return;
            }
        };
        for session in sessions {
            self.kick_pending_queue(session);
        }
    }

    /// Bound of the post-drive shadow watcher: it may retry the durable
    /// finalize for at most this long while the session's shadow row is
    /// still LIVE (a non-terminal task row, or an IntegrationBlocked row
    /// waiting for the user to resolve drift). After it gives up, the
    /// deterministic settlement on the next run start (and every
    /// operator-facing [`Self::cancel_run`]) is the backstop — nothing is
    /// ever lost.
    const SHADOW_WATCH_DEADLINE: Duration = Duration::from_secs(120);
    /// Poll interval of the post-drive shadow watcher.
    const SHADOW_WATCH_INTERVAL: Duration = Duration::from_millis(250);

    /// Bounded re-arm of the post-drive settlement: a shadowed run whose
    /// drive ended with a LIVE shadow row (a non-terminal task row —
    /// verification in flight — or an IntegrationBlocked row awaiting the
    /// user's drift resolution) is re-settled on every poll until the row
    /// retires or the deadline passes. Late VerifiedComplete completions
    /// therefore integrate (and any then-runnable contract steps execute)
    /// and operator cancels discard the shadow without requiring a new run
    /// start. Every decision stays on the durable rows (settlement is
    /// per-state idempotent), so a crash of the watcher is recovered by the
    /// next run's deterministic settlement.
    pub(super) fn watch_shadow_settle(self: &Arc<Self>, parent: SessionId) {
        let exec = self.clone();
        // Registry-owned watcher (audit spawn ownership): never a dropped
        // JoinHandle. A shut-down registry refuses the watcher — the durable
        // settlement on the next run start is the documented backstop.
        let _ = self
            .drives
            .spawn(format!("tx-session-{}", parent.raw()), async move {
                let deadline = Instant::now() + Self::SHADOW_WATCH_DEADLINE;
                // Backoff: the watcher re-settles a parked run, and an
                // unchanged refusal must not be re-driven at the base tick
                // for the whole 120s deadline (480 no-op reviewer attempts).
                let mut wait = Self::SHADOW_WATCH_INTERVAL;
                loop {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(4));
                    if Instant::now() >= deadline {
                        return;
                    }
                    let run_id = format!("tx-session-{}", parent.raw());
                    match exec
                        .settle_run(RunSettlement::InSession { parent, run_id })
                        .await
                    {
                        Ok(outcome)
                            if !matches!(
                                outcome.finalize.as_ref().map(|f| &f.action),
                                Some(ShadowFinalizeAction::Retained)
                                    | Some(ShadowFinalizeAction::IntegrationBlocked)
                            ) =>
                        {
                            // Integrated/discarded/no-live-shadow: nothing left
                            // to watch.
                            return;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!(
                                "shadowed-run watch settlement failed for session {parent}: {e}"
                            );
                            return;
                        }
                    }
                }
            });
    }

    /// Cancel ONE task run of the session, durably and exactly once:
    ///
    /// - an in-session run (linkage row) has its drive op ABORTED first
    ///   (queued prompts kill their queue row; live turns land the session
    ///   ReadyForNextTurn), then the session's task row is transitioned to
    ///   `Cancelled` (legal from every non-terminal state) and any shadow
    ///   of the run is discarded through the durable finalize;
    /// - an orchestrated run has every non-terminal child sent a durable
    ///   Cancel control through the runtime's exactly-once queue, and its
    ///   root task row (the cap/criteria row, when one exists) is
    ///   transitioned the same way.
    ///
    /// Typed refusals: unknown runs are `NotFound`, already-terminal runs
    /// are `Conflict` (a cancelled run is never cancelled twice), and a
    /// stale task-row revision (a concurrent verifier won the race) is a
    /// `Conflict` naming the run — retrying is the only forward path.
    pub fn cancel_run(self: &Arc<Self>, parent: SessionId, run_id: &str) -> Result<(), ExecError> {
        if run_id.is_empty()
            || run_id.len() > MAX_RUN_ID_CHARS
            || !run_id.is_ascii()
            || run_id.contains('/')
        {
            return Err(ExecError::NotFound(format!("task run {run_id:?}")));
        }
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let facts = parent_facts(&handle)?;
        let run_row = facts
            .iter()
            .find(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == run_id);
        if let Some((_, _, value)) = run_row {
            let row = TaskRunRow::decode(value)
                .map_err(|m| ExecError::Internal(format!("stored run row {run_id}: {m}")))?;
            return self.cancel_in_session_run(parent, &handle, &row);
        }
        let has_plan = facts
            .iter()
            .any(|(kind, key, _)| kind == PLAN_ROW_KIND && key == run_id);
        let children = OrchestratorRuntime::registry_rows(self.session.clone(), parent, run_id)?;
        if has_plan || !children.is_empty() {
            return self.cancel_orchestrated_run(&handle, run_id, &children);
        }
        Err(ExecError::NotFound(format!(
            "task run {run_id} under session {parent}"
        )))
    }

    /// Cancel one in-session run: abort its drive op, cancel the task row,
    /// discard any shadow (see [`Self::cancel_run`]).
    pub(super) fn cancel_in_session_run(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
        row: &TaskRunRow,
    ) -> Result<(), ExecError> {
        let task_id = handle.task_id()?;
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?;
        if task.as_ref().is_some_and(|t| t.state.is_terminal()) {
            return Err(ExecError::Conflict(format!(
                "task run {} is already terminal; a cancelled run is never cancelled twice",
                row.run_id
            )));
        }
        if let Some(op) = row.op_id {
            self.agent
                .abort_op(parent, Some(OpId::new(op)))
                .map_err(|e| ExecError::Internal(format!("abort of run op {op}: {}", e.message)))?;
        }
        if let Some(_task) = &task {
            let rev = handle
                .task_revision(task_id)
                .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
            handle
                .transition_task(task_id, rev, TaskTransition::Cancel, None)
                .map_err(|e| {
                    ExecError::Conflict(format!(
                        "task-row cancel of run {}: {e} (a concurrent verifier may have won; retry the cancel)",
                        row.run_id
                    ))
                })?;
        }
        // Cancelled is terminal: the durable finalize discards any shadow.
        if let Err(e) = self.finalize_shadow_run(parent) {
            eprintln!("shadow discard after cancel of run {}: {e}", row.run_id);
        }
        Ok(())
    }

    /// Cancel one orchestrated run: durable Cancel controls on every
    /// non-terminal child + the root task row (see [`Self::cancel_run`]).
    pub(super) fn cancel_orchestrated_run(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        children: &[crate::runtime::ChildRuntime],
    ) -> Result<(), ExecError> {
        if !children
            .iter()
            .any(|c| !matches!(c.state, ChildState::Done | ChildState::Cancelled))
        {
            return Err(ExecError::Conflict(format!(
                "task run {run_id} has no non-terminal children; nothing to cancel"
            )));
        }
        for c in children {
            if matches!(c.state, ChildState::Done | ChildState::Cancelled) {
                continue;
            }
            self.orchestrator
                .control_child(&c.child_id, faktor_session::child::ChildControl::Cancel)
                .map_err(|e| match e {
                    ExecError::NotFound(m) => ExecError::Internal(format!(
                        "child {} of the cancelled run {run_id} is unknown to the runtime: {m}",
                        c.child_id
                    )),
                    ExecError::Conflict(m) => {
                        ExecError::Conflict(format!("child cancel of {}: {m}", c.child_id))
                    }
                    other => ExecError::Internal(format!(
                        "child cancel of {} failed: {other}",
                        c.child_id
                    )),
                })?;
        }
        let task_id = handle.task_id()?;
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?;
        if let Some(t) = task {
            if !t.state.is_terminal() {
                let rev = handle
                    .task_revision(task_id)
                    .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
                handle
                    .transition_task(task_id, rev, TaskTransition::Cancel, None)
                    .map_err(|e| {
                        ExecError::Conflict(format!(
                            "root task-row cancel of run {run_id}: {e} (a concurrent verifier may have won; retry the cancel)"
                        ))
                    })?;
            }
        }
        Ok(())
    }

    /// The durable lifecycle decision of a shadowed run (P0-48/49): read the
    /// session's shadow row + task row and act ONCE per terminal state:
    ///
    /// - `Failed`/`Cancelled` → discard the shadow;
    /// - `VerifiedComplete` → RETAINED: the owner landing is owned by the
    ///   async settlement pipeline ([`Self::settle_shadowed_in_session`]);
    ///   a completed task can only have reached that state through a
    ///   successful landing, so this path is the crash window between the
    ///   completion gate and the shadow retirement, recovered by the next
    ///   settlement;
    /// - any non-terminal state → retained (the drive may continue or the
    ///   settlement may still integrate; see [`Self::watch_shadow_settle`]
    ///   and every next run's deterministic settlement).
    ///
    /// `Ok(None)` when no live shadow exists (plain runs). This function
    /// NEVER lands content: the single commitment engine is the
    /// transactional [`Self::land_verified_integration`] pipeline.
    pub fn finalize_shadow_run(
        self: &Arc<Self>,
        parent: SessionId,
    ) -> Result<Option<ShadowFinalize>, ExecError> {
        let Some(shadows) = self.shadows() else {
            return Ok(None);
        };
        let Some(row) = shadows.active_shadow(parent)? else {
            return Ok(None);
        };
        if !row.state.is_live() {
            return Ok(None);
        }
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle.task_id()?;
        let Some(task) = handle.get_task(task_id)? else {
            return Ok(None);
        };
        match task.state {
            TaskState::Failed | TaskState::Cancelled => {
                shadows
                    .discard(parent)
                    .map_err(|e| ExecError::from_shadow("shadow discard", e))?;
                Ok(Some(ShadowFinalize {
                    action: ShadowFinalizeAction::Discarded,
                    merged: Vec::new(),
                    rejected: Vec::new(),
                    conflicts: Vec::new(),
                }))
            }
            _ => Ok(Some(retained_shadow_finalize())),
        }
    }

    // ------------------------------------------------------ tournament entry

    /// Start a multi-candidate implementation tournament (N = 2..=4): the
    /// SAME goal and the SAME criteria are fanned out to N ISOLATED
    /// candidate worktrees through the existing executor/assignment
    /// machinery (an N-item plan of `Implementation` items under
    /// `OwnershipSpec::IsolatedWorktree`), and the durable
    /// `TournamentStarted` ledger anchor is written on the parent session.
    /// The candidate child ids are the deterministic plan-order ids
    /// (`child-0..child-{n-1}`) the assignment compile mints; the receipt
    /// returns them plus the orchestrated run id whose registry rows carry
    /// the candidate drives.
    pub fn start_tournament(
        self: &Arc<Self>,
        parent: SessionId,
        goal: &str,
        criteria: &[String],
        n: usize,
        mutation_mode: Option<MutationMode>,
    ) -> Result<TournamentReceipt, ExecError> {
        self.start_tournament_with(
            parent,
            TournamentStartRequest {
                goal: goal.to_string(),
                criteria: criteria.to_vec(),
                n,
                mutation_mode,
                ..Default::default()
            },
        )
    }

    /// [`Self::start_tournament`] with the full request (model, budgets,
    /// ceilings, files, isolated root, crash seam). The tournament engine
    /// enforces the candidate band and the criterion bounds; the task
    /// start validates the plan through the ONE task-start authority.
    pub fn start_tournament_with(
        self: &Arc<Self>,
        parent: SessionId,
        req: TournamentStartRequest,
    ) -> Result<TournamentReceipt, ExecError> {
        req.validate()?;
        let criteria = build_tournament_criteria(&req.criteria)?;
        let tournament_id = format!(
            "tour-{:016x}",
            self.session
                .try_next_op_id()
                .map_err(|e| ExecError::from(faktor_core::Error::from(e)))?
                .raw()
        );
        let mut tournament = crate::tournament::Tournament::new(
            &tournament_id,
            "pending-run",
            &req.goal,
            criteria,
            req.n,
        )
        .map_err(tournament_exec_error)?;
        // All candidates receive the byte-identical goal summary and the
        // byte-identical criterion ids/specs: the assertion is a typed
        // refusal BEFORE any child spawns (a drift never fans out).
        let canonical = crate::tournament::canonical_criteria_text(&tournament.criteria);
        let criterion_specs: Vec<String> =
            tournament.criteria.iter().map(|c| c.spec.clone()).collect();
        let work_items: Vec<WorkItem> = (0..req.n)
            .map(|i| {
                let mut item = WorkItem::new(
                    format!("candidate-{i}"),
                    canonical.clone(),
                    WorkKind::Implementation,
                );
                item.acceptance_checks = criterion_specs.clone();
                item
            })
            .collect();
        crate::tournament::assert_candidates_identical(&work_items, &tournament.criteria)
            .map_err(tournament_exec_error)?;
        let run_request = TaskRunRequest {
            goal: req.goal.clone(),
            work_items,
            model: req.model,
            max_tokens: req.max_tokens,
            max_cost_micro: req.max_cost_micro,
            criteria: req.criteria.clone(),
            mutation_mode: req.mutation_mode,
            files: req.files,
            parent_caps: req.parent_caps,
            ceilings: req.ceilings,
            isolated_root: req.isolated_root,
            crash_seam: req.crash_seam,
            ..Default::default()
        };
        let receipt = self.start_task(parent, run_request)?;
        if receipt.mode != TaskRunMode::Orchestrated {
            return Err(ExecError::Internal(
                "a tournament start must dispatch to the orchestrated child path".into(),
            ));
        }
        tournament.run_family = receipt.run_id.clone();
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        tournament
            .persist_started(&handle)
            .map_err(tournament_exec_error)?;
        Ok(TournamentReceipt {
            tournament_id,
            run_id: receipt.run_id,
            candidates: tournament
                .candidates
                .iter()
                .map(|c| c.child_id.clone())
                .collect(),
        })
    }

    /// Read ONE tournament's durable state (reconstructed from the ledger)
    /// with RUNNING candidates refreshed from the run's registry rows
    /// (location, base revision, observed child state). Unknown ids are
    /// typed `NotFound`.
    pub fn tournament_state(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
    ) -> Result<crate::tournament::Tournament, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        crate::tournament::refresh_candidates_from_registry(&self.session, parent, &mut tournament)
            .map_err(tournament_exec_error)?;
        Ok(tournament)
    }

    /// Every reconstructed tournament of one session, oldest first.
    pub fn tournaments_of(
        self: &Arc<Self>,
        parent: SessionId,
    ) -> Result<Vec<crate::tournament::Tournament>, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        crate::tournament::Tournament::reopen(&handle).map_err(tournament_exec_error)
    }

    /// Build the settlement skeleton of ONE candidate from its durable
    /// state: the DERIVED check set of the tournament, the candidate's
    /// worktree/base revision from the run's registry row, and its observed
    /// terminal state. The caller attaches the verification record, the
    /// independent review and the measured cost/wall axes. A candidate that
    /// has not settled (`Running`) is a typed Conflict.
    pub fn candidate_settlement(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
        child_id: &str,
        reason: &str,
    ) -> Result<crate::tournament::CandidateSettlement, ExecError> {
        let tournament = self.tournament_state(parent, tournament_id)?;
        let candidate = tournament
            .candidates
            .iter()
            .find(|c| c.child_id == child_id)
            .ok_or_else(|| {
                ExecError::NotFound(format!(
                    "candidate {child_id} of tournament {tournament_id}"
                ))
            })?;
        if matches!(
            candidate.state,
            crate::tournament::CandidateState::Running
                | crate::tournament::CandidateState::Discarded
        ) {
            return Err(ExecError::Conflict(format!(
                "candidate {child_id} of tournament {tournament_id} has not settled (state {:?})",
                candidate.state
            )));
        }
        Ok(crate::tournament::CandidateSettlement {
            child_id: candidate.child_id.clone(),
            worktree: candidate.worktree.clone(),
            base_revision: candidate.base_revision.clone(),
            state: candidate.state,
            verification: None,
            verification_pass: None,
            checks: tournament.check_specs(),
            review: None,
            cost_micro: 0,
            wall_ms: 0,
            reason: reason.to_string(),
        })
    }

    /// Settle ONE candidate: validate the settlement against the durable
    /// tournament (byte-identical derived check set, independent reviewer),
    /// then persist the `CandidateSettled` ledger row.
    pub fn settle_tournament_candidate(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
        settlement: crate::tournament::CandidateSettlement,
    ) -> Result<crate::tournament::Tournament, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        tournament
            .settle_candidate(settlement.clone())
            .map_err(tournament_exec_error)?;
        tournament
            .persist_settlement(&handle, &settlement)
            .map_err(tournament_exec_error)?;
        Ok(tournament)
    }

    /// Decide ONE tournament with the documented deterministic ordering
    /// (verification pass > review rank > cost > ordinal): persist the
    /// `TournamentDecided` audit row FIRST, then discard every loser
    /// (registry row settled terminal, isolated worktree directory + row
    /// removed). The winner's worktree is only PROPOSED — integration stays
    /// the explicit approved-merge path and is never called here.
    pub fn decide_tournament(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
    ) -> Result<crate::tournament::TournamentDecision, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        crate::tournament::refresh_candidates_from_registry(&self.session, parent, &mut tournament)
            .map_err(tournament_exec_error)?;
        let decision = tournament.decide().map_err(tournament_exec_error)?;
        tournament
            .persist_decision(
                &handle,
                faktor_session::TOURNAMENT_OUTCOME_DECIDED,
                &decision.rationale,
            )
            .map_err(tournament_exec_error)?;
        for (loser_id, _why) in &decision.discarded {
            if let Some(candidate) = tournament
                .candidates
                .iter()
                .find(|c| &c.child_id == loser_id)
            {
                crate::tournament::discard_candidate_worktree(
                    &self.session,
                    &handle,
                    &tournament.run_family,
                    candidate,
                )
                .map_err(tournament_exec_error)?;
            }
        }
        Ok(decision)
    }

    /// Abort ONE tournament: every candidate is discarded (registry rows
    /// settled terminal; isolated worktrees removed), the terminal
    /// `TournamentDecided { outcome: aborted }` audit row records WHY, and
    /// no winner is proposed.
    pub fn abort_tournament(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
        reason: &str,
    ) -> Result<crate::tournament::Tournament, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        crate::tournament::refresh_candidates_from_registry(&self.session, parent, &mut tournament)
            .map_err(tournament_exec_error)?;
        tournament.abort(reason).map_err(tournament_exec_error)?;
        let rationale = if reason.trim().is_empty() {
            "aborted by operator; every candidate discarded".to_string()
        } else {
            reason.to_string()
        };
        tournament
            .persist_decision(
                &handle,
                faktor_session::TOURNAMENT_OUTCOME_ABORTED,
                &rationale,
            )
            .map_err(tournament_exec_error)?;
        for candidate in &tournament.candidates {
            crate::tournament::discard_candidate_worktree(
                &self.session,
                &handle,
                &tournament.run_family,
                candidate,
            )
            .map_err(tournament_exec_error)?;
        }
        Ok(tournament)
    }
}
