//! Provider-based verification interface (`docs/specs/autonomous-engineering.md`).
//!
//! Verification is not one engine: it is an interface with a NATIVE baseline
//! and OPTIONAL external providers. This module defines the typed contract:
//!
//! - [`VerificationOp`] enumerates the verification work Faktor can ask for
//!   (build, lint, typecheck, tests, contracts, compatibility, security,
//!   performance, runtime checks, and claim verification).
//! - [`VerificationRequest`] carries the explicit async-operation envelope
//!   (`op_id`, `session_id`, `workspace_id`, `created_ms`, `deadline_ms`,
//!   cancellation), a bounded target/selector, an optional claim, and the
//!   output bound. Every field is bounded and validated before dispatch.
//! - [`VerificationResponse`] is the result: a typed outcome
//!   (`Passed | Failed | Unavailable | Refused`), an optional exit code, a
//!   BOUNDED summary, bounded [`ArtifactRef`] evidence, and a duration.
//! - [`VerificationError`] is the typed refusal surface. The variants are
//!   structural: an unavailable provider is never a silent pass, an oversized
//!   payload is never silently truncated by a consumer, and an unknown op is
//!   never guessed into a known one.
//! - [`VerificationProvider`] is the provider contract (boxed futures, no
//!   async-runtime dependency beyond what callers already carry).
//! - [`VerificationRegistry`] dispatches over ordered providers (native
//!   first, optional externals after). A call carrying
//!   [`VerificationRequest::require_provider`] is served by an EXTERNAL
//!   provider or refused typed — a native substitution is never silent
//!   fallback for a call that demanded independent verification.
//!
//! The native baseline lives in [`crate::native`]; the optional Kiwi-shaped
//! external adapter lives in the `faktor-kiwi` crate. Neither is required by
//! this interface: a registry without externals is a complete, honest
//! native-only registry.

use std::fmt;
use std::future::Future;
use std::path::{Component, Path};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use faktor_core::contract::{EvidenceRef, ProofEvidenceKind};
use faktor_core::{CancellationToken, Deadline, OpId, SessionId, WorkspaceId};
use serde::{Deserialize, Serialize};

/// Boxed future returned by every [`VerificationProvider`] async method.
/// Standard library only: this crate carries no async runtime of its own.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Maximum bytes of one verification provider id.
pub const MAX_PROVIDER_ID_BYTES: usize = 128;
/// Maximum workspace-relative paths in one bounded target.
pub const MAX_TARGET_PATHS: usize = 4096;
/// Maximum bytes of one workspace-relative target path.
pub const MAX_TARGET_PATH_BYTES: usize = 1024;
/// Maximum bytes of one claim handed to a provider.
pub const MAX_CLAIM_BYTES: usize = 16 * 1024;
/// Maximum bytes of a response summary AFTER output bounding.
pub const MAX_SUMMARY_BYTES: usize = 16 * 1024;
/// Maximum bytes of one evidence source label.
pub const MAX_ARTIFACT_SOURCE_BYTES: usize = 512;
/// Maximum bytes of one evidence detail.
pub const MAX_ARTIFACT_DETAIL_BYTES: usize = 4 * 1024;
/// Maximum evidence refs one response may carry.
pub const MAX_EVIDENCE_REFS: usize = 1024;
/// Default bound on a response's total (summary + evidence) output.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Hard ceiling of one request's `max_output_bytes` (a request asking for
/// more is a typed [`VerificationError::OutputTooLarge`]).
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// One kind of verification work. Serde names are stable wire names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationOp {
    /// Compile/build the target.
    Build,
    /// Static lint analysis of the target.
    Lint,
    /// Type/interface checking of the target.
    Typecheck,
    /// Run the target's existing test suite.
    RunTests,
    /// Run a generated test suite (a provider that generated tests runs
    /// them; the native baseline holds no generated-test inventory).
    RunGeneratedTests,
    /// Check the stated contracts.
    CheckContracts,
    /// Check the stated compatibility constraints.
    CheckCompatibility,
    /// Run security checks against the target.
    RunSecurityChecks,
    /// Run performance checks against the target.
    RunPerformanceChecks,
    /// Run runtime checks against the target.
    RunRuntimeChecks,
    /// Evaluate one arbitrary semantic claim.
    VerifyClaim,
}

