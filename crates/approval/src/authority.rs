//! The durable approval authority: propose, decide, authorize, audit.
//!
//! The authority holds no cached truth. Every call replays the durable
//! ledger (verifying its chain) and reloads the operator registry, so a
//! decision made by a human through the CLI is visible to an already-running
//! daemon on its next tool call, and a stale in-memory view can never
//! authorize a later mutation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{
    ApprovalCheck, ApprovalDecision, ApprovalDraft, ApprovalError, ApprovalRequest, ApprovalToken,
    DecisionOutcome, Principal,
};
use crate::operator::{key_registered_for, load_registry, OperatorKey};
use crate::store::{ApprovalLedger, LedgerBody, LedgerEntry, MonotonicClock};

/// Current wall-clock time in epoch milliseconds (0 only if the clock is
/// before the epoch, which every gate then refuses).
pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// One request plus its decision, when decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    /// The request.
    pub request: ApprovalRequest,
    /// The decision, if any.
    pub decision: Option<ApprovalDecision>,
}

/// A compact status projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalStatusView {
    /// `open`, `approved`, `denied` or `expired`.
    pub status: String,
    /// The request id.
    pub request_id: String,
    /// Kind.
    pub kind: String,
    /// Subject.
    pub subject: String,
    /// Bound revision.
    pub bound_revision: String,
    /// Bound digest.
    pub bound_digest: String,
    /// Expiry.
    pub expires_at_ms: u64,
    /// Deciding human, when decided.
    pub decided_by: Option<String>,
    /// Rationale, when decided.
    pub rationale: Option<String>,
}

/// The durable approval authority over one data dir.
#[derive(Debug, Clone)]
pub struct ApprovalAuthority {
    root: PathBuf,
    clock: MonotonicClock,
}

impl ApprovalAuthority {
    /// Open the approval authority over `data_dir`. Calls that observe time
    /// (`propose`, `decide`, `authorize`, `status`) persist the wall-clock
    /// high-water mark; ledger inspection (`list`, `audit`, `view`) creates
    /// nothing, and the ledger/registry are created lazily.
    pub fn open(data_dir: &Path) -> Result<Self, ApprovalError> {
        Ok(Self {
            root: data_dir.to_path_buf(),
            clock: MonotonicClock::new(data_dir),
        })
    }

    /// The data dir this authority reads and writes.
    pub fn data_dir(&self) -> &Path {
        &self.root
    }

    /// Propose a consequential action. Idempotent: the same draft at the same
    /// millisecond produces the same request id and is returned unchanged.
    pub fn propose(
        &self,
        draft: ApprovalDraft,
        now_ms: u64,
    ) -> Result<ApprovalRequest, ApprovalError> {
        let now_ms = self.clock.effective_now(now_ms)?;
        let request = ApprovalRequest::propose(draft, now_ms)?;
        let mut transaction = ApprovalLedger::begin(&self.root)?;
        if let Some(existing) = transaction.ledger().requests.get(&request.id) {
            if existing == &request {
                return Ok(existing.clone());
            }
            return Err(ApprovalError::Conflict(format!(
                "request id {} already records different content",
                request.id
            )));
        }
        transaction.append(
            LedgerBody::Requested {
                request: request.clone(),
            },
            now_ms,
        )?;
        Ok(request)
    }

