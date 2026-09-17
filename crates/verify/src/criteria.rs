//! Typed criterion evaluation (P0 criteria mandate): a criterion is PASSED
//! only through its OWN [`CriterionBinding`] — never through a global suite
//! status.
//!
//! The module is pure with respect to application state: it reads evidence
//! through the caller-supplied read-only ports ([`EvidenceResolver`],
//! [`ReadOnlyRepo`], [`AdditionalCheckRunner`], [`IndependentReviewer`]) and
//! returns a typed [`CriterionEvaluation`]. The fallback evaluator is an
//! INDEPENDENT verification agent: it receives the criterion text+id, the
//! candidate snapshot id, the bounded change set, the goal, read-only repo
//! access, the test/check outcomes and evidence retrieval — and it has NO
//! mutation surface. A reviewer output is only accepted when it is a
//! structured record tied to the exact candidate snapshot AND criterion id,
//! and a pass with zero evidence is rejected.
//!
//! The types are deliberately hostile-safe: every list is bounded, every
//! reviewer payload is parsed strictly, and every failure mode degrades to
//! [`CriterionEvaluation::Unavailable`] or [`CriterionEvaluation::Failed`] —
//! never to a pass.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use faktor_core::authority::{classify_authority_digest, AuthorityDigestKind};
use faktor_core::state::{command_binding_digest_parts, CriterionBinding};

/// Hard bound on the structured reviewer's evidence reference list.
pub const MAX_REVIEW_EVIDENCE_REFS: usize = 16;
/// Hard bound on the reviewer's explanation text.
pub const MAX_REVIEW_EXPLANATION_BYTES: usize = 2000;
/// Hard bound on the criterion text carried into a review request.
pub const MAX_REVIEW_CRITERION_BYTES: usize = 2000;
/// Hard bound on the bounded change set carried into a review request.
pub const MAX_REVIEW_CHANGE_SET: usize = 64;
/// Hard bound on the check outcome rows carried into a review request.
pub const MAX_REVIEW_CHECK_ROWS: usize = 64;
/// Hard bound on the diff bytes one additional verification check may read.
pub const MAX_ADDITIONAL_CHECK_DIFF_BYTES: usize = 64 * 1024;
/// Hard bound on the parsed additional-check argv.
pub const MAX_ADDITIONAL_CHECK_ARGS: usize = 32;

/// One executed check outcome row (the evaluator's check-outcome universe).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckOutcomeRow {
    pub check_id: String,
    pub command_digest: String,
    pub status: CheckOutcomeStatus,
    /// Bounded human evidence (summary/exit), never a substitute for proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// The status of one executed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcomeStatus {
    Passed,
    Failed,
    Unavailable,
}

/// One change-set entry of the VERIFIED CANDIDATE.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSetEntry {
    pub path: String,
    pub digest_hex: String,
    pub status: ChangeSetStatus,
}

/// The status of one candidate change-set entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetStatus {
    Added,
    Modified,
    Deleted,
}

/// One work item's contribution to the integrated change set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceContribution {
    pub work_item: String,
    /// Digest of the intended source/change set the work item contributed.
    pub source_digest: String,
    /// Candidate paths the contribution owns.
    pub paths: Vec<String>,
}

/// The integration-coverage evidence of one orchestrated run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationCoverageEvidence {
    pub run_id: String,
    pub contributions: Vec<SourceContribution>,
    /// Work items whose contribution disappeared (never silently ignored).
    #[serde(default)]
    pub disappeared: Vec<String>,
}

/// One immutable evidence record (resolved by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    pub digest: String,
    /// The candidate snapshot the evidence was captured for. Evidence for a
    /// snapshot other than the verified candidate can never certify it.
    pub snapshot: String,
}

/// Read-only evidence retrieval port.
pub trait EvidenceResolver: Send + Sync {
    fn resolve(&self, evidence_id: &str) -> Option<EvidenceRecord>;
}

impl<F> EvidenceResolver for F
where
    F: Fn(&str) -> Option<EvidenceRecord> + Send + Sync,
{
    fn resolve(&self, evidence_id: &str) -> Option<EvidenceRecord> {
        self(evidence_id)
    }
}

/// Read-only repository port over the VERIFIED CANDIDATE (no mutation).
pub trait ReadOnlyRepo: Send + Sync {
    /// The candidate digest of one workspace-relative path, `None` when the
    /// path cannot be resolved (never a guessed digest).
    fn hash_file(&self, path: &str) -> Option<String>;
}

impl<F> ReadOnlyRepo for F
where
    F: Fn(&str) -> Option<String> + Send + Sync,
{
    fn hash_file(&self, path: &str) -> Option<String> {
        self(path)
    }
}

/// One additional verification check a testable criterion may request; it
/// executes through the normal verifier and becomes a proof row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalCheckRequest {
    pub check_id: String,
    pub program: String,
    pub args: Vec<String>,
}

impl AdditionalCheckRequest {
    /// The canonical command text of this request (`program arg...`).
    pub fn command_text(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }

    /// The command digest of this request: the value the resulting proof row
    /// carries so the requesting binding resolves to it. The identity is the
    /// STRUCTURED (program, argv) pair — never the whitespace-joined command
    /// text — so no argv boundary can be forged.
    pub fn command_digest(&self) -> String {
        command_binding_digest_parts(&self.program, &self.args, None, &[])
    }