impl VerificationOp {
    pub const ALL: [VerificationOp; 11] = [
        VerificationOp::Build,
        VerificationOp::Lint,
        VerificationOp::Typecheck,
        VerificationOp::RunTests,
        VerificationOp::RunGeneratedTests,
        VerificationOp::CheckContracts,
        VerificationOp::CheckCompatibility,
        VerificationOp::RunSecurityChecks,
        VerificationOp::RunPerformanceChecks,
        VerificationOp::RunRuntimeChecks,
        VerificationOp::VerifyClaim,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            VerificationOp::Build => "build",
            VerificationOp::Lint => "lint",
            VerificationOp::Typecheck => "typecheck",
            VerificationOp::RunTests => "run_tests",
            VerificationOp::RunGeneratedTests => "run_generated_tests",
            VerificationOp::CheckContracts => "check_contracts",
            VerificationOp::CheckCompatibility => "check_compatibility",
            VerificationOp::RunSecurityChecks => "run_security_checks",
            VerificationOp::RunPerformanceChecks => "run_performance_checks",
            VerificationOp::RunRuntimeChecks => "run_runtime_checks",
            VerificationOp::VerifyClaim => "verify_claim",
        }
    }

    pub const fn bit(self) -> u32 {
        match self {
            VerificationOp::Build => 1 << 0,
            VerificationOp::Lint => 1 << 1,
            VerificationOp::Typecheck => 1 << 2,
            VerificationOp::RunTests => 1 << 3,
            VerificationOp::RunGeneratedTests => 1 << 4,
            VerificationOp::CheckContracts => 1 << 5,
            VerificationOp::CheckCompatibility => 1 << 6,
            VerificationOp::RunSecurityChecks => 1 << 7,
            VerificationOp::RunPerformanceChecks => 1 << 8,
            VerificationOp::RunRuntimeChecks => 1 << 9,
            VerificationOp::VerifyClaim => 1 << 10,
        }
    }

    /// Parse a wire op name. Unknown names are a typed
    /// [`VerificationError::UnknownOp`], never a guessed default.
    pub fn parse(raw: &str) -> Result<Self, VerificationError> {
        for op in Self::ALL {
            if op.as_str() == raw {
                return Ok(op);
            }
        }
        Err(VerificationError::UnknownOp(format!(
            "verification op {raw:?} is unknown to this build"
        )))
    }
}

/// Bitflags-like capability descriptor: which operations a provider serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VerificationCapabilities {
    ops: u32,
}

impl VerificationCapabilities {
    pub const NONE: Self = Self { ops: 0 };
    pub const ALL: Self = Self {
        ops: 0b111_1111_1111,
    };

    pub const fn of(op: VerificationOp) -> Self {
        Self { ops: op.bit() }
    }

    /// The minimal capability value covering one op (selection comparison).
    pub const fn for_op(op: VerificationOp) -> Self {
        Self { ops: op.bit() }
    }

    pub const fn supports(self, op: VerificationOp) -> bool {
        self.ops & op.bit() != 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self {
            ops: self.ops | other.ops,
        }
    }

    /// True when `self` covers every bit of `required`.
    pub const fn covers(self, required: Self) -> bool {
        self.ops & required.ops == required.ops
    }

    pub const fn is_empty(self) -> bool {
        self.ops == 0
    }
}

impl Default for VerificationCapabilities {
    fn default() -> Self {
        Self::NONE
    }
}

/// A validated verification provider id. Never empty, never oversized, never
/// carrying characters outside `[A-Za-z0-9._-]` so ids embed in logs and
/// registry keys without escaping games.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct VerificationProviderId(String);

impl VerificationProviderId {
    pub fn parse(raw: &str) -> Result<Self, VerificationError> {
        if raw.is_empty() {
            return Err(VerificationError::Malformed(
                "verification provider id must not be empty".to_string(),
            ));
        }
        if raw.len() > MAX_PROVIDER_ID_BYTES {
            return Err(VerificationError::Malformed(format!(
                "verification provider id is {} bytes, over the {MAX_PROVIDER_ID_BYTES}-byte bound",
                raw.len()
            )));
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(VerificationError::Malformed(format!(
                "verification provider id {raw:?} must match [A-Za-z0-9._-]+"
            )));
        }
        Ok(Self(raw.to_string()))
    }

