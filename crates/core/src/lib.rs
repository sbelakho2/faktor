//! faktor-core — pure core types for the Faktor runtime.
//!
//! This crate has **no workspace dependencies** and no I/O. Every other crate
//! depends on these types; dependencies point inward. It provides:
//!
//! - IDs (`SessionId`, `WorkspaceId`, `WorktreeId`, `TaskId`, `OpId`, `EventSeq`)
//! - typed errors with retryability classification
//! - the explicit session state machine (no implicit async state)
//! - the append-only event journal contract
//! - operation metadata (deadline, retry policy, cancellation, recovery)
//! - cancellation tokens (std-only, no tokio)
//! - injectable clocks and deadlines
//! - resource classes and budgets
//! - sandbox capabilities and permission decisions
//! - model capabilities (provider behavior lives here, not in the agent)

pub mod attachment;
pub mod authority;
pub mod blocker;
pub mod cancellation;
pub mod capability;
pub mod command;
pub mod completion;
pub mod error;
pub mod event;
pub mod hash;
pub mod id;
pub mod model;
pub mod op;
pub mod path;
pub mod resource;
pub mod retry;
pub mod state;
pub mod time;

pub use attachment::{
    validate_filename, validate_mime, AttachmentId, MAX_ATTACHMENTS_PER_TASK, MAX_ATTACHMENT_BYTES,
    MAX_ATTACHMENT_FILENAME_BYTES, MAX_ATTACHMENT_MIME_BYTES,
};
pub use authority::{
    authority_digest, authority_digest_hex, authority_digest_labeled, classify_authority_digest,
    refuse_legacy_authority_digest, AuthorityDigestKind, CanonicalFieldWriter, CanonicalFields,
    Fields, LegacyAuthorityDigest, DOMAIN_ACCOUNTING_BALANCE, DOMAIN_BASE_MAP,
    DOMAIN_CANDIDATE_MANIFEST, DOMAIN_CHANGED_FILES, DOMAIN_CHANGE_SET, DOMAIN_CHECK_BASIS,
    DOMAIN_CHECK_EXECUTION, DOMAIN_COMMAND_BINDING, DOMAIN_CRITERION_BINDING,
    DOMAIN_INTEGRATION_SOURCES, DOMAIN_RUN_BASE_MANIFEST, DOMAIN_SEMANTIC_FACT,
    DOMAIN_TASK_CONTRACT, MAX_AUTHORITY_FIELD_BYTES,
};
pub use blocker::{
    child_lifecycle_tag_is_known, validate_child_runtime_state, BlockerKind, ChildBlocker,
    ExecutionPhase, CHILD_LIFECYCLE_TAGS, MAX_CHILD_BLOCKER_DEPENDENCY_CHARS,
    MAX_CHILD_BLOCKER_REASON_CHARS, MAX_CHILD_BLOCKER_RESOLUTION_CHARS,
};
pub use cancellation::CancellationToken;
pub use capability::{
    Capability, CapabilityKind, CapabilitySet, NetworkPolicy, PermissionDecision,
};
pub use command::{CommandSpec, EnvSpec, NetworkIsolationRequirement, ResolvedCommand, ShellKind};
pub use completion::{CompletionContract, CompletionStep, CompletionStepOutcome};
pub use error::{Error, ErrorKind, Result};
pub use event::{Event, EventKind};
pub use hash::FileHash;
pub use id::{EventSeq, OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
pub use model::{ModelCapabilities, ReasoningMode};
pub use op::{EffectStatus, OpMeta, OpState, RecoveryStrategy};
pub use path::{
    NormalizedWorkspacePath, PathViolation, PathViolationKind, MAX_NORMALIZED_WORKSPACE_PATH_BYTES,
};
pub use resource::{ResourceClass, ResourceLimits};
pub use retry::{RetryClass, RetryPolicy};
pub use state::{AgentState, SessionLifecycle, StateMachine};
pub use time::{Clock, Deadline, SystemClock, TestClock};

/// The Faktor daemon version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The Faktor-owned UI baseline this build ships as its frontend.
pub const UX_BASELINE: &str = "faktor-native-ui-1";

/// Every file/tool call explicitly carries its workspace identity.
/// There is no global mutable "current directory" in the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceIdentity {
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub task_id: TaskId,
}

impl WorkspaceIdentity {
    pub const fn new(workspace_id: WorkspaceId, worktree_id: WorktreeId, task_id: TaskId) -> Self {
        Self {
            workspace_id,
            worktree_id,
            task_id,
        }
    }
}

/// Zero is never a valid identifier; every ID newtype constructor routes
/// its invariant check through this single authority.
///
/// A macro (not a function) because the check must stay callable from the
/// `const fn` constructors while the panic still names the offending id
/// TYPE: a shared `const fn` cannot format its message (const-formatting is
/// unstable) and a shared non-const fn would de-`const` every id
/// constructor (e.g. the const evidence access-scope builders).
macro_rules! reject_zero {
    ($raw:expr, $what:expr $(,)?) => {{
        let raw = $raw;
        assert!(
            raw != 0,
            concat!("faktor-core invariant violated: ", $what, " cannot be 0")
        );
        raw
    }};
}
pub(crate) use reject_zero;

#[cfg(test)]
mod tests {
    use crate::id::{
        EventSeq, OpId, ProviderCallId, SessionId, TaskId, TaskRevision, VerificationRecordId,
        WorkspaceId, WorktreeId,
    };

    /// Adversarial: every ID constructor must reject zero through the one
    /// authority, and the panic must name the offending id type (a silent
    /// zero would violate the id contract at the construction boundary).
    #[test]
    fn every_id_constructor_rejects_zero_and_names_the_type() {
        macro_rules! assert_zero_rejected {
            ($($t:ty),+ $(,)?) => {$(
                let caught = std::panic::catch_unwind(|| <$t>::new(0));
                let payload = caught.expect_err(concat!(stringify!($t), "::new(0) must panic"));
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                assert!(
                    msg.contains("cannot be 0") && msg.contains(stringify!($t)),
                    "panic for {}::new(0) must name the type and the zero violation: {msg}",
                    stringify!($t)
                );
            )+};
        }
        assert_zero_rejected!(
            SessionId,
            WorkspaceId,
            WorktreeId,
            TaskId,
            VerificationRecordId,
            TaskRevision,
            OpId,
            ProviderCallId,
            EventSeq,
        );
        // The shared authority itself refuses zero and passes non-zero
        // through unchanged.
        assert_eq!(reject_zero!(1, "test"), 1);
        let caught = std::panic::catch_unwind(|| reject_zero!(0, "probe"));
        assert!(caught.is_err(), "reject_zero!(0) must panic");
    }
}
