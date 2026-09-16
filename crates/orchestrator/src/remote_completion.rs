//! The additive remote-run COMPLETION gate of the orchestrator.
//!
//! A run placed remotely ([`crate::placement::PlacementDecision::Remote`])
//! never executes locally; the worker plane lands one digest-bound result for
//! the immutable generation it minted. This module is how the daemon hands
//! that landed result back to the ONE settlement pipeline
//! ([`crate::task_executor::TaskExecutor::settle_run`]).
//!
//! Classification (documented, fail closed):
//!
//! - **self-verifying** — a SUCCEEDED run that reports `self_verified` and
//!   NO produced tree (`produced_digest: None`, i.e. the run was read-only).
//!   The origin may settle the parent run from the worker's claim through
//!   the same settlement pass local runs use.
//! - **origin-verified** — EVERY other result (a produced/mutated tree, a
//!   missing self-verification claim, or a failed outcome). The claim is
//!   never proof: the run is NOT completed from it, and the daemon must run
//!   the origin's own verification (the same verification pipeline local
//!   runs use, over the produced digest/tree) before any settlement. The
//!   typed [`RemoteCompletionOutcome::OriginVerificationRequired`] names the
//!   exact reason; nothing is settled.
//!
//! Lease loss is owned by the worker plane (bounded requeue policy): a
//! superseded/lost lease never produces a landed result, so it can never
//! reach this gate. A malformed completion (bad digest shape, unknown run
//! kind, degenerate generation) is a typed refusal before any durable read.

use faktor_core::id::SessionId;
use serde::{Deserialize, Serialize};

use crate::runtime::task_executor::SettlementOutcome;

/// The digest length (BLAKE3 hex) every placement result binds.
pub const REMOTE_COMPLETION_DIGEST_BYTES: usize = 64;

/// The worker's verification claim as it rides a landed result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteVerificationClaim {
    /// `true` only when the worker's own pipeline deterministically verified
    /// the produced work.
    pub self_verified: bool,
    /// The digest of the produced tree when the run mutated one; `None`
    /// means the run was read-only.
    #[serde(default)]
    pub produced_digest: Option<String>,
}

/// The outcome one landed result reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRunOutcome {
    Succeeded,
    Failed,
}

/// One landed remote result as the daemon delivers it to the completion gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRunCompletion {
    /// The parent session the run was placed for.
    pub parent: SessionId,
    /// The run identity the placement receipt named (the worker job id).
    pub run_id: String,
    pub job_id: String,
    /// The immutable generation the result was accepted under (>= 1).
    pub generation: u64,
    /// The run shape the placement minted: `in_session` or `orchestrated`.
    pub kind: String,
    /// The landed digest (the job's immutable payload digest, 64 hex).
    pub digest: String,
    pub outcome: RemoteRunOutcome,
    #[serde(default)]
    pub claim: RemoteVerificationClaim,
}

/// Which settlement class one landed result belongs to (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCompletionClass {
    /// Read-only run whose worker-side deterministic verification passed:
    /// the origin may settle the parent run from the claim.
    SelfVerifiedReadOnly,
    /// Any produced tree, missing claim or failed outcome: origin
    /// verification is mandatory before settlement.
    OriginVerificationRequired,
}

/// The typed result of one completion consultation. A settlement is a
/// delegation to the ONE post-run settlement pass; everything else settles
/// NOTHING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCompletionOutcome {
    /// The self-verified read-only result settled through the same pipeline
    /// local runs use.
    Settled {
        class: RemoteCompletionClass,
        settlement: SettlementOutcome,
    },
    /// The result can never complete the parent run by itself: the origin
    /// must verify (fail closed). `reason` names the exact cause.
    OriginVerificationRequired {
        class: RemoteCompletionClass,
        job_id: String,
        run_id: String,
        reason: String,
    },
}

impl RemoteRunCompletion {
    /// The fail-closed classification of one landed result (pure).
    pub fn classify(&self) -> Result<RemoteCompletionClass, String> {
        if self.outcome == RemoteRunOutcome::Failed {
            return Ok(RemoteCompletionClass::OriginVerificationRequired);
        }
        if !self.claim.self_verified {
            return Ok(RemoteCompletionClass::OriginVerificationRequired);
        }
        if self.claim.produced_digest.is_some() {
            return Ok(RemoteCompletionClass::OriginVerificationRequired);
        }
        Ok(RemoteCompletionClass::SelfVerifiedReadOnly)
    }

    /// The human reason behind an origin-verification requirement.
    pub fn origin_reason(&self) -> String {
        if self.outcome == RemoteRunOutcome::Failed {
            return "the remote run reported a failed outcome; a failure is never completed from the result".into();
        }
        if !self.claim.self_verified {
            return "the result carries no worker-side self-verification claim".into();
        }
        if self.claim.produced_digest.is_some() {
            return "the run produced a mutated tree; the origin must verify the produced work"
                .into();
        }
        "the result is not classifiable as a self-verified read-only run".into()
    }

    /// Strict shape validation: the gate refuses malformed completions
    /// before any durable read.
    pub fn validate(&self) -> Result<(), String> {
        if self.job_id.trim().is_empty() || self.job_id.len() > 256 {
            return Err("job_id must be 1..=256 bytes".into());
        }
        if self.run_id.trim().is_empty() || self.run_id.len() > 256 {
            return Err("run_id must be 1..=256 bytes".into());
        }
        if self.generation == 0 {
            return Err("generation must be >= 1".into());
        }
        if self.kind != "in_session" && self.kind != "orchestrated" {
            return Err(format!(
                "unknown run kind {:?} (expected in_session|orchestrated)",
                self.kind
            ));
        }
        if self.digest.len() != REMOTE_COMPLETION_DIGEST_BYTES
            || !self.digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "digest must be {} hex characters",
                REMOTE_COMPLETION_DIGEST_BYTES
            ));
        }
        if let Some(produced) = &self.claim.produced_digest {
            if produced.len() != REMOTE_COMPLETION_DIGEST_BYTES
                || !produced.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(format!(
                    "produced_digest must be {} hex characters",
                    REMOTE_COMPLETION_DIGEST_BYTES
                ));
            }
        }
        Ok(())
    }
}