    /// Parse `verify: <program> <args...>` (bounded, whitespace-split, never
    /// a shell). Returns None when the criterion text is not an explicit
    /// additional-check request.
    pub fn parse_criterion_text(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("verify: ")?;
        let mut parts = rest.split_whitespace();
        let program = parts.next()?.to_string();
        let args: Vec<String> = parts
            .take(MAX_ADDITIONAL_CHECK_ARGS)
            .map(str::to_string)
            .collect();
        if program.is_empty() {
            return None;
        }
        let request = Self {
            check_id: String::new(),
            program,
            args,
        };
        Some(Self {
            check_id: format!("additional:{}", request.command_digest()),
            ..request
        })
    }
}

/// The check-runner port for additional verification checks.
pub trait AdditionalCheckRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        request: &'a AdditionalCheckRequest,
    ) -> Pin<Box<dyn Future<Output = CheckOutcomeRow> + Send + 'a>>;
}

/// The independent reviewer's verdict vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Pass,
    Fail,
    Unavailable,
}

/// The structured output of one independent review. Parsed strictly: only
/// exactly this shape is accepted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredReviewOutput {
    pub criterion_id: String,
    pub snapshot: String,
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub explanation: String,
}

/// Everything the independent reviewer receives. It carries NO mutation
/// surface: read-only repo access and evidence retrieval only.
pub struct ReviewRequest {
    pub criterion_id: String,
    pub criterion_text: String,
    pub candidate_snapshot: String,
    pub goal: String,
    pub change_set: Vec<ChangeSetEntry>,
    pub check_outcomes: Vec<CheckOutcomeRow>,
    pub repo: Option<Arc<dyn ReadOnlyRepo>>,
    pub evidence: Arc<dyn EvidenceResolver>,
}

/// The independent-review port. Implementations must be a SEPARATE
/// verification agent from the implementation child; the evaluator only
/// consumes the structured result.
pub trait IndependentReviewer: Send + Sync {
    fn review<'a>(
        &'a self,
        request: &'a ReviewRequest,
    ) -> Pin<Box<dyn Future<Output = Result<StructuredReviewOutput, String>> + Send + 'a>>;
}

/// The general evaluator contract: every criterion is evaluated through ONE
/// [`CriterionEvaluator`] implementation; the fallback implementation is the
/// independent verification agent's deterministic side.
#[allow(async_fn_in_trait)]
pub trait CriterionEvaluator {
    async fn evaluate(
        &self,
        ctx: &mut CriterionEvaluationContext,
        criterion: &Criterion,
    ) -> CriterionEvaluation;
}

/// The typed evaluation of one criterion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriterionEvaluation {
    Passed {
        evidence: Vec<String>,
    },
    Failed {
        evidence: Vec<String>,
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

impl CriterionEvaluation {
    pub fn is_passed(&self) -> bool {
        matches!(self, CriterionEvaluation::Passed { .. })
    }

    pub fn evidence_refs(&self) -> &[String] {
        match self {
            CriterionEvaluation::Passed { evidence }
            | CriterionEvaluation::Failed { evidence, .. } => evidence,
            CriterionEvaluation::Unavailable { .. } => &[],
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            CriterionEvaluation::Failed { reason, .. }
            | CriterionEvaluation::Unavailable { reason } => Some(reason),
            CriterionEvaluation::Passed { .. } => None,
        }
    }

    pub fn verdict_label(&self) -> &'static str {
        match self {
            CriterionEvaluation::Passed { .. } => "passed",
            CriterionEvaluation::Failed { .. } => "failed",
            CriterionEvaluation::Unavailable { .. } => "unavailable",
        }
    }
}

/// The criterion view the evaluator consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Criterion {
    pub id: String,
    pub text: String,
    pub binding: Option<CriterionBinding>,
}

impl Criterion {
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        binding: Option<CriterionBinding>,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            binding,
        }
    }
}

/// One subordinate criterion's already-computed evaluation (the aggregate
/// goal's input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriterionEvaluationRow {
    pub criterion_id: String,
    pub binding: Option<CriterionBinding>,
    pub evaluation: CriterionEvaluation,
}

/// Everything one criterion evaluation may read. The evaluator may append
/// proof rows for additional checks it requests (bounded).
pub struct CriterionEvaluationContext {
    pub criterion_id: String,
    pub candidate_snapshot: String,
    pub goal: String,
    pub change_set: Vec<ChangeSetEntry>,
    pub checks: Vec<CheckOutcomeRow>,
    pub integration: Option<IntegrationCoverageEvidence>,
    pub evidence: Arc<dyn EvidenceResolver>,
    pub repo: Option<Arc<dyn ReadOnlyRepo>>,
    pub reviewer: Option<Arc<dyn IndependentReviewer>>,
    pub additional_checks: Option<Arc<dyn AdditionalCheckRunner>>,
    /// The already-computed subordinate evaluations (AggregateGoal input).
    pub subordinate: Vec<CriterionEvaluationRow>,
    /// Proof rows produced by additional checks requested during evaluation.
    pub additional_rows: Vec<CheckOutcomeRow>,
}

impl CriterionEvaluationContext {
    pub fn new(
        criterion_id: impl Into<String>,
        candidate_snapshot: impl Into<String>,
        evidence: Arc<dyn EvidenceResolver>,
    ) -> Self {
        Self {
            criterion_id: criterion_id.into(),
            candidate_snapshot: candidate_snapshot.into(),
            goal: String::new(),
            change_set: Vec::new(),
            checks: Vec::new(),
            integration: None,
            evidence,
            repo: None,
            reviewer: None,
            additional_checks: None,
            subordinate: Vec::new(),
            additional_rows: Vec::new(),
        }
    }
}

