//! Durable human-operator approval authority.
//!
//! Autonomous agents may *propose* consequential Engineering/Supply actions,
//! but they can never execute or approve them. This crate is the one
//! authority that:
//!
//! 1. records a typed [`ApprovalRequest`] (kind, subject, evidence, proposed
//!    change, risk, requester, expiry) in an append-only, hash-chained
//!    durable ledger;
//! 2. records an exactly-once [`ApprovalDecision`] only from a registered
//!    [`PrincipalKind::HumanOperator`] who signed the decision with their
//!    Ed25519 operator key and who is not the requester;
//! 3. issues an unforgeable [`ApprovalToken`] to a gate only when an approved
//!    decision is current and bound to the exact revision and digest the gate
//!    is about to change.
//!
//! Binding is digest-based: the gate recomputes the proposed-change digest
//! from live state, so any later mutation makes an old approval
//! [`ApprovalError::Stale`] instead of silently authorizing the new state.
//!
//! # Durable layout
//!
//! ```text
//! <data-dir>/approval/ledger.jsonl      append-only request/decision chain
//! <data-dir>/approval/ledger.head       committed tail anchor (anti-truncation)
//! <data-dir>/approval/operators.json    human-operator public-key registry
//! <data-dir>/approval/operators/*.key   owner-only (0600) signing seeds
//! ```
//!
//! Every ledger entry commits to the previous digest, its sequence number
//! and the canonical JSON body; re-opening verifies the whole chain and the
//! committed head, and refuses typed on corruption or tail truncation. The decision signature commits to the
//! request digest, request id, outcome, human principal, rationale and
//! decision time, so a decision cannot be replayed for another request or
//! altered after the fact.

mod authority;
mod model;
mod operator;
mod store;

pub use authority::{unix_now_ms, ApprovalAuthority, ApprovalStatusView, ApprovalView};
pub use model::{
    content_digest, ApprovalCheck, ApprovalDecision, ApprovalDraft, ApprovalError, ApprovalKind,
    ApprovalRequest, ApprovalToken, DecisionOutcome, DecisionSignature, Principal, PrincipalKind,
    RiskLevel, MAX_APPROVAL_EVIDENCE, MAX_APPROVAL_EVIDENCE_BYTES, MAX_APPROVAL_TEXT_BYTES,
    MAX_OPEN_APPROVALS, MAX_PRINCIPAL_ID_BYTES, MAX_REQUEST_LIFETIME_MS,
};
pub use operator::{
    load_operator_key, operator_key_path, operator_key_paths, register_operator_key,
    save_operator_key, OperatorKey, OPERATOR_DIRECTORY, OPERATOR_REGISTRY_FILE,
};
pub use store::{ledger_head_path, ledger_path, ApprovalLedger, LedgerBody, LedgerEntry};
