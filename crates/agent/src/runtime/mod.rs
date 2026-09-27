#![allow(unused_imports)]

//! The agent runtime: the durable turn loop that drives the session with
//! commands, streams providers, schedules tools, and keeps context bounded.
//!
//! # Layered bounded lifetimes (audit 26)
//!
//! A TASK's lifetime is bounded by its durable budget (max tokens / max
//! turns) — **never by a single future**. No runtime future or retry loop is
//! ever scheduled for more than one `turn_budget_ms` slice (per logical
//! turn, default 30 minutes, configured on the `SessionManager`); progress
//! is persisted between slices via the ledger, the typed `task` rows and the
//! memory facts, and the turn loop re-enters on the next prompt/queue
//! admission. The operation budget (`tool_deadline_ms`) bounds one tool call
//! and the verification policy's per-category budgets bound verification
//! (P0-9/10: typed checks, no universal wall cap). There is deliberately no
//! 24h deadline anywhere: a task that spans days does so across many turns,
//! restarts and compaction cycles — each one a bounded future.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::wire_plan::plan_wire_turn_with_prior;
use crate::EfficiencyFlags;
use faktor_context::artifact::ArtifactWriter;
use faktor_context::assembler::{Evidence, RecentTurn};
use faktor_context::budget::ContextBudget;
use faktor_context::compactor::{CompactionPlan, CompactionRequest, Compactor, Summarizer};
use faktor_context::compiler::{
    CompilerError, CompilerInput, ContextCompiler, CoordinationNoticeMemo, CriterionFact,
    DurableEvidenceAuthority, EvidenceId, EvidenceKind, ProvenanceSource, TaskFacts,
    VerificationState, VolatileClaims, WorkItem,
};
use faktor_context::ledger::TaskLedger;
use faktor_context::wire_plan::WirePlan;
use faktor_context::TokenCache;
use faktor_core::blocker::ExecutionPhase;
use faktor_core::cancellation::CancellationToken;
use faktor_core::capability::{Capability, PermissionDecision};
use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;
use faktor_core::id::{OpId, SessionId, TaskId, WorkspaceId};
use faktor_core::model::PricingSnapshot;
use faktor_core::op::{EffectStatus, ModelCallAttempt, OpMeta, RecoveryStrategy};
use faktor_core::state::{
    command_binding_digest, AgentState, CandidateProofRef, CheckExecution, CriterionBinding,
    CriterionOrigin, CriterionRequirement, CriterionVerification, EnvironmentFingerprint,
    FileStateEvidence, FingerprintFileHash, OutcomeReason, ReasonCode, TaskState, TaskTransition,
    ToolVersion, VerificationStatus,
};
use faktor_core::time::Clock;
use faktor_core::WorkspaceIdentity;
use faktor_fs::{legacy_relative_path_within, workspace_relative_path_rejection};
use faktor_protocol::native::ToolResultBody;
use faktor_provider::{
    CanonicalUsage, CapabilityValidator, ContentKind, ContentPart, GenericAgentRequest,
    ProviderChunk, ProviderError, ProviderErrorKind, ProviderRegistry, ReportedCost,
    RequestMessage, RequestMeta, Role,
};
use faktor_scheduler::{OwnershipSet, ResourceRequest, ScheduledOp, Scheduler};
use faktor_semantic::{
    AffectedRequest, RiskLevel, RiskPolicy, SemanticCall, SemanticCapabilities, SemanticEntityId,
    SemanticEntityRef, SemanticOp, SemanticRisk, SemanticSelection, SemanticSnapshotId,
    WorkspacePath, GENERIC_FALLBACK_ID, MAX_ENTITY_ID_BYTES, SEMANTIC_SCHEMA_VERSION,
};
use faktor_session::ops::PermissionRequest as SessionPermission;
use faktor_session::task::{
    decode_criteria, encode_criteria, merge_derived_criteria, Criterion, ProofBasis,
    ProofBasisCheck, ProofBasisCriterion,
};
use faktor_session::{
    BudgetError as SessionBudgetError, CompletionContractGate, RecoveredOp, RecoveryAction,
    RecoveryReport, SessionManager, Task, TaskError, TaskPatch,
};
use faktor_store::ToolRunRow;
use faktor_verify::criteria::{
    ChangeSetEntry, ChangeSetStatus, CheckOutcomeRow, CheckOutcomeStatus, CriterionEvaluation,
    CriterionEvaluationContext, CriterionEvaluationRow, EvidenceRecord, EvidenceResolver,
    FallbackEvaluator, IndependentReviewer, ReadOnlyRepo, ReviewRequest, ReviewVerdict,
    StructuredReviewOutput,
};
use faktor_verify::exec::{BudgetDecision, CheckRunStatus};

use crate::activation::ToolActivationSet;
use crate::loop_detect::{Fingerprint, LoopDetector};
use crate::stall::{ProgressEvidence, StallTracker};
use crate::tool::{
    FilePostcondition, RecoveryHint, ReplayDescriptor, Tool, ToolBundle, ToolOutcome, ToolRegistry,
    ToolRunCtx,
};
use crate::tool_json::ToolCallMode;
use crate::{
    RiskBucket, RouteDecision, RouterPhase, SettledCallOutcome, TaskClass, VerifiedCallAttribution,
};

mod request;
pub use request::*;
mod media;
pub(crate) use media::*;
mod retrieval;
pub use retrieval::*;
mod routing;
pub use routing::*;
mod provider_loop;
pub use provider_loop::*;
mod tool_loop;
pub use tool_loop::*;
mod retry;
pub(crate) use retry::*;
mod settlement;
pub(crate) use settlement::*;
mod compaction;
pub(crate) use compaction::*;
mod verification_attribution;
pub use verification_attribution::*;
mod turn;
pub use turn::*;

#[cfg(test)]
#[path = "fixtures_tests.rs"]
mod fixtures_tests;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "request_tests.rs"]
mod request_tests;

#[cfg(test)]
#[path = "media_tests.rs"]
mod media_tests;

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod retrieval_tests;

#[cfg(test)]
#[path = "routing_tests.rs"]
mod routing_tests;

#[cfg(test)]
#[path = "provider_loop_tests.rs"]
mod provider_loop_tests;

#[cfg(test)]
#[path = "tool_loop_tests.rs"]
mod tool_loop_tests;

#[cfg(test)]
#[path = "retry_tests.rs"]
mod retry_tests;

#[cfg(test)]
#[path = "settlement_tests.rs"]
mod settlement_tests;

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod compaction_tests;

#[cfg(test)]
#[path = "verification_attribution_tests.rs"]
mod verification_attribution_tests;

#[cfg(test)]
#[path = "activation_scan_diagnostics_tests.rs"]
mod activation_scan_diagnostics_tests;

#[cfg(test)]
#[path = "durable_faults_tests.rs"]
mod durable_faults_tests;

#[cfg(test)]
#[path = "scheduler_faults_tests.rs"]
mod scheduler_faults_tests;