    /// A human operator decides exactly once. Agents are refused by type, a
    /// requester cannot decide its own request, and the signing key must be
    /// the one registered for that human principal.
    pub fn decide(
        &self,
        request_id: &str,
        principal: Principal,
        key: &OperatorKey,
        outcome: DecisionOutcome,
        rationale: String,
        now_ms: u64,
    ) -> Result<ApprovalDecision, ApprovalError> {
        principal.validate()?;
        let now_ms = self.clock.effective_now(now_ms)?;
        if principal.kind != crate::model::PrincipalKind::HumanOperator {
            return Err(ApprovalError::AgentApproval {
                principal: principal.id,
            });
        }
        let mut transaction = ApprovalLedger::begin(&self.root)?;
        let request = transaction
            .ledger()
            .requests
            .get(request_id)
            .cloned()
            .ok_or_else(|| ApprovalError::NotFound(format!("approval request {request_id}")))?;
        if request.requester.id == principal.id {
            return Err(ApprovalError::SelfApproval {
                principal: principal.id,
            });
        }
        key_registered_for(&self.root, &principal.id, key)?;
        if let Some(existing) = transaction.ledger().decisions.get(request_id) {
            if existing.outcome == outcome
                && existing.principal == principal
                && existing.rationale == rationale
            {
                return Ok(existing.clone());
            }
            return Err(ApprovalError::Conflict(format!(
                "approval request {request_id} is already decided"
            )));
        }
        if now_ms > request.expires_at_ms {
            return Err(ApprovalError::Expired {
                request_id: request.id.clone(),
                expired_at_ms: request.expires_at_ms,
            });
        }
        let decision = ApprovalDecision::issue(
            &request,
            outcome,
            principal,
            rationale,
            now_ms,
            &key.handle(),
        )?;
        transaction.append(
            LedgerBody::Decided {
                decision: decision.clone(),
            },
            now_ms,
        )?;
        Ok(decision)
    }