/// Validate one structured reviewer output against the request it answers.
/// A pass with zero evidence is rejected; an output naming another snapshot
/// or criterion is rejected; every evidence reference must resolve to
/// immutable evidence captured for the SAME candidate snapshot.
pub fn validate_review_output(
    output: &StructuredReviewOutput,
    request: &ReviewRequest,
) -> Result<(), String> {
    if output.criterion_id != request.criterion_id {
        return Err(format!(
            "reviewer answered criterion {:?}, not {:?}",
            output.criterion_id, request.criterion_id
        ));
    }
    if output.snapshot != request.candidate_snapshot {
        return Err(format!(
            "reviewer judged snapshot {:?}, not the verified candidate {:?}",
            output.snapshot, request.candidate_snapshot
        ));
    }
    if output.explanation.trim().is_empty() {
        return Err("reviewer returned an empty explanation".into());
    }
    if output.explanation.len() > MAX_REVIEW_EXPLANATION_BYTES {
        return Err(format!(
            "reviewer explanation of {} bytes exceeds MAX_REVIEW_EXPLANATION_BYTES ({MAX_REVIEW_EXPLANATION_BYTES})",
            output.explanation.len()
        ));
    }
    if output.evidence_refs.len() > MAX_REVIEW_EVIDENCE_REFS {
        return Err(format!(
            "{} reviewer evidence refs exceed MAX_REVIEW_EVIDENCE_REFS ({MAX_REVIEW_EVIDENCE_REFS})",
            output.evidence_refs.len()
        ));
    }
    if output.verdict == ReviewVerdict::Pass && output.evidence_refs.is_empty() {
        return Err("a reviewer pass with zero evidence is rejected".into());
    }
    for reference in &output.evidence_refs {
        match request.evidence.resolve(reference) {
            Some(record) if record.snapshot == request.candidate_snapshot => {}
            Some(record) => {
                return Err(format!(
                    "reviewer evidence {reference:?} was captured for snapshot {:?}, not the verified candidate",
                    record.snapshot
                ))
            }
            None => {
                return Err(format!(
                    "reviewer evidence {reference:?} does not resolve to immutable evidence"
                ))
            }
        }
    }
    Ok(())
}

/// The fallback evaluator: the independent verification agent's deterministic
/// contract side. It never mutates anything; every verdict is derived from
/// the binding and the read-only ports.
#[derive(Debug, Clone, Copy, Default)]
pub struct FallbackEvaluator;

impl CriterionEvaluator for FallbackEvaluator {
    async fn evaluate(
        &self,
        ctx: &mut CriterionEvaluationContext,
        criterion: &Criterion,
    ) -> CriterionEvaluation {
        self.evaluate(ctx, criterion).await
    }
}

impl FallbackEvaluator {
    /// Evaluate one criterion through its binding.
    pub async fn evaluate(
        &self,
        ctx: &mut CriterionEvaluationContext,
        criterion: &Criterion,
    ) -> CriterionEvaluation {
        let Some(binding) = criterion.binding.as_ref() else {
            return CriterionEvaluation::Unavailable {
                reason: "criterion carries no binding: no objective mechanism can certify it"
                    .into(),
            };
        };
        match binding {
            CriterionBinding::RequiredCheck {
                check_id,
                command_digest,
            } => {
                self.evaluate_required_check(ctx, criterion, check_id, command_digest)
                    .await
            }
            CriterionBinding::IntegrationCoverage {
                required_work_items,
            } => self.evaluate_integration_coverage(ctx, required_work_items),
            CriterionBinding::FileState {
                path,
                expected_digest,
            } => self.evaluate_file_state(ctx, path, expected_digest),
            CriterionBinding::Evidence {
                evidence_id,
                evidence_digest,
            } => self.evaluate_evidence(ctx, evidence_id, evidence_digest),
            CriterionBinding::IndependentReview { reviewer_id } => {
                self.evaluate_review(ctx, criterion, Some(reviewer_id))
                    .await
            }
            CriterionBinding::AggregateGoal => self.evaluate_aggregate(ctx, criterion).await,
            CriterionBinding::Unavailable { reason } => CriterionEvaluation::Unavailable {
                reason: format!("criterion binding is explicitly unavailable: {reason}"),
            },
        }
    }

