//! Tangerine merge-time semantic preflight (audits 99 / Tangerine-11).
//!
//! The preflight runs on the ASYNC merge path with the descriptor handshake
//! awaited, a real staged-change-set candidate identity, a bounded awaited
//! delta (a Pending provider is never treated as absence) and a task-policy
//! requirement that blocks the merge when compiler-exact verification is
//! unavailable. Provider absence/failure stays ADVISORY unless the child's
//! durable identity says otherwise.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use faktor_core::cancellation::CancellationToken;
use faktor_core::id::{SessionId, WorkspaceId};
use faktor_semantic::{
    SemanticCall, SemanticCapabilities, SemanticDelta, SemanticDeltaKind, SemanticDeltaRequest,
    SemanticOp, SemanticSelection, SemanticSnapshotId, SemanticSnapshotRequest,
    GENERIC_FALLBACK_ID, SEMANTIC_SCHEMA_VERSION,
};

use crate::runtime::{ExecError, OrchestratorRuntime};

use super::{truncate, ChangeSet, SEMANTIC_PREFLIGHT_MAX_CONFLICTS};

/// Test override for the merge-preflight wall bound (0 = production
/// default); module scope so both the impl and the tests can reach it.
#[cfg(test)]
pub(crate) static SEMANTIC_MERGE_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl OrchestratorRuntime {
    const SEMANTIC_MERGE_PREFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// The effective preflight wall bound (test-overridable through
    /// [`SEMANTIC_MERGE_TIMEOUT_OVERRIDE_MS`]).
    #[cfg(test)]
    fn merge_preflight_timeout() -> std::time::Duration {
        let override_ms =
            SEMANTIC_MERGE_TIMEOUT_OVERRIDE_MS.load(std::sync::atomic::Ordering::SeqCst);
        if override_ms > 0 {
            return std::time::Duration::from_millis(override_ms);
        }
        Self::SEMANTIC_MERGE_PREFLIGHT_TIMEOUT
    }

    #[cfg(not(test))]
    fn merge_preflight_timeout() -> std::time::Duration {
        Self::SEMANTIC_MERGE_PREFLIGHT_TIMEOUT
    }

    /// TRUE when the merging child's durable identity requires compiler-exact
    /// semantic verification (task policy). ANY failure to read that durable
    /// policy — locate failure, missing session, missing or corrupt identity
    /// row, store error — is a typed refusal (audit P0-POLICY): an unreadable
    /// policy is NEVER silently downgraded to advisory. Only an identity row
    /// that is present and decodes cleanly may answer `false`.
    fn semantic_merge_required(&self, cs: &ChangeSet) -> Result<bool, ExecError> {
        let (_parent, _run, child) = self.locate_child(&cs.child_id).map_err(|e| {
            ExecError::SemanticRequired(format!(
                "the semantic merge policy for child {} is unreadable: {e}",
                cs.child_id
            ))
        })?;
        let child_session = SessionId::try_from(child.session_id).map_err(|e| {
            ExecError::SemanticRequired(format!(
                "child {} carries session id {} in the semantic merge policy: {}",
                cs.child_id, child.session_id, e.message
            ))
        })?;
        let handle = self
            .manager
            .get_session(child_session)
            .map_err(|e| {
                ExecError::SemanticRequired(format!(
                    "child {} session read failed while reading the semantic merge policy: {e}",
                    cs.child_id
                ))
            })?
            .ok_or_else(|| {
                ExecError::SemanticRequired(format!(
                    "child {} names session {} which does not exist; the semantic merge policy is unreadable",
                    cs.child_id, child_session
                ))
            })?;
        match handle.orchestrator_child_identity_get() {
            Ok(Some(identity)) => Ok(identity.require_semantic_delta),
            Ok(None) => Err(ExecError::SemanticRequired(format!(
                "child {} has no durable identity row; the semantic merge policy is unreadable and is never treated as advisory",
                cs.child_id
            ))),
            Err(e) => Err(ExecError::SemanticRequired(format!(
                "child {} identity read failed while reading the semantic merge policy ({e}); refusing to treat the requirement as advisory",
                cs.child_id
            ))),
        }
    }

    pub(crate) async fn semantic_merge_preflight(
        &self,
        parent: SessionId,
        run: &str,
        cs: &ChangeSet,
        approved: &[PathBuf],
    ) -> Result<(), ExecError> {
        let required = self.semantic_merge_required(cs)?;
        let registry = self.agent.deps().semantic.clone();
        // The handshake is AWAITED before selection: a real asynchronous
        // provider is never silently skipped because an unrelated earlier
        // operation happened not to fetch its descriptor.
        let cancel = CancellationToken::new();
        let selection = registry
            .select_validated(&SemanticCapabilities::DELTA, SemanticOp::Delta, &cancel)
            .await;
        let provider = match selection {
            SemanticSelection::Provider(provider) if provider.capabilities().compose_delta => {
                // Fidelity requirement (audit Tangerine-3/4): a REQUIRED
                // merge verification only trusts compiler-exact (or proof)
                // evidence; a declared structural/heuristic provider blocks
                // instead of pretending.
                if required
                    && provider.fidelity() < faktor_semantic::SemanticFidelity::CompilerExact
                {
                    return Err(ExecError::SemanticRequired(format!(
                        "run {run} requires compiler-exact semantic verification but provider {} declares fidelity {:?}",
                        provider.id(),
                        provider.fidelity()
                    )));
                }
                // Completeness is part of the trust claim (audit
                // P1-SEMANTIC): a provider that cannot state its coverage as
                // at least conservative-complete must not satisfy a REQUIRED
                // merge preflight.
                if required
                    && provider.completeness()
                        < faktor_semantic::SemanticCompleteness::ConservativeComplete
                {
                    return Err(ExecError::SemanticRequired(format!(
                        "run {run} requires conservative-complete semantic verification but provider {} declares completeness {:?}",
                        provider.id(),
                        provider.completeness()
                    )));
                }
                provider
            }
            _ if required => {
                return Err(ExecError::SemanticRequired(format!(
                    "run {run} requires a compiler-exact semantic delta for change set {} but no delta-capable provider is registered (the handshake was awaited)",
                    cs.id()
                )));
            }
            _ => {
                tracing::info!(
                    run = %run,
                    "semantic merge preflight is ADVISORY: no delta-capable provider is registered; the fs/CAS merge stands"
                );
                return Ok(());
            }
        };
        let owner = self.plan_row(parent, run)?.owner;
        let workspace = WorkspaceId::try_from(owner.workspace_id).map_err(|e| {
            ExecError::Malformed(format!(
                "run {run} plan row carries owner workspace id {}: {}",
                owner.workspace_id, e.message
            ))
        })?;
        let provider_id = provider.id();
        // The candidate is the STAGED change set with its canonical
        // content-derived identity (`cs.id()`), never a fabricated
        // `{id}-candidate-{count}` string: a provider that materializes the
        // candidate can resolve this identity, and re-staging identical
        // content re-derives the same one.
        let candidate_revision = cs.id();
        let from_snapshot = SemanticSnapshotId::derive(
            workspace,
            &cs.base_id,
            &provider_id,
            provider.version(),
            SEMANTIC_SCHEMA_VERSION,
        );
        let call = match self.manager.try_next_op_id() {
            Ok(op_id) => SemanticCall::new(
                op_id,
                parent,
                workspace,
                self.manager.now_ms(),
                cancel.child(),
            )
            // The view identity above was derived from THIS provider (audit
            // P1-SEMANTIC): pin dispatch so a fail-over provider can never
            // answer a call whose identity describes the selected one.
            .pinned_to(provider_id.clone()),
            Err(e) => {
                if required {
                    return Err(ExecError::SemanticRequired(format!(
                        "op-id allocation failed ({e}); the required semantic merge preflight cannot run"
                    )));
                }
                tracing::warn!(
                    error = %e,
                    "op-id allocation failed; advisory semantic merge preflight skipped"
                );
                return Ok(());
            }
        };
        // Snapshot-ensure (when the provider can): both the base and the
        // staged candidate must be resolvable snapshots before a delta
        // between them can mean anything. The ensure goes through the
        // REGISTRY (audit P1-SEMANTIC): envelope identity/provider/payload
        // validation applies exactly like any other dispatch, never a raw
        // provider call that bypasses it.
        if provider.capabilities().supports(SemanticOp::Snapshot) {
            for revision in [cs.base_id.clone(), candidate_revision.clone()] {
                let ensure = registry.snapshot(SemanticSnapshotRequest {
                    call: call.clone(),
                    workspace,
                    source_revision: revision.clone(),
                });
                let served = match tokio::time::timeout(Self::merge_preflight_timeout(), ensure)
                    .await
                {
                    Ok(Ok(envelope)) if envelope.provider_id.as_str() != GENERIC_FALLBACK_ID => {
                        Ok(())
                    }
                    Ok(Ok(envelope)) => Err(format!(
                        "provider {} degraded the snapshot ensure for {revision:?}",
                        envelope.provider_id
                    )),
                    Ok(Err(err)) => Err(format!(
                        "provider {provider_id} could not ensure snapshot {revision:?}: {err}"
                    )),
                    Err(_) => Err(format!(
                        "provider {provider_id} timed out ensuring snapshot {revision:?}"
                    )),
                };
                match served {
                    Ok(()) => {}
                    Err(detail) if required => {
                        return Err(ExecError::SemanticRequired(detail));
                    }
                    Err(detail) => {
                        tracing::warn!(
                            provider = %provider_id,
                            "advisory semantic merge preflight: {detail}; the fs/CAS merge stands"
                        );
                        return Ok(());
                    }
                }
            }
        }
        let request = SemanticDeltaRequest {
            call,
            workspace,
            from_snapshot,
            from_source_revision: cs.base_id.clone(),
            to_source_revision: candidate_revision,
        };
        // AWAITED with a wall bound: a Pending provider is never treated as
        // absence (the old single-poll helper), and a required preflight
        // blocks on any failure.
        let envelope = match tokio::time::timeout(
            Self::merge_preflight_timeout(),
            registry.delta(request),
        )
        .await
        {
            Ok(Ok(envelope)) => envelope,
            Ok(Err(err)) if required => {
                return Err(ExecError::SemanticRequired(format!(
                    "required semantic merge preflight failed: {err}"
                )));
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    provider = %provider_id,
                    "advisory semantic merge preflight failed: {err}; the fs/CAS merge stands"
                );
                return Ok(());
            }
            Err(_) if required => {
                return Err(ExecError::SemanticRequired(format!(
                    "required semantic merge preflight exceeded its {}ms bound",
                    Self::merge_preflight_timeout().as_millis()
                )));
            }
            Err(_) => {
                tracing::warn!(
                    provider = %provider_id,
                    "advisory semantic merge preflight exceeded its bound; the fs/CAS merge stands"
                );
                return Ok(());
            }
        };
        if envelope.provider_id.as_str() == GENERIC_FALLBACK_ID || envelope.payload.degraded {
            if required {
                return Err(ExecError::SemanticRequired(format!(
                    "provider {provider_id} degraded to fallback/degraded data; required compiler-exact verification is unavailable"
                )));
            }
            tracing::warn!(
                provider = %provider_id,
                "advisory semantic merge preflight has no trustworthy provider data; the fs/CAS merge stands"
            );
            return Ok(());
        }
        let conflicts = semantic_delta_conflicts(cs, &envelope.payload, approved);
        if conflicts.is_empty() {
            return Ok(());
        }
        let mut summary = conflicts
            .iter()
            .take(3)
            .map(|(path, detail)| format!("{}: {detail}", path.display()))
            .collect::<Vec<_>>()
            .join("; ");
        if conflicts.len() > 3 {
            summary.push_str(&format!("; (+{} more)", conflicts.len() - 3));
        }
        Err(ExecError::SemanticConflict(format!(
            "provider {provider_id} reported a delta that contradicts the staged change set ({} paths): {summary}",
            conflicts.len()
        )))
    }
}