    /// The canonical id of the in-process native baseline.
    pub fn native() -> Self {
        Self("native".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VerificationProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for VerificationProviderId {
    type Err = VerificationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl<'de> Deserialize<'de> for VerificationProviderId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The bounded target of one verification request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationTarget {
    /// The whole workspace as currently checked out.
    #[default]
    Workspace,
    /// A bounded, workspace-relative set of paths (changed files, a
    /// component selector). Absolute paths, `..`, backslashes and NULs are
    /// refused typed at validation.
    Paths(Vec<String>),
}

impl VerificationTarget {
    /// Build a validated path target (normalized by validation, order
    /// preserved; callers that need determinism sort themselves).
    pub fn paths(
        paths: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, VerificationError> {
        let target = Self::Paths(paths.into_iter().map(Into::into).collect());
        target.validate()?;
        Ok(target)
    }

    pub fn validate(&self) -> Result<(), VerificationError> {
        let Self::Paths(paths) = self else {
            return Ok(());
        };
        if paths.len() > MAX_TARGET_PATHS {
            return Err(VerificationError::Malformed(format!(
                "verification target carries {} paths, over the {MAX_TARGET_PATHS}-path bound",
                paths.len()
            )));
        }
        for path in paths {
            if path.is_empty() {
                return Err(VerificationError::Malformed(
                    "verification target carries an empty path".to_string(),
                ));
            }
            if path.len() > MAX_TARGET_PATH_BYTES {
                return Err(VerificationError::Malformed(format!(
                    "verification target path is {} bytes, over the {MAX_TARGET_PATH_BYTES}-byte bound",
                    path.len()
                )));
            }
            if path.contains('\0') {
                return Err(VerificationError::Malformed(
                    "verification target path carries a NUL byte".to_string(),
                ));
            }
            if path.contains('\\') {
                return Err(VerificationError::Malformed(format!(
                    "verification target path {path:?} uses a platform separator outside the workspace grammar"
                )));
            }
            let parsed = Path::new(path);
            if parsed.is_absolute() {
                return Err(VerificationError::Malformed(format!(
                    "verification target path {path:?} is absolute"
                )));
            }
            for component in parsed.components() {
                match component {
                    Component::Normal(_) | Component::CurDir => {}
                    Component::ParentDir => {
                        return Err(VerificationError::Malformed(format!(
                            "verification target path {path:?} escapes the workspace root"
                        )))
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        return Err(VerificationError::Malformed(format!(
                            "verification target path {path:?} is absolute"
                        )))
                    }
                }
            }
        }
        Ok(())
    }

    pub fn is_workspace(&self) -> bool {
        matches!(self, Self::Workspace)
    }

    /// The workspace-relative paths, empty for the whole-workspace target.
    pub fn paths_ref(&self) -> &[String] {
        match self {
            Self::Workspace => &[],
            Self::Paths(paths) => paths,
        }
    }
}

/// One bounded reference to evidence a verification produced. The `kind`
/// reuses the frozen contract taxonomy (`ProofEvidenceKind`), so a response's
/// evidence can discharge proof obligations directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub kind: ProofEvidenceKind,
    /// The producing surface (check id, command, provider).
    pub source: String,
    /// Bounded human-readable detail.
    pub detail: String,
}

impl ArtifactRef {
    pub fn new(
        kind: ProofEvidenceKind,
        source: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<Self, VerificationError> {
        let source = source.into();
        let detail = detail.into();
        if source.is_empty() || source.len() > MAX_ARTIFACT_SOURCE_BYTES {
            return Err(VerificationError::Malformed(format!(
                "artifact source must be 1..={MAX_ARTIFACT_SOURCE_BYTES} bytes"
            )));
        }
        if detail.len() > MAX_ARTIFACT_DETAIL_BYTES {
            return Err(VerificationError::OutputTooLarge {
                max: MAX_ARTIFACT_DETAIL_BYTES,
                actual: detail.len(),
            });
        }
        Ok(Self {
            kind,
            source,
            detail,
        })
    }

    /// The frozen contract evidence ref this artifact discharges with.
    pub fn to_evidence_ref(&self) -> EvidenceRef {
        EvidenceRef {
            kind: self.kind,
            source: self.source.clone(),
            detail: self.detail.clone(),
        }
    }

    pub fn byte_len(&self) -> usize {
        self.source.len() + self.detail.len()
    }
}

/// One bounded verification request: the explicit async-operation envelope
/// plus the work selection. All fields are bounded; [`Self::validate`]
/// refuses hostile shapes before any provider (or process) is touched.
#[derive(Debug, Clone)]
pub struct VerificationRequest {
    pub op_id: OpId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub created_ms: i64,
    /// Absolute epoch-millisecond deadline; `None` = no caller deadline (the
    /// provider's own watchdog still bounds the work).
    pub deadline_ms: Option<i64>,
    pub cancellation: CancellationToken,
    pub op: VerificationOp,
    pub target: VerificationTarget,
    /// The claim to evaluate (required for [`VerificationOp::VerifyClaim`],
    /// optional context elsewhere).
    pub claim: Option<String>,
    pub max_output_bytes: usize,
    /// Demand INDEPENDENT verification: dispatch skips the native baseline
    /// and refuses typed when no external provider is configured.
    pub require_provider: bool,
}

impl VerificationRequest {
    pub fn new(
        op_id: OpId,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        created_ms: i64,
        cancellation: CancellationToken,
        op: VerificationOp,
    ) -> Self {
        Self {
            op_id,
            session_id,
            workspace_id,
            created_ms,
            deadline_ms: None,
            cancellation,
            op,
            target: VerificationTarget::Workspace,
            claim: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            require_provider: false,
        }
    }

    pub fn with_target(mut self, target: VerificationTarget) -> Self {
        self.target = target;
        self
    }

    pub fn with_claim(mut self, claim: impl Into<String>) -> Self {
        self.claim = Some(claim.into());
        self
    }

    pub fn with_deadline_ms(mut self, deadline_ms: i64) -> Self {
        self.deadline_ms = Some(deadline_ms);
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Demand independent verification (see the field docs).
    pub fn requiring_provider(mut self) -> Self {
        self.require_provider = true;
        self
    }

    /// The caller deadline as a core [`Deadline`], when one was given.
    pub fn deadline(&self) -> Option<Deadline> {
        self.deadline_ms.map(Deadline::at)
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.deadline().is_some_and(|d| d.is_expired(now_ms))
    }

    pub fn validate(&self) -> Result<(), VerificationError> {
        self.target.validate()?;
        if let Some(claim) = &self.claim {
            if claim.is_empty() {
                return Err(VerificationError::Malformed(
                    "verification claim must not be empty".to_string(),
                ));
            }
            if claim.len() > MAX_CLAIM_BYTES {
                return Err(VerificationError::Malformed(format!(
                    "verification claim is {} bytes, over the {MAX_CLAIM_BYTES}-byte bound",
                    claim.len()
                )));
            }
        }
        if self.op == VerificationOp::VerifyClaim && self.claim.is_none() {
            return Err(VerificationError::Malformed(
                "VerifyClaim requires a claim".to_string(),
            ));
        }
        if self.max_output_bytes == 0 {
            return Err(VerificationError::Malformed(
                "max_output_bytes must be non-zero".to_string(),
            ));
        }
        if self.max_output_bytes > MAX_OUTPUT_BYTES {
            return Err(VerificationError::OutputTooLarge {
                max: MAX_OUTPUT_BYTES,
                actual: self.max_output_bytes,
            });
        }
        if let Some(deadline_ms) = self.deadline_ms {
            if deadline_ms <= self.created_ms {
                return Err(VerificationError::Malformed(format!(
                    "deadline_ms {deadline_ms} is not after created_ms {}",
                    self.created_ms
                )));
            }
        }
        Ok(())
    }
}

/// The honest verdict of one verification operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationOutcome {
    /// The verified claim holds (as far as this provider observed).
    Passed,
    /// The verified claim does not hold (a failure was observed).
    Failed,
    /// No verdict was possible (missing toolchain, no derivable check, a
    /// provider that cannot serve the operation).
    Unavailable,
    /// A policy or contract refusal stands in the way of verification.
    Refused,
}

/// One verification result. `summary` and `evidence` are bounded; consumers
/// may relay them without re-bounding but must not trust an unvalidated
/// response (the registry validates every provider response against its
/// request before returning it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResponse {
    pub provider: VerificationProviderId,
    pub op: VerificationOp,
    pub outcome: VerificationOutcome,
    /// Exit code when a single process verdict exists.
    pub exit_code: Option<i32>,
    pub summary: String,
    pub evidence: Vec<ArtifactRef>,
    pub duration_ms: u64,
}

impl VerificationResponse {
    pub fn new(
        provider: VerificationProviderId,
        op: VerificationOp,
        outcome: VerificationOutcome,
    ) -> Self {
        Self {
            provider,
            op,
            outcome,
            exit_code: None,
            summary: String::new(),
            evidence: Vec::new(),
            duration_ms: 0,
        }
    }

    pub fn with_exit_code(mut self, exit_code: Option<i32>) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    pub fn with_evidence(mut self, evidence: Vec<ArtifactRef>) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// Total bounded output bytes (summary + every evidence ref).
    pub fn output_bytes(&self) -> usize {
        self.summary.len()
            + self
                .evidence
                .iter()
                .map(ArtifactRef::byte_len)
                .sum::<usize>()
    }

    /// Tail-truncate summary and evidence (last bytes of each field, like the
    /// executor's bounded ring tail) until the response fits `max_total`.
    /// Evidence entries are bounded per field, then the summary is trimmed,
    /// then evidence DETAILS shrink from the end, then whole entries drop.
    pub fn fit_output(&mut self, max_total: usize) {
        for artifact in &mut self.evidence {
            artifact.detail = tail_bytes(&artifact.detail, MAX_ARTIFACT_DETAIL_BYTES);
        }
        if self.output_bytes() <= max_total {
            return;
        }
        let mut total = self.output_bytes();
        let excess = total - max_total;
        if self.summary.len() >= excess {
            let trimmed = tail_bytes(&self.summary, self.summary.len() - excess);
            total -= self.summary.len() - trimmed.len();
            self.summary = trimmed;
        } else {
            total -= self.summary.len();
            self.summary.clear();
        }
        let mut index = self.evidence.len();
        while total > max_total && index > 0 {
            index -= 1;
            let excess = total - max_total;
            let artifact = &mut self.evidence[index];
            if artifact.detail.len() >= excess {
                let trimmed = tail_bytes(&artifact.detail, artifact.detail.len() - excess);
                total -= artifact.detail.len() - trimmed.len();
                artifact.detail = trimmed;
            } else {
                total -= artifact.detail.len();
                artifact.detail.clear();
            }
        }
        while total > max_total && !self.evidence.is_empty() {
            let removed = self.evidence.pop().expect("non-empty");
            total -= removed.byte_len();
        }
    }

    /// Validate the response against its request: op identity, per-field
    /// bounds and the request's total output bound. Every failure is typed.
    pub fn validate(&self, request: &VerificationRequest) -> Result<(), VerificationError> {
        if self.op != request.op {
            return Err(VerificationError::Malformed(format!(
                "verification response op {} does not match the request op {}",
                self.op.as_str(),
                request.op.as_str()
            )));
        }
        if self.summary.len() > MAX_SUMMARY_BYTES {
            return Err(VerificationError::OutputTooLarge {
                max: MAX_SUMMARY_BYTES,
                actual: self.summary.len(),
            });
        }
        if self.evidence.len() > MAX_EVIDENCE_REFS {
            return Err(VerificationError::OutputTooLarge {
                max: MAX_EVIDENCE_REFS,
                actual: self.evidence.len(),
            });
        }
        for artifact in &self.evidence {
            if artifact.source.is_empty() || artifact.source.len() > MAX_ARTIFACT_SOURCE_BYTES {
                return Err(VerificationError::Malformed(format!(
                    "artifact source must be 1..={MAX_ARTIFACT_SOURCE_BYTES} bytes"
                )));
            }
            if artifact.detail.len() > MAX_ARTIFACT_DETAIL_BYTES {
                return Err(VerificationError::OutputTooLarge {
                    max: MAX_ARTIFACT_DETAIL_BYTES,
                    actual: artifact.detail.len(),
                });
            }
        }
        let total = self.output_bytes();
        if total > request.max_output_bytes {
            return Err(VerificationError::OutputTooLarge {
                max: request.max_output_bytes,
                actual: total,
            });
        }
        Ok(())
    }
}

/// Typed verification error. Callers react structurally: a caller-side
/// deadline is terminal, an unavailable provider is degradable, an oversized
/// payload is never silently truncated by a consumer, and an unknown op is
/// never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationError {
    /// A hard invariant or policy refused the request.
    Refused(String),
    /// The caller deadline expired or the call was cancelled before a
    /// verdict: terminal, never failed over and never degraded to a pass.
    Timeout(String),
    /// The provider cannot serve this operation or has no usable transport
    /// right now: recoverable, dispatch may fail over to the next provider.
    Unavailable(String),
    /// Input or a provider response could not be parsed/validated.
    Malformed(String),
    /// A bounded payload exceeded its explicit bound (request workload
    /// ceiling or provider output). Never silently truncated.
    OutputTooLarge { max: usize, actual: usize },
    /// An operation name this build does not know.
    UnknownOp(String),
}

impl VerificationError {
    /// True when the error is terminal for the CALLER (deadline/cancel):
    /// dispatch must never fail over to another provider after one of these.
    /// A recoverable [`VerificationError::Unavailable`] is NOT terminal.
    pub const fn caller_terminal(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }
}

impl fmt::Display for VerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(detail) => write!(f, "verification refused: {detail}"),
            Self::Timeout(detail) => write!(f, "verification timed out: {detail}"),
            Self::Unavailable(detail) => write!(f, "verification unavailable: {detail}"),
            Self::Malformed(detail) => write!(f, "malformed verification payload: {detail}"),
            Self::OutputTooLarge { max, actual } => write!(
                f,
                "verification output oversized: {actual} exceeds the {max} bound"
            ),
            Self::UnknownOp(detail) => write!(f, "unknown verification op: {detail}"),
        }
    }
}