    /// Authorize the exact live action, or fail typed. This is the only path
    /// that mints an [`ApprovalToken`].
    pub fn authorize(
        &self,
        check: &ApprovalCheck,
        now_ms: u64,
    ) -> Result<ApprovalToken, ApprovalError> {
        let now_ms = self.clock.effective_now(now_ms)?;
        let ledger = ApprovalLedger::load(&self.root)?;
        let mut exact: Vec<&ApprovalRequest> = ledger
            .requests
            .values()
            .filter(|request| {
                request.kind == check.kind
                    && request.subject == check.subject
                    && request.bound_revision == check.bound_revision
                    && request.bound_digest == check.bound_digest
            })
            .collect();
        if exact.is_empty() {
            let latest = ledger
                .requests
                .values()
                .filter(|request| request.kind == check.kind && request.subject == check.subject)
                .max_by_key(|request| request.created_at_ms);
            if let Some(request) = latest {
                return Err(ApprovalError::Stale {
                    request_id: request.id.clone(),
                    expected_digest: check.bound_digest.clone(),
                    actual_digest: request.bound_digest.clone(),
                    expected_revision: check.bound_revision.clone(),
                    actual_revision: request.bound_revision.clone(),
                });
            }
            return Err(ApprovalError::ApprovalRequired {
                kind: check.kind,
                subject: check.subject.clone(),
                bound_revision: check.bound_revision.clone(),
                bound_digest: check.bound_digest.clone(),
            });
        }
        exact.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then(right.id.cmp(&left.id))
        });
        self.evaluate(exact[0], &ledger, now_ms)
    }

    /// Authorize one explicit request id, refusing when it does not bind to
    /// the exact live action.
    pub fn authorize_request(
        &self,
        request_id: &str,
        check: &ApprovalCheck,
        now_ms: u64,
    ) -> Result<ApprovalToken, ApprovalError> {
        let now_ms = self.clock.effective_now(now_ms)?;
        let ledger = ApprovalLedger::load(&self.root)?;
        let request = ledger
            .requests
            .get(request_id)
            .ok_or_else(|| ApprovalError::NotFound(format!("approval request {request_id}")))?;
        if request.kind != check.kind
            || request.subject != check.subject
            || request.bound_revision != check.bound_revision
            || request.bound_digest != check.bound_digest
        {
            return Err(ApprovalError::Stale {
                request_id: request.id.clone(),
                expected_digest: check.bound_digest.clone(),
                actual_digest: request.bound_digest.clone(),
                expected_revision: check.bound_revision.clone(),
                actual_revision: request.bound_revision.clone(),
            });
        }
        self.evaluate(request, &ledger, now_ms)
    }

    fn evaluate(
        &self,
        request: &ApprovalRequest,
        ledger: &ApprovalLedger,
        now_ms: u64,
    ) -> Result<ApprovalToken, ApprovalError> {
        let trusted = crate::operator::trusted_key_ids(&self.root)?;
        let operators = load_registry(&self.root)?;
        Self::evaluate_with_trust(request, ledger, now_ms, &trusted, &operators)
    }

    fn evaluate_with_trust(
        request: &ApprovalRequest,
        ledger: &ApprovalLedger,
        now_ms: u64,
        trusted: &BTreeSet<String>,
        operators: &[crate::operator::RegisteredOperator],
    ) -> Result<ApprovalToken, ApprovalError> {
        if now_ms > request.expires_at_ms {
            return Err(ApprovalError::Expired {
                request_id: request.id.clone(),
                expired_at_ms: request.expires_at_ms,
            });
        }
        let Some(decision) = ledger.decisions.get(&request.id) else {
            return Err(ApprovalError::Pending {
                request_id: request.id.clone(),
                kind: request.kind,
                subject: request.subject.clone(),
            });
        };
        verify_decision_with(request, decision, trusted, operators)?;
        match &decision.outcome {
            DecisionOutcome::Approved => ApprovalToken::from_verified(request, decision, now_ms),
            DecisionOutcome::Denied { reason } => Err(ApprovalError::Denied {
                request_id: request.id.clone(),
                kind: request.kind,
                subject: request.subject.clone(),
                principal: decision.principal.id.clone(),
                reason: reason.clone(),
            }),
        }
    }

    /// Verify every authority property of a recorded decision: human type,
    /// requester separation, cryptographic signature, registration of that
    /// exact key for that exact principal, and decision timing.
    fn verify_decision(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
    ) -> Result<(), ApprovalError> {
        let trusted = crate::operator::trusted_key_ids(&self.root)?;
        let operators = load_registry(&self.root)?;
        verify_decision_with(request, decision, &trusted, &operators)
    }

    /// The newest request of one kind and subject, decided or not. Used to
    /// distinguish "no plan baseline was ever approved" from "the baseline
    /// lapsed", so an expired approval cannot be treated as absence.
    pub fn latest_request(
        &self,
        kind: crate::model::ApprovalKind,
        subject: &str,
    ) -> Result<Option<ApprovalRequest>, ApprovalError> {
        let ledger = ApprovalLedger::load(&self.root)?;
        Ok(ledger
            .requests
            .values()
            .filter(|request| request.kind == kind && request.subject == subject)
            .max_by(|left, right| {
                left.created_at_ms
                    .cmp(&right.created_at_ms)
                    .then(left.id.cmp(&right.id))
            })
            .cloned())
    }

    /// The newest currently-approved authorization of one kind and subject,
    /// if any. Used by closed-loop plan baselines: the latest human-approved
    /// plan is the approved baseline an update must be compared against.
    pub fn latest_authorized(
        &self,
        kind: crate::model::ApprovalKind,
        subject: &str,
        now_ms: u64,
    ) -> Result<Option<ApprovalToken>, ApprovalError> {
        let now_ms = self.clock.effective_now(now_ms)?;
        let ledger = ApprovalLedger::load(&self.root)?;
        let mut requests: Vec<&ApprovalRequest> = ledger
            .requests
            .values()
            .filter(|request| request.kind == kind && request.subject == subject)
            .collect();
        requests.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then(right.id.cmp(&left.id))
        });
        let trusted = crate::operator::trusted_key_ids(&self.root)?;
        let operators = load_registry(&self.root)?;
        for request in requests {
            match Self::evaluate_with_trust(request, &ledger, now_ms, &trusted, &operators) {
                Ok(token) => return Ok(Some(token)),
                Err(
                    ApprovalError::Pending { .. }
                    | ApprovalError::Denied { .. }
                    | ApprovalError::Expired { .. },
                ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// One request plus its decision.
    pub fn view(&self, request_id: &str) -> Result<ApprovalView, ApprovalError> {
        let ledger = ApprovalLedger::load(&self.root)?;
        let request = ledger
            .requests
            .get(request_id)
            .ok_or_else(|| ApprovalError::NotFound(format!("approval request {request_id}")))?;
        let decision = ledger.decisions.get(request_id).cloned();
        if let Some(decision) = &decision {
            self.verify_decision(request, decision)?;
        }
        Ok(ApprovalView {
            request: request.clone(),
            decision,
        })
    }

    /// Compact status for CLI/tool surfaces.
    pub fn status(&self, request_id: &str) -> Result<ApprovalStatusView, ApprovalError> {
        let view = self.view(request_id)?;
        let now_ms = self.clock.effective_now(unix_now_ms())?;
        let status = match &view.decision {
            None => {
                if now_ms > view.request.expires_at_ms {
                    "expired"
                } else {
                    "open"
                }
            }
            Some(decision) => match &decision.outcome {
                DecisionOutcome::Approved => {
                    if now_ms > view.request.expires_at_ms {
                        "expired"
                    } else {
                        "approved"
                    }
                }
                DecisionOutcome::Denied { .. } => "denied",
            },
        };
        Ok(ApprovalStatusView {
            status: status.to_string(),
            request_id: view.request.id.clone(),
            kind: view.request.kind.as_str().to_string(),
            subject: view.request.subject.clone(),
            bound_revision: view.request.bound_revision.clone(),
            bound_digest: view.request.bound_digest.clone(),
            expires_at_ms: view.request.expires_at_ms,
            decided_by: view
                .decision
                .as_ref()
                .map(|decision| decision.principal.id.clone()),
            rationale: view
                .decision
                .as_ref()
                .map(|decision| decision.rationale.clone()),
        })
    }

    /// Every request/decision in ledger order.
    pub fn list(&self) -> Result<Vec<ApprovalView>, ApprovalError> {
        let ledger = ApprovalLedger::load(&self.root)?;
        let trusted = crate::operator::trusted_key_ids(&self.root)?;
        let operators = load_registry(&self.root)?;
        let mut views: Vec<ApprovalView> = ledger
            .requests
            .values()
            .map(|request| {
                let decision = ledger.decisions.get(&request.id).cloned();
                if let Some(decision) = &decision {
                    verify_decision_with(request, decision, &trusted, &operators)?;
                }
                Ok(ApprovalView {
                    request: request.clone(),
                    decision,
                })
            })
            .collect::<Result<Vec<_>, ApprovalError>>()?;
        views.sort_by(|left, right| {
            left.request
                .created_at_ms
                .cmp(&right.request.created_at_ms)
                .then(left.request.id.cmp(&right.request.id))
        });
        Ok(views)
    }

    /// The hash-chained audit trail, optionally narrowed to one request.
    pub fn audit(&self, request_id: Option<&str>) -> Result<Vec<LedgerEntry>, ApprovalError> {
        let ledger = ApprovalLedger::load(&self.root)?;
        Ok(ledger
            .entries
            .into_iter()
            .filter(|entry| match (request_id, &entry.body) {
                (None, _) => true,
                (Some(id), LedgerBody::Requested { request }) => request.id == id,
                (Some(id), LedgerBody::Decided { decision }) => decision.request_id == id,
            })
            .collect())
    }

    /// The registered human operator key fingerprints.
    pub fn operator_key_ids(&self) -> Result<BTreeSet<String>, ApprovalError> {
        crate::operator::trusted_key_ids(&self.root)
    }
}

fn verify_decision_with(
    request: &ApprovalRequest,
    decision: &ApprovalDecision,
    trusted: &BTreeSet<String>,
    operators: &[crate::operator::RegisteredOperator],
) -> Result<(), ApprovalError> {
    if decision.principal.kind != crate::model::PrincipalKind::HumanOperator {
        return Err(ApprovalError::AgentApproval {
            principal: decision.principal.id.clone(),
        });
    }
    if decision.principal.id == request.requester.id {
        return Err(ApprovalError::SelfApproval {
            principal: decision.principal.id.clone(),
        });
    }
    if decision.decided_at_ms < request.created_at_ms
        || decision.decided_at_ms > request.expires_at_ms
    {
        return Err(ApprovalError::Invalid(
            "decision is timed outside its request lifetime".to_string(),
        ));
    }
    decision.verify(Some(trusted))?;
    if !operators.iter().any(|operator| {
        operator.key_id == decision.signature.key_id
            && operator.principal_id == decision.principal.id
            && operator.public_key == decision.signature.public_key
    }) {
        return Err(ApprovalError::UntrustedOperator {
            key_id: decision.signature.key_id.clone(),
        });
    }
    Ok(())
}