    async fn evaluate_required_check(
        &self,
        ctx: &mut CriterionEvaluationContext,
        criterion: &Criterion,
        check_id: &str,
        command_digest: &str,
    ) -> CriterionEvaluation {
        // Legacy 64-bit FNV digests decode for viewing but never authorize a
        // pass: a binding written before the canonical BLAKE3 authority
        // digests forces re-verification instead of resolving.
        if classify_authority_digest(command_digest) == AuthorityDigestKind::LegacyFnv {
            return CriterionEvaluation::Unavailable {
                reason: format!(
                    "required-check binding carries the legacy FNV digest {command_digest}; \
                     it can be viewed but never certified — reverify under the canonical \
                     BLAKE3 authority digests"
                ),
            };
        }
        // A testable criterion may request an ADDITIONAL verification check;
        // the check executes through the normal verifier port and becomes a
        // proof row before the binding is resolved.
        if check_id.is_empty() {
            if let (Some(runner), Some(request)) = (
                ctx.additional_checks.clone(),
                AdditionalCheckRequest::parse_criterion_text(&criterion.text),
            ) {
                let row = runner.run(&request).await;
                ctx.additional_rows.push(row);
            }
        }
        let candidates = || ctx.checks.iter().chain(ctx.additional_rows.iter());
        let matches: Vec<&CheckOutcomeRow> = if check_id.is_empty() {
            // Legacy check-derived criteria carry no id: resolve by the
            // command digest, and only when EXACTLY ONE check matches.
            candidates()
                .filter(|row| row.command_digest == command_digest)
                .collect()
        } else {
            candidates()
                .filter(|row| row.check_id == check_id && row.command_digest == command_digest)
                .collect()
        };
        match matches.len() {
            1 => {}
            0 => {
                // A digest mismatch under a known id is a drift (the
                // binding pinned a command the run no longer executes).
                if !check_id.is_empty() {
                    let by_id: Vec<&CheckOutcomeRow> = candidates()
                        .filter(|r| r.check_id == check_id)
                        .collect();
                    if by_id.len() == 1 {
                        return CriterionEvaluation::Unavailable {
                            reason: format!(
                                "required-check criterion binds check {check_id:?} with command digest {command_digest}, but the executed check carries digest {}",
                                by_id[0].command_digest
                            ),
                        };
                    }
                }
                return CriterionEvaluation::Unavailable {
                    reason: format!(
                        "required-check binding does not resolve to exactly one derived check result (id {check_id:?}, digest {command_digest})"
                    ),
                };
            }
            _ => {
                return CriterionEvaluation::Unavailable {
                    reason: "required-check binding resolves to more than one check result; ambiguous bindings never pass".into(),
                }
            }
        }
        let row = matches[0];
        let evidence = vec![format!("check:{}:{}", row.check_id, row.command_digest)];
        match row.status {
            CheckOutcomeStatus::Passed => CriterionEvaluation::Passed { evidence },
            CheckOutcomeStatus::Failed => CriterionEvaluation::Failed {
                evidence,
                reason: format!("required check {:?} failed", row.check_id),
            },
            CheckOutcomeStatus::Unavailable => CriterionEvaluation::Unavailable {
                reason: format!(
                    "required check {:?} produced no verdict (unavailable)",
                    row.check_id
                ),
            },
        }
    }

    fn evaluate_integration_coverage(
        &self,
        ctx: &CriterionEvaluationContext,
        required_work_items: &[String],
    ) -> CriterionEvaluation {
        let Some(integration) = &ctx.integration else {
            return CriterionEvaluation::Unavailable {
                reason: "no integration-coverage evidence was captured for the verified candidate"
                    .into(),
            };
        };
        if !integration.disappeared.is_empty() {
            return CriterionEvaluation::Failed {
                evidence: integration.disappeared.clone(),
                reason: format!(
                    "work item contributions disappeared: {:?}",
                    integration.disappeared
                ),
            };
        }
        let mut evidence = Vec::new();
        for item in required_work_items {
            let matches: Vec<&SourceContribution> = integration
                .contributions
                .iter()
                .filter(|c| &c.work_item == item)
                .collect();
            if matches.len() != 1 {
                return CriterionEvaluation::Failed {
                    evidence: vec![format!("work-item:{item}")],
                    reason: format!(
                        "required work item {item:?} contributed {} source/change-set entries; exactly one is required",
                        matches.len()
                    ),
                };
            }
            let contribution = matches[0];
            if contribution.source_digest.is_empty() {
                return CriterionEvaluation::Failed {
                    evidence: vec![format!("work-item:{item}")],
                    reason: format!("work item {item:?} contributed an empty source digest"),
                };
            }
            for path in &contribution.paths {
                match ctx.change_set.iter().find(|e| &e.path == path) {
                    Some(entry) if entry.status != ChangeSetStatus::Deleted => {}
                    Some(_) => {
                        return CriterionEvaluation::Failed {
                            evidence: vec![format!("work-item:{item}:{path}")],
                            reason: format!(
                                "work item {item:?} claims path {path:?}, but the verified candidate deleted it"
                            ),
                        }
                    }
                    None => {
                        return CriterionEvaluation::Failed {
                            evidence: vec![format!("work-item:{item}:{path}")],
                            reason: format!(
                                "work item {item:?} claims path {path:?}, which is absent from the verified candidate change set"
                            ),
                        }
                    }
                }
            }
            evidence.push(format!("work-item:{item}:{}", contribution.source_digest));
        }
        CriterionEvaluation::Passed { evidence }
    }

    fn evaluate_file_state(
        &self,
        ctx: &CriterionEvaluationContext,
        path: &str,
        expected_digest: &str,
    ) -> CriterionEvaluation {
        let Some(repo) = &ctx.repo else {
            return CriterionEvaluation::Unavailable {
                reason: "no read-only candidate repo access was provided".into(),
            };
        };
        let evidence = vec![format!("file:{path}")];
        match repo.hash_file(path) {
            Some(digest) if digest == expected_digest => CriterionEvaluation::Passed { evidence },
            Some(digest) => CriterionEvaluation::Failed {
                evidence,
                reason: format!(
                    "file {path:?} hashes to {digest} in the verified candidate, not the bound digest {expected_digest}"
                ),
            },
            None => CriterionEvaluation::Unavailable {
                reason: format!("file {path:?} could not be hashed in the verified candidate"),
            },
        }
    }