impl std::error::Error for VerificationError {}

/// The provider contract. `id`/`capabilities` are synchronous; verification
/// is explicit async returning a validated response. A provider that does not
/// support an op returns the typed capability boundary — never a fabricated
/// verdict.
pub trait VerificationProvider: Send + Sync {
    /// Stable identity of this provider (bounded, validated).
    fn id(&self) -> VerificationProviderId;

    /// TRUE for the in-process native baseline. Dispatch uses this to keep
    /// `require_provider` calls away from the baseline.
    fn is_native(&self) -> bool {
        false
    }

    /// The operations this provider can serve.
    fn capabilities(&self) -> VerificationCapabilities;

    /// Whether this provider can serve one op (defaults to its declared
    /// capabilities).
    fn supports(&self, op: VerificationOp) -> bool {
        self.capabilities().supports(op)
    }

    /// Execute one bounded verification request.
    fn verify(
        &self,
        request: VerificationRequest,
    ) -> BoxFuture<'_, Result<VerificationResponse, VerificationError>>;
}

/// Ordered verification providers with capability-driven dispatch. Native
/// providers always precede externals regardless of registration order.
///
/// Dispatch rules:
///
/// 1. the request is validated (bounds) before any provider is consulted;
/// 2. caller cancellation/deadline are terminal — no fail-over;
/// 3. candidates are the providers supporting the op; with
///    [`VerificationRequest::require_provider`] the native baseline is
///    excluded (an absent external provider is then a typed
///    [`VerificationError::Unavailable`], never a silent native answer);
/// 4. an `Unavailable` OUTCOME is honest ("no verdict") and advances to the
///    next candidate; `Passed`/`Failed`/`Refused` outcomes are returned as
///    the verdict; a recoverable `Unavailable` ERROR advances too;
/// 5. after every candidate, the last honest `Unavailable` response is
///    returned when one exists, otherwise the last typed error.
pub struct VerificationRegistry {
    providers: Vec<Arc<dyn VerificationProvider>>,
}