/// Typed semantic-preflight conflicts (audit 79): a provider base→candidate
/// delta entry that CONTRADICTS a staged change entry is a semantic
/// conflict. Pure and fully typed — only validated entity paths and hex
/// digests can reach the bounded reason; provider prose never does. Entity
/// paths outside the approved set are ignored (not part of THIS merge) and
/// non-contradictory deltas yield nothing.
///
/// The check is deliberately an INCONSISTENCY check, not a merge decision:
/// it can only ever ADD a refusal (a delta whose digests contradict what is
/// staged cannot be trusted), never clear or reshape a file-level CAS
/// conflict.
///
/// HONEST HOLD-OUT: provider deltas carry whole-CONTENT hashes only, so the
/// digest comparison applies to REGULAR entries (whose payload digest IS the
/// content hash). A symlink entry's payload digest covers the literal target
/// bytes — a different domain — and mode-only/symlink differences are not
/// representable in the delta model, so those comparisons are skipped
/// (the triple comparison in stage/compose/apply remains the authority).
pub fn semantic_delta_conflicts(
    cs: &ChangeSet,
    delta: &SemanticDelta,
    approved: &[PathBuf],
) -> Vec<(PathBuf, String)> {
    let approved_set: HashSet<&Path> = approved.iter().map(|p| p.as_path()).collect();
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for change in &delta.changes {
        let entity_path = change.entity.path.as_str();
        let path = PathBuf::from(entity_path);
        if !approved_set.contains(path.as_path()) {
            continue;
        }
        let Some(entry) = cs.files.iter().find(|e| e.path == path) else {
            continue;
        };
        let child = entry.child_state();
        let base = entry.base_state();
        let reason: Option<String> = if child.is_none() {
            if change.kind == SemanticDeltaKind::Removed {
                None
            } else {
                Some(format!(
                    "provider delta keeps content at {entity_path} but the staged candidate deletes it"
                ))
            }
        } else if change.kind == SemanticDeltaKind::Removed {
            Some(format!(
                "provider delta removes {entity_path} but the staged candidate keeps content"
            ))
        } else if matches!(
            (change.new_hash, child.as_ref().and_then(|s| s.payload_digest())),
            (Some(provider), Some(staged)) if provider != staged
        ) {
            let provider = change.new_hash.map(|h| h.to_hex()).unwrap_or_default();
            let staged = child
                .as_ref()
                .and_then(|s| s.payload_digest_hex())
                .unwrap_or_default();
            Some(format!(
                "provider candidate hash {provider} disagrees with the staged candidate hash {staged} at {entity_path}"
            ))
        } else if matches!(
            (change.old_hash, base.as_ref().and_then(|s| s.payload_digest())),
            (Some(provider), Some(staged)) if provider != staged
        ) {
            let provider = change.old_hash.map(|h| h.to_hex()).unwrap_or_default();
            let staged = base
                .as_ref()
                .and_then(|s| s.payload_digest_hex())
                .unwrap_or_default();
            Some(format!(
                "provider composed over base hash {provider} but the staged base hash is {staged} at {entity_path}"
            ))
        } else {
            None
        };
        if let Some(reason) = reason {
            out.push((path, truncate(&reason, 300)));
            if out.len() >= SEMANTIC_PREFLIGHT_MAX_CONFLICTS {
                break;
            }
        }
    }
    out
}