    fn evaluate_evidence(
        &self,
        ctx: &CriterionEvaluationContext,
        evidence_id: &str,
        evidence_digest: &str,
    ) -> CriterionEvaluation {
        let evidence = vec![format!("evidence:{evidence_id}")];
        match ctx.evidence.resolve(evidence_id) {
            None => CriterionEvaluation::Unavailable {
                reason: format!("evidence {evidence_id:?} does not resolve to immutable evidence"),
            },
            Some(record) if record.snapshot != ctx.candidate_snapshot => CriterionEvaluation::Failed {
                evidence,
                reason: format!(
                    "evidence {evidence_id:?} was captured for snapshot {:?}, not the verified candidate {:?}",
                    record.snapshot, ctx.candidate_snapshot
                ),
            },
            Some(record) if record.digest != evidence_digest => CriterionEvaluation::Failed {
                evidence,
                reason: format!(
                    "evidence {evidence_id:?} digests to {}, not the bound digest {evidence_digest}",
                    record.digest
                ),
            },
            Some(_) => CriterionEvaluation::Passed { evidence },
        }
    }

    async fn evaluate_review(
        &self,
        ctx: &mut CriterionEvaluationContext,
        criterion: &Criterion,
        reviewer_id: Option<&str>,
    ) -> CriterionEvaluation {
        let Some(reviewer) = ctx.reviewer.clone() else {
            return CriterionEvaluation::Unavailable {
                reason: "no independent reviewer is configured; the criterion cannot be certified"
                    .into(),
            };
        };
        let request = ReviewRequest {
            criterion_id: criterion.id.clone(),
            criterion_text: criterion
                .text
                .chars()
                .take(MAX_REVIEW_CRITERION_BYTES)
                .collect(),
            candidate_snapshot: ctx.candidate_snapshot.clone(),
            goal: ctx.goal.clone(),
            change_set: ctx
                .change_set
                .iter()
                .take(MAX_REVIEW_CHANGE_SET)
                .cloned()
                .collect(),
            check_outcomes: ctx
                .checks
                .iter()
                .take(MAX_REVIEW_CHECK_ROWS)
                .cloned()
                .collect(),
            repo: ctx.repo.clone(),
            evidence: ctx.evidence.clone(),
        };
        let output = match reviewer.review(&request).await {
            Ok(output) => output,
            Err(e) => {
                return CriterionEvaluation::Unavailable {
                    reason: format!("independent reviewer failed: {e}"),
                }
            }
        };
        if let Some(expected) = reviewer_id {
            // The binding names WHICH reviewer must have produced the record;
            // the structured output has no id field, so the caller's port IS
            // that reviewer. An empty id is a corrupt binding.
            if expected.is_empty() {
                return CriterionEvaluation::Unavailable {
                    reason: "independent-review binding carries an empty reviewer id".into(),
                };
            }
        }
        if let Err(e) = validate_review_output(&output, &request) {
            return CriterionEvaluation::Unavailable {
                reason: format!("reviewer output refused: {e}"),
            };
        }
        let evidence = output.evidence_refs.clone();
        match output.verdict {
            ReviewVerdict::Pass => CriterionEvaluation::Passed { evidence },
            ReviewVerdict::Fail => CriterionEvaluation::Failed {
                evidence,
                reason: output.explanation,
            },
            ReviewVerdict::Unavailable => CriterionEvaluation::Unavailable {
                reason: output.explanation,
            },
        }
    }