impl Default for VerificationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl VerificationRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Register a provider. Order is preference order within a class; native
    /// providers are always ordered before external providers.
    pub fn register(&mut self, provider: Arc<dyn VerificationProvider>) -> &mut Self {
        let index = if provider.is_native() {
            self.providers
                .iter()
                .position(|existing| !existing.is_native())
                .unwrap_or(self.providers.len())
        } else {
            self.providers.len()
        };
        self.providers.insert(index, provider);
        self
    }

    pub fn providers(&self) -> &[Arc<dyn VerificationProvider>] {
        &self.providers
    }

    /// TRUE when at least one EXTERNAL (non-native) provider is registered.
    pub fn has_external_provider(&self) -> bool {
        self.providers.iter().any(|provider| !provider.is_native())
    }

    /// TRUE when an external provider's declared capabilities cover `op`.
    pub fn has_external_provider_for(&self, op: VerificationOp) -> bool {
        self.providers
            .iter()
            .any(|provider| !provider.is_native() && provider.supports(op))
    }

    /// Dispatch one request over the registry (see the type docs).
    pub async fn verify(
        &self,
        request: VerificationRequest,
    ) -> Result<VerificationResponse, VerificationError> {
        request.validate()?;
        if request.cancellation.is_cancelled() {
            return Err(VerificationError::Timeout(
                "request was cancelled before dispatch".to_string(),
            ));
        }
        let now_ms = now_epoch_ms();
        if request.is_expired(now_ms) {
            return Err(VerificationError::Timeout(
                "request deadline expired before dispatch".to_string(),
            ));
        }

        let mut unavailable_response: Option<VerificationResponse> = None;
        let mut last_unavailable_error: Option<VerificationError> = None;
        let mut external_skipped = false;
        let mut candidates = 0usize;
        for provider in &self.providers {
            if !provider.supports(request.op) {
                continue;
            }
            if request.require_provider && provider.is_native() {
                external_skipped = true;
                continue;
            }
            candidates += 1;
            match provider.verify(request.clone()).await {
                Ok(response) => {
                    response.validate(&request)?;
                    if response.outcome == VerificationOutcome::Unavailable {
                        unavailable_response = Some(response);
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) if err.caller_terminal() => return Err(err),
                Err(err @ VerificationError::Unavailable(_)) => {
                    last_unavailable_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }

        if let Some(response) = unavailable_response {
            return Ok(response);
        }
        if let Some(err) = last_unavailable_error {
            return Err(err);
        }
        if request.require_provider && external_skipped {
            return Err(VerificationError::Unavailable(format!(
                "operation {} requires an external verification provider (require_provider); \
                 none is configured or capable",
                request.op.as_str()
            )));
        }
        if candidates == 0 {
            return Err(VerificationError::Unavailable(format!(
                "no registered verification provider serves operation {}",
                request.op.as_str()
            )));
        }
        Err(VerificationError::Unavailable(format!(
            "no verification provider produced a verdict for operation {}",
            request.op.as_str()
        )))
    }
}

/// The current epoch in milliseconds (the request envelope's clock).
pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Keep the LAST `keep` bytes of `text` on a char boundary (bounded ring-tail
/// semantics, like the executor's summary).
pub(crate) fn tail_bytes(text: &str, keep: usize) -> String {
    if text.len() <= keep {
        return text.to_string();
    }
    let mut start = text.len() - keep;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: VerificationOp) -> VerificationRequest {
        VerificationRequest::new(
            OpId::new(1),
            SessionId::new(1),
            WorkspaceId::new(1),
            now_epoch_ms(),
            CancellationToken::new(),
            op,
        )
    }

    struct FakeProvider {
        id: &'static str,
        native: bool,
        ops: VerificationCapabilities,
        outcome: VerificationOutcome,
        error: Option<VerificationError>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        summary: String,
    }

    impl FakeProvider {
        fn new(id: &'static str, native: bool, outcome: VerificationOutcome) -> Self {
            Self {
                id,
                native,
                ops: VerificationCapabilities::ALL,
                outcome,
                error: None,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                summary: "fake".to_string(),
            }
        }

        fn failing(id: &'static str, error: VerificationError) -> Self {
            Self {
                error: Some(error),
                ..Self::new(id, false, VerificationOutcome::Unavailable)
            }
        }
    }

    impl VerificationProvider for FakeProvider {
        fn id(&self) -> VerificationProviderId {
            VerificationProviderId::parse(self.id).unwrap()
        }

        fn is_native(&self) -> bool {
            self.native
        }

        fn capabilities(&self) -> VerificationCapabilities {
            self.ops
        }

        fn verify(
            &self,
            request: VerificationRequest,
        ) -> BoxFuture<'_, Result<VerificationResponse, VerificationError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = self.outcome;
            let summary = self.summary.clone();
            let error = self.error.clone();
            let id = self.id();
            Box::pin(async move {
                if let Some(error) = error {
                    return Err(error);
                }
                Ok(VerificationResponse::new(id, request.op, outcome).with_summary(summary))
            })
        }
    }

    #[test]
    fn hostile_ops_ids_targets_and_bounds_are_refused_typed() {
        match VerificationOp::parse("melted") {
            Err(VerificationError::UnknownOp(detail)) => assert!(detail.contains("melted")),
            other => panic!("unknown op must be UnknownOp, got {other:?}"),
        }
        for raw in [
            "",
            "has space",
            "semi;colon",
            "nl\n",
            "x".repeat(129).as_str(),
        ] {
            assert!(
                VerificationProviderId::parse(raw).is_err(),
                "{raw:?} must not be a provider id"
            );
        }
        assert!(VerificationProviderId::parse("kiwi.verifier-1").is_ok());
        for hostile in [
            "../escape",
            "/etc/passwd",
            "C:\\windows",
            "a\\b",
            "nul\0byte",
            "",
        ] {
            assert!(
                VerificationTarget::paths([hostile]).is_err(),
                "{hostile:?} must not be a target path"
            );
        }
        assert!(VerificationTarget::paths(["src/lib.rs", "crates/x/src/y.rs"]).is_ok());

        let mut req = request(VerificationOp::Build);
        req.max_output_bytes = 0;
        assert!(matches!(
            req.validate(),
            Err(VerificationError::Malformed(_))
        ));
        req.max_output_bytes = MAX_OUTPUT_BYTES + 1;
        assert!(matches!(
            req.validate(),
            Err(VerificationError::OutputTooLarge {
                max: MAX_OUTPUT_BYTES,
                ..
            })
        ));
        let mut req = request(VerificationOp::VerifyClaim);
        assert!(
            matches!(req.validate(), Err(VerificationError::Malformed(_))),
            "VerifyClaim without a claim must be malformed"
        );
        req.claim = Some("x".repeat(MAX_CLAIM_BYTES + 1));
        assert!(matches!(
            req.validate(),
            Err(VerificationError::Malformed(_))
        ));
        let mut req = request(VerificationOp::Build);
        req.deadline_ms = Some(req.created_ms);
        assert!(matches!(
            req.validate(),
            Err(VerificationError::Malformed(_))
        ));
    }

    #[test]
    fn registry_orders_native_before_externals_and_reports_them() {
        let mut registry = VerificationRegistry::new();
        let external: Arc<dyn VerificationProvider> = Arc::new(FakeProvider::new(
            "kiwi",
            false,
            VerificationOutcome::Passed,
        ));
        let native: Arc<dyn VerificationProvider> = Arc::new(FakeProvider::new(
            "native",
            true,
            VerificationOutcome::Passed,
        ));
        registry.register(external);
        registry.register(native);
        let ids: Vec<String> = registry
            .providers()
            .iter()
            .map(|provider| provider.id().to_string())
            .collect();
        assert_eq!(ids, vec!["native", "kiwi"], "native is always first");
        assert!(registry.has_external_provider());
        assert!(registry.has_external_provider_for(VerificationOp::VerifyClaim));
    }

    #[tokio::test]
    async fn require_provider_refuses_a_native_substitution() {
        let mut registry = VerificationRegistry::new();
        let native = Arc::new(FakeProvider::new(
            "native",
            true,
            VerificationOutcome::Passed,
        ));
        let calls = native.calls.clone();
        registry.register(native);
        let err = registry
            .verify(request(VerificationOp::Build).requiring_provider())
            .await
            .expect_err("native-only registry must refuse require_provider");
        match err {
            VerificationError::Unavailable(detail) => {
                assert!(detail.contains("external"), "{detail}");
                assert!(detail.contains("build"), "{detail}");
            }
            other => panic!("expected typed Unavailable, got {other:?}"),
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the native provider must never be consulted for a provider-required call"
        );

        // With an external registered, the same call is served by it.
        let external = Arc::new(FakeProvider::new(
            "kiwi",
            false,
            VerificationOutcome::Passed,
        ));
        let external_calls = external.calls.clone();
        registry.register(external);
        let response = registry
            .verify(request(VerificationOp::Build).requiring_provider())
            .await
            .unwrap();
        assert_eq!(response.provider.as_str(), "kiwi");
        assert_eq!(external_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unavailable_outcomes_fail_over_but_verdicts_do_not() {
        let mut registry = VerificationRegistry::new();
        let native = Arc::new(FakeProvider::new(
            "native",
            true,
            VerificationOutcome::Unavailable,
        ));
        let external = Arc::new(FakeProvider::new(
            "kiwi",
            false,
            VerificationOutcome::Failed,
        ));
        registry.register(native);
        registry.register(external);
        let response = registry.verify(request(VerificationOp::VerifyClaim)).await;
        // VerifyClaim without a claim is malformed before dispatch...
        assert!(matches!(response, Err(VerificationError::Malformed(_))));
        let response = registry
            .verify(request(VerificationOp::Build).with_claim("c"))
            .await
            .unwrap();
        assert_eq!(
            response.outcome,
            VerificationOutcome::Failed,
            "the external verdict wins over the native unavailable outcome"
        );

        // A Failed verdict never fails over: the first verdict is returned.
        let mut verdict_registry = VerificationRegistry::new();
        let first = Arc::new(FakeProvider::new(
            "first",
            true,
            VerificationOutcome::Failed,
        ));
        let second = Arc::new(FakeProvider::new(
            "second",
            false,
            VerificationOutcome::Passed,
        ));
        let second_calls = second.calls.clone();
        verdict_registry.register(first);
        verdict_registry.register(second);
        let response = verdict_registry
            .verify(request(VerificationOp::Build))
            .await
            .unwrap();
        assert_eq!(response.outcome, VerificationOutcome::Failed);
        assert_eq!(
            second_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a real verdict is terminal for dispatch"
        );
    }

    #[tokio::test]
    async fn unavailable_errors_fail_over_and_callers_timeout_is_terminal() {
        let mut registry = VerificationRegistry::new();
        let native = Arc::new(FakeProvider::failing(
            "native",
            VerificationError::Unavailable("no toolchain".to_string()),
        ));
        let external = Arc::new(FakeProvider::new(
            "kiwi",
            false,
            VerificationOutcome::Passed,
        ));
        registry.register(native);
        registry.register(external);
        let response = registry
            .verify(request(VerificationOp::Build))
            .await
            .unwrap();
        assert_eq!(response.provider.as_str(), "kiwi");

        // A caller-terminal error stops immediately, even with a candidate
        // left that could otherwise answer.
        let mut terminal = VerificationRegistry::new();
        let timed_out = Arc::new(FakeProvider::failing(
            "native",
            VerificationError::Timeout("deadline".to_string()),
        ));
        let never = Arc::new(FakeProvider::new(
            "kiwi",
            false,
            VerificationOutcome::Passed,
        ));
        let never_calls = never.calls.clone();
        terminal.register(timed_out);
        terminal.register(never);
        let err = terminal
            .verify(request(VerificationOp::Build))
            .await
            .expect_err("timeout is terminal");
        assert!(err.caller_terminal(), "{err:?}");
        assert_eq!(never_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_oversized_provider_response_is_refused_typed() {
        let mut registry = VerificationRegistry::new();
        let mut noisy = FakeProvider::new("noisy", true, VerificationOutcome::Passed);
        noisy.summary = "x".repeat(MAX_SUMMARY_BYTES + 1);
        registry.register(Arc::new(noisy));
        let mut req = request(VerificationOp::Build);
        req.max_output_bytes = 64;
        let err = registry
            .verify(req)
            .await
            .expect_err("oversized provider output must be refused");
        assert!(
            matches!(err, VerificationError::OutputTooLarge { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn empty_and_uncappable_registries_refuse_typed() {
        let registry = VerificationRegistry::new();
        let err = registry
            .verify(request(VerificationOp::Build))
            .await
            .expect_err("an empty registry cannot serve");
        assert!(matches!(err, VerificationError::Unavailable(_)));

        // Capability gating: a provider that does not support the op is
        // skipped, and the registry says so.
        let mut registry = VerificationRegistry::new();
        let mut limited = FakeProvider::new("native", true, VerificationOutcome::Passed);
        limited.ops = VerificationCapabilities::of(VerificationOp::Lint);
        registry.register(Arc::new(limited));
        let err = registry
            .verify(request(VerificationOp::Build))
            .await
            .expect_err("unsupported op must refuse typed");
        assert!(matches!(err, VerificationError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn response_output_bounding_is_tail_preserving_and_bounded() {
        let mut response = VerificationResponse::new(
            VerificationProviderId::native(),
            VerificationOp::Build,
            VerificationOutcome::Failed,
        )
        .with_summary("HEAD".to_string() + &"s".repeat(MAX_SUMMARY_BYTES))
        .with_evidence(vec![ArtifactRef::new(
            ProofEvidenceKind::Compile,
            "cargo_check",
            "d".repeat(MAX_ARTIFACT_DETAIL_BYTES),
        )
        .unwrap()]);
        response.fit_output(1024);
        assert!(
            response.output_bytes() <= 1024,
            "{}",
            response.output_bytes()
        );
        assert!(
            response.summary.is_empty() || response.summary.ends_with('s'),
            "the tail is preserved: {}",
            response.summary
        );
        // A per-field bound still holds after fitting.
        let request = request(VerificationOp::Build).with_max_output_bytes(1024);
        response.validate(&request).unwrap();
    }
}