    async fn evaluate_aggregate(
        &self,
        ctx: &mut CriterionEvaluationContext,
        criterion: &Criterion,
    ) -> CriterionEvaluation {
        if ctx.subordinate.is_empty() {
            return CriterionEvaluation::Unavailable {
                reason: "aggregate-goal criterion has no subordinate criteria to aggregate".into(),
            };
        }
        let mut evidence: Vec<String> = Vec::new();
        for row in &ctx.subordinate {
            match &row.evaluation {
                CriterionEvaluation::Failed { reason, evidence: e } => {
                    evidence.extend(e.iter().cloned());
                    return CriterionEvaluation::Failed {
                        evidence,
                        reason: format!(
                            "subordinate criterion {} failed: {reason}",
                            row.criterion_id
                        ),
                    };
                }
                CriterionEvaluation::Unavailable { reason } => {
                    return CriterionEvaluation::Unavailable {
                        reason: format!(
                            "subordinate criterion {} is unavailable ({reason}); an aggregate goal cannot pass over it",
                            row.criterion_id
                        ),
                    }
                }
                CriterionEvaluation::Passed { evidence: e } => evidence.extend(e.iter().cloned()),
            }
        }
        // Every subordinate passed: the independent FINAL reviewer decides.
        // The aggregate review never substitutes for it, and the reviewer
        // record must be tied to the aggregate criterion id + candidate.
        match self.evaluate_review(ctx, criterion, None).await {
            CriterionEvaluation::Passed { evidence: reviewer } => {
                evidence.extend(reviewer);
                CriterionEvaluation::Passed { evidence }
            }
            CriterionEvaluation::Failed {
                evidence: reviewer_evidence,
                reason,
            } => {
                evidence.extend(reviewer_evidence);
                CriterionEvaluation::Failed { evidence, reason }
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::state::command_binding_digest;

    #[derive(Clone)]
    struct StaticReviewer(Result<StructuredReviewOutput, String>);

    impl IndependentReviewer for StaticReviewer {
        fn review<'a>(
            &'a self,
            _request: &'a ReviewRequest,
        ) -> Pin<Box<dyn Future<Output = Result<StructuredReviewOutput, String>> + Send + 'a>>
        {
            let output = self.0.clone();
            Box::pin(async move { output })
        }
    }

    /// A well-behaved reviewer that echoes the request's criterion id and
    /// candidate snapshot (the honest structured-record shape).
    #[derive(Clone)]
    struct EchoReviewer {
        verdict: ReviewVerdict,
        refs: Vec<String>,
    }

    impl IndependentReviewer for EchoReviewer {
        fn review<'a>(
            &'a self,
            request: &'a ReviewRequest,
        ) -> Pin<Box<dyn Future<Output = Result<StructuredReviewOutput, String>> + Send + 'a>>
        {
            let output = StructuredReviewOutput {
                criterion_id: request.criterion_id.clone(),
                snapshot: request.candidate_snapshot.clone(),
                verdict: self.verdict,
                evidence_refs: self.refs.clone(),
                explanation: "reviewed the candidate".into(),
            };
            Box::pin(async move { Ok(output) })
        }
    }

    fn evidence_resolver() -> Arc<dyn EvidenceResolver> {
        Arc::new(|id: &str| {
            Some(EvidenceRecord {
                evidence_id: id.to_string(),
                digest: "d-1".into(),
                snapshot: "snap-1".into(),
            })
        })
    }

    fn check(id: &str, command: &str, status: CheckOutcomeStatus) -> CheckOutcomeRow {
        CheckOutcomeRow {
            check_id: id.into(),
            command_digest: command_binding_digest(command),
            status,
            evidence: Some("exit 0".into()),
        }
    }

    fn base_ctx() -> CriterionEvaluationContext {
        CriterionEvaluationContext::new("crit-1", "snap-1", evidence_resolver())
    }

    fn review_output(verdict: ReviewVerdict, refs: Vec<String>) -> StructuredReviewOutput {
        StructuredReviewOutput {
            criterion_id: "crit-1".into(),
            snapshot: "snap-1".into(),
            verdict,
            evidence_refs: refs,
            explanation: "reviewed the candidate".into(),
        }
    }

    #[test]
    fn additional_check_identity_is_structured_and_boundary_safe() {
        let request = AdditionalCheckRequest {
            check_id: "c".into(),
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
        };
        // The structured digest equals the digest of the canonical text the
        // text-only producers carry (one exact rendering, one identity).
        assert_eq!(
            request.command_digest(),
            command_binding_digest(&request.command_text())
        );
        // Whitespace-joined ambiguity can never alias: ("a b", "c") and
        // ("a", "b c") render different canonical texts AND digest as
        // different structured identities.
        let left = AdditionalCheckRequest {
            check_id: String::new(),
            program: "a b".into(),
            args: vec!["c".into()],
        };
        let right = AdditionalCheckRequest {
            check_id: String::new(),
            program: "a".into(),
            args: vec!["b c".into()],
        };
        // The whitespace rendering collapses both to the same prose; the
        // structured identity must still separate them.
        assert_eq!(left.command_text(), right.command_text());
        assert_ne!(left.command_digest(), right.command_digest());
        // The digest is a labelled BLAKE3 authority digest, never FNV.
        assert!(request.command_digest().starts_with("blake3:"));
    }

    #[tokio::test]
    async fn additional_check_identity_refusal_does_not_accept_a_fnv_legacy_digest() {
        // A check row whose digest is the legacy FNV shape can never resolve
        // a structured binding: the digest mismatch forces re-verification.
        let mut ctx = base_ctx();
        let request = AdditionalCheckRequest {
            check_id: "additional".into(),
            program: "cargo".into(),
            args: vec!["clippy".into()],
        };
        let legacy = "fnv1a64:0123456789abcdef".to_string();
        ctx.checks = vec![CheckOutcomeRow {
            check_id: request.check_id.clone(),
            command_digest: legacy.clone(),
            status: CheckOutcomeStatus::Passed,
            evidence: Some("exit 0".into()),
        }];
        let criterion = Criterion::new(
            "crit-1",
            "verify: cargo clippy",
            Some(CriterionBinding::RequiredCheck {
                check_id: request.check_id.clone(),
                command_digest: legacy,
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(
            !eval.is_passed(),
            "a legacy FNV digest must never certify a criterion: {eval:?}"
        );
    }

    #[tokio::test]
    async fn green_global_suite_does_not_pass_unbound_user_criterion() {
        // The entire suite passed: it is irrelevant to a criterion whose
        // binding is not its own.
        let mut ctx = base_ctx();
        ctx.checks = vec![
            check(
                "cargo_test",
                "cargo test --workspace",
                CheckOutcomeStatus::Passed,
            ),
            check(
                "cargo_check",
                "cargo check --workspace",
                CheckOutcomeStatus::Passed,
            ),
        ];
        let unbound = Criterion::new("crit-1", "the user's prose acceptance", None);
        let eval = FallbackEvaluator.evaluate(&mut ctx, &unbound).await;
        assert_eq!(eval.verdict_label(), "unavailable");
        assert!(!eval.is_passed());

        // Even an explicit Unavailable binding never passes on a green suite.
        let explicit = Criterion::new(
            "crit-1",
            "the user's prose acceptance",
            Some(CriterionBinding::Unavailable {
                reason: "no objective mechanism".into(),
            }),
        );
        assert!(!FallbackEvaluator
            .evaluate(&mut ctx, &explicit)
            .await
            .is_passed());
    }

    #[tokio::test]
    async fn required_check_criterion_binds_only_to_its_check() {
        let mut ctx = base_ctx();
        ctx.checks = vec![
            check("rust_check", "cargo check", CheckOutcomeStatus::Passed),
            check("rust_test", "cargo test", CheckOutcomeStatus::Passed),
        ];
        let bound = Criterion::new(
            "crit-1",
            "required check: cargo check",
            Some(CriterionBinding::RequiredCheck {
                check_id: "rust_check".into(),
                command_digest: command_binding_digest("cargo check"),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &bound).await;
        assert!(eval.is_passed(), "{eval:?}");
        // The SAME binding with the test check's digest must not resolve.
        let confused = Criterion::new(
            "crit-1",
            "required check: cargo test",
            Some(CriterionBinding::RequiredCheck {
                check_id: "rust_test".into(),
                command_digest: command_binding_digest("cargo check"),
            }),
        );
        assert!(!FallbackEvaluator
            .evaluate(&mut ctx, &confused)
            .await
            .is_passed());
        // A binding that aliases another criterion's check id is refused:
        // exactly one result must match id AND digest.
        let aliased = Criterion::new(
            "crit-1",
            "required check: cargo check",
            Some(CriterionBinding::RequiredCheck {
                check_id: "rust_check".into(),
                command_digest: command_binding_digest("cargo test"),
            }),
        );
        assert!(!FallbackEvaluator
            .evaluate(&mut ctx, &aliased)
            .await
            .is_passed());
    }

    #[tokio::test]
    async fn failed_check_cannot_produce_pass() {
        let mut ctx = base_ctx();
        ctx.checks = vec![check(
            "rust_check",
            "cargo check",
            CheckOutcomeStatus::Failed,
        )];
        let criterion = Criterion::new(
            "crit-1",
            "required check: cargo check",
            Some(CriterionBinding::RequiredCheck {
                check_id: "rust_check".into(),
                command_digest: command_binding_digest("cargo check"),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(
            matches!(eval, CriterionEvaluation::Failed { .. }),
            "{eval:?}"
        );
    }

    #[tokio::test]
    async fn unavailable_check_cannot_produce_pass() {
        let mut ctx = base_ctx();
        ctx.checks = vec![check(
            "rust_check",
            "cargo check",
            CheckOutcomeStatus::Unavailable,
        )];
        let criterion = Criterion::new(
            "crit-1",
            "required check: cargo check",
            Some(CriterionBinding::RequiredCheck {
                check_id: "rust_check".into(),
                command_digest: command_binding_digest("cargo check"),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(
            matches!(eval, CriterionEvaluation::Unavailable { .. }),
            "{eval:?}"
        );
    }

    #[tokio::test]
    async fn evidence_digest_mismatch_refuses_criterion() {
        let ctx = base_ctx();
        let criterion = Criterion::new(
            "crit-1",
            "certified by evidence",
            Some(CriterionBinding::Evidence {
                evidence_id: "e-1".into(),
                evidence_digest: "the-wrong-digest".into(),
            }),
        );
        let mut ctx = ctx;
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(
            matches!(eval, CriterionEvaluation::Failed { .. }),
            "{eval:?}"
        );
    }

    #[tokio::test]
    async fn evidence_for_old_snapshot_refuses_criterion() {
        let mut ctx = base_ctx();
        ctx.candidate_snapshot = "snap-2".into();
        let criterion = Criterion::new(
            "crit-1",
            "certified by evidence",
            Some(CriterionBinding::Evidence {
                evidence_id: "e-1".into(),
                evidence_digest: "d-1".into(),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(!eval.is_passed(), "{eval:?}");
        assert!(
            matches!(eval, CriterionEvaluation::Failed { .. }),
            "{eval:?}"
        );
    }

    #[tokio::test]
    async fn reviewer_pass_without_evidence_is_refused() {
        let mut ctx = base_ctx();
        ctx.reviewer = Some(Arc::new(StaticReviewer(Ok(review_output(
            ReviewVerdict::Pass,
            Vec::new(),
        )))));
        let criterion = Criterion::new(
            "crit-1",
            "reviewed criterion",
            Some(CriterionBinding::IndependentReview {
                reviewer_id: "reviewer-1".into(),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(!eval.is_passed(), "{eval:?}");
        assert!(
            matches!(eval, CriterionEvaluation::Unavailable { .. }),
            "{eval:?}"
        );
    }

    #[tokio::test]
    async fn reviewer_for_old_candidate_is_refused() {
        let mut ctx = base_ctx();
        let mut output = review_output(ReviewVerdict::Pass, vec!["e-1".into()]);
        output.snapshot = "snap-0".into();
        ctx.reviewer = Some(Arc::new(StaticReviewer(Ok(output))));
        let criterion = Criterion::new(
            "crit-1",
            "reviewed criterion",
            Some(CriterionBinding::IndependentReview {
                reviewer_id: "reviewer-1".into(),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(!eval.is_passed(), "{eval:?}");
    }

    /// An additional-check runner that echoes the request's check id and
    /// returns the fixed status (the normal verifier's shape).
    #[derive(Clone)]
    struct FixedRunner(CheckOutcomeStatus);

    impl AdditionalCheckRunner for FixedRunner {
        fn run<'a>(
            &'a self,
            request: &'a AdditionalCheckRequest,
        ) -> Pin<Box<dyn Future<Output = CheckOutcomeRow> + Send + 'a>> {
            let status = self.0;
            Box::pin(async move {
                CheckOutcomeRow {
                    check_id: request.check_id.clone(),
                    command_digest: request.command_digest(),
                    status,
                    evidence: Some("additional check".into()),
                }
            })
        }
    }

    #[tokio::test]
    async fn additional_check_requests_execute_and_become_proof_rows() {
        let mut ctx = base_ctx();
        ctx.checks = vec![check(
            "rust_check",
            "cargo check",
            CheckOutcomeStatus::Passed,
        )];
        ctx.additional_checks = Some(Arc::new(FixedRunner(CheckOutcomeStatus::Passed)));
        // A testable criterion with a digest-only binding asks for a NEW
        // verification check through the normal verifier port.
        let criterion = Criterion::new(
            "crit-1",
            "verify: cargo clippy",
            Some(CriterionBinding::RequiredCheck {
                check_id: String::new(),
                command_digest: command_binding_digest("cargo clippy"),
            }),
        );
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(eval.is_passed(), "{eval:?}");
        assert_eq!(ctx.additional_rows.len(), 1, "the check became a proof row");
        assert!(
            ctx.additional_rows[0].check_id.starts_with("additional:"),
            "{:?}",
            ctx.additional_rows[0]
        );
    }

    #[tokio::test]
    async fn file_state_binding_hashes_the_verified_candidate() {
        let mut ctx = base_ctx();
        ctx.repo = Some(Arc::new(|path: &str| {
            (path == "src/lib.rs").then(|| "candidate-digest".to_string())
        }));
        let pass = Criterion::new(
            "crit-1",
            "src/lib.rs frozen",
            Some(CriterionBinding::FileState {
                path: "src/lib.rs".into(),
                expected_digest: "candidate-digest".into(),
            }),
        );
        assert!(FallbackEvaluator
            .evaluate(&mut ctx, &pass)
            .await
            .is_passed());
        let stale = Criterion::new(
            "crit-1",
            "src/lib.rs frozen",
            Some(CriterionBinding::FileState {
                path: "src/lib.rs".into(),
                expected_digest: "stale-digest".into(),
            }),
        );
        assert!(!FallbackEvaluator
            .evaluate(&mut ctx, &stale)
            .await
            .is_passed());
        let missing = Criterion::new(
            "crit-1",
            "missing.rs frozen",
            Some(CriterionBinding::FileState {
                path: "missing.rs".into(),
                expected_digest: "x".into(),
            }),
        );
        assert!(matches!(
            FallbackEvaluator.evaluate(&mut ctx, &missing).await,
            CriterionEvaluation::Unavailable { .. }
        ));
    }

    #[tokio::test]
    async fn integration_coverage_requires_every_work_item_and_no_disappearance() {
        let mut ctx = base_ctx();
        ctx.change_set = vec![ChangeSetEntry {
            path: "src/a.rs".into(),
            digest_hex: "d-a".into(),
            status: ChangeSetStatus::Modified,
        }];
        ctx.integration = Some(IntegrationCoverageEvidence {
            run_id: "run-1".into(),
            contributions: vec![SourceContribution {
                work_item: "impl-a".into(),
                source_digest: "s-a".into(),
                paths: vec!["src/a.rs".into()],
            }],
            disappeared: vec![],
        });
        let binding = CriterionBinding::IntegrationCoverage {
            required_work_items: vec!["impl-a".into()],
        };
        let criterion = Criterion::new("crit-1", "all items land", Some(binding.clone()));
        assert!(FallbackEvaluator
            .evaluate(&mut ctx, &criterion)
            .await
            .is_passed());
        // A missing contribution for a required item is a failure.
        let missing = Criterion::new(
            "crit-1",
            "all items land",
            Some(CriterionBinding::IntegrationCoverage {
                required_work_items: vec!["impl-b".into()],
            }),
        );
        assert!(matches!(
            FallbackEvaluator.evaluate(&mut ctx, &missing).await,
            CriterionEvaluation::Failed { .. }
        ));
        // A disappeared contribution never passes either.
        if let Some(integration) = ctx.integration.as_mut() {
            integration.disappeared.push("impl-c".into());
        }
        assert!(!FallbackEvaluator
            .evaluate(&mut ctx, &criterion)
            .await
            .is_passed());
    }

    #[tokio::test]
    async fn aggregate_goal_requires_all_subordinates_and_final_review() {
        let mut ctx = base_ctx();
        ctx.reviewer = Some(Arc::new(EchoReviewer {
            verdict: ReviewVerdict::Pass,
            refs: vec!["e-1".into()],
        }));
        let criterion = Criterion::new(
            "crit-goal",
            "goal: ship it",
            Some(CriterionBinding::AggregateGoal),
        );
        ctx.subordinate = vec![
            CriterionEvaluationRow {
                criterion_id: "c-1".into(),
                binding: None,
                evaluation: CriterionEvaluation::Passed {
                    evidence: vec!["check:rust_check".into()],
                },
            },
            CriterionEvaluationRow {
                criterion_id: "c-2".into(),
                binding: None,
                evaluation: CriterionEvaluation::Passed {
                    evidence: vec!["check:rust_test".into()],
                },
            },
        ];
        assert!(FallbackEvaluator
            .evaluate(&mut ctx, &criterion)
            .await
            .is_passed());
        // ONE failed subordinate blocks the aggregate even when every other
        // criterion and the reviewer pass.
        ctx.subordinate[0].evaluation = CriterionEvaluation::Failed {
            evidence: vec![],
            reason: "check failed".into(),
        };
        let eval = FallbackEvaluator.evaluate(&mut ctx, &criterion).await;
        assert!(
            matches!(eval, CriterionEvaluation::Failed { .. }),
            "{eval:?}"
        );
    }
}
