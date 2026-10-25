// TEST-SEPARATION: oracle=contract isolation=pure-unit
//! Fresh-context design readiness: a pure, zero-call audit of the compiled
//! engineering contract.
//!
//! The audit sees ONLY the compiled contract. It never reads the
//! implementation conversation, never calls a model or tool, never creates a
//! design capsule/reviewer transcript/artifact and never mutates the contract
//! it inspects. Its findings are bounded RISK HYPOTHESES for structural gaps:
//!
//! - an elevated (High/Unknown) risk with no claim-bound proof obligation,
//! - a required proof obligation whose evidence binding is the legacy
//!   category-wide fallback (nothing specific can discharge it),
//! - concurrency/replay, migration or compatibility concerns carried by the
//!   contract without a matching claim-bound proof obligation.
//!
//! Applying an audit may only append bounded risk rows. It never creates or
//! discharges a proof obligation, never certifies completion and never raises
//! the contract's verification depth on its own: a newly added row is Medium
//! unless the pre-audit contract already carried an elevated risk, in which
//! case an elevated finding may preserve its tier. A calm contract can
//! therefore never be newly promoted to High/Unknown by this layer.
//!
//! The focus projection is derived from proof bindings, risk descriptions and
//! invariants in memory. It is bounded, unique, sorted, sanitized and may only
//! REPLACE duplicated retrieval-key material behind the tokenizer-free
//! economic gate: at least 12.5% smaller AND at least 2 KiB saved.

use std::collections::BTreeSet;

use crate::contract::{
    ContractOrigin, EngineeringContract, EvidenceBinding, ProofObligation, Requirement,
    RiskHypothesis, RiskSeverity,
};
use crate::contract_compile::CompiledContract;

/// Hard bound on readiness findings accepted by one audit/apply.
pub const MAX_DESIGN_READINESS_FINDINGS: usize = 8;
/// Hard bound on the focus projection's terms.
pub const MAX_DESIGN_FOCUS_TERMS: usize = 16;
/// Hard bound on ONE focus term (UTF-8 bytes).
pub const MAX_DESIGN_FOCUS_TERM_BYTES: usize = 48;
/// Minimum byte saving required before the focus projection may replace the
/// baseline retrieval keys (2 KiB).
pub const MIN_DESIGN_FOCUS_SAVINGS_BYTES: usize = 2 * 1024;

/// Hard bound on one finding's statement (UTF-8 bytes).
const MAX_DESIGN_FINDING_BYTES: usize = 320;
/// Hard bound on one finding's basis (UTF-8 bytes).
const MAX_DESIGN_BASIS_BYTES: usize = 320;
/// Hard bound on one applied risk id (UTF-8 bytes): a full `dr1:` digest.
const MAX_DESIGN_FINDING_ID_BYTES: usize = 96;
/// Hard bound on the bytes inspected from one contract string: hostile
/// content cannot make the audit scan without a bound.
const MAX_FOCUS_SOURCE_BYTES: usize = 4 * 1024;
/// Hard bound on one binding's targets inspected by the audit.
const MAX_FOCUS_BINDING_TARGETS: usize = 64;
/// Hard bound on contract entries inspected from each vector.
const MAX_DESIGN_SOURCES: usize = 64;
/// Candidate terms retained before the final sorted truncation.
const MAX_FOCUS_CANDIDATES: usize = MAX_DESIGN_FOCUS_TERMS * 8;
/// Minimum meaningful focus term (bytes), matching intent tokenization.
const MIN_DESIGN_FOCUS_TERM_BYTES: usize = 3;

/// Tokens that ground a concurrency/replay concern.
const CONCURRENCY_TOKENS: &[&str] = &[
    "atomic",
    "atomicity",
    "concurrency",
    "concurrent",
    "deadlock",
    "idempotent",
    "idempotency",
    "race",
    "races",
    "reentrancy",
    "reentrant",
    "replay",
    "replayed",
    "toctou",
];

/// Tokens that ground a migration concern.
const MIGRATION_TOKENS: &[&str] = &[
    "backfill",
    "migrate",
    "migration",
    "migrations",
    "schema",
    "upgrade",
];

/// Tokens that ground a compatibility concern.
const COMPATIBILITY_TOKENS: &[&str] = &[
    "api",
    "backward",
    "breaking",
    "compat",
    "compatibility",
    "compatible",
    "deprecat",
    "deprecated",
    "public",
    "rename",
    "semver",
];

/// Generic words dropped from the focus projection so it carries retrieval
/// keys rather than prose glue.
const FOCUS_STOPWORDS: &[&str] = &[
    "about", "after", "again", "against", "all", "also", "and", "any", "are", "before", "being",
    "but", "can", "could", "does", "each", "for", "from", "has", "have", "into", "its", "may",
    "more", "must", "not", "only", "other", "over", "same", "should", "still", "such", "than",
    "that", "the", "their", "then", "there", "these", "they", "this", "those", "through", "under",
    "until", "use", "using", "was", "were", "what", "when", "where", "which", "while", "will",
    "with", "without", "would",
];

/// The structural class of one readiness finding. Every variant is a risk
/// hypothesis, never a proof obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DesignReadinessKind {
    /// An elevated (High/Unknown) risk exists but no claim-bound proof
    /// obligation can ever verify it.
    ElevatedRiskWithoutProof,
    /// A `Required` obligation still carries the legacy category-wide
    /// binding: no specific evidence can discharge it.
    UnboundRequiredProof,
    /// A concurrency/replay concern has no matching claim-bound proof.
    ConcurrencyWithoutProof,
    /// A migration concern has no matching claim-bound proof.
    MigrationWithoutProof,
    /// A compatibility concern has no matching claim-bound proof.
    CompatibilityWithoutProof,
}

impl DesignReadinessKind {
    /// Stable diagnostic name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ElevatedRiskWithoutProof => "elevated_risk_without_proof",
            Self::UnboundRequiredProof => "unbound_required_proof",
            Self::ConcurrencyWithoutProof => "concurrency_without_proof",
            Self::MigrationWithoutProof => "migration_without_proof",
            Self::CompatibilityWithoutProof => "compatibility_without_proof",
        }
    }

    /// The concern label these tokens ground (empty for non-concern kinds).
    const fn concern_label(self) -> &'static str {
        match self {
            Self::ConcurrencyWithoutProof => "concurrency/replay",
            Self::MigrationWithoutProof => "migration",
            Self::CompatibilityWithoutProof => "compatibility",
            Self::ElevatedRiskWithoutProof | Self::UnboundRequiredProof => "",
        }
    }

    /// The concern tokens this kind is grounded on.
    const fn concern_tokens(self) -> &'static [&'static str] {
        match self {
            Self::ConcurrencyWithoutProof => CONCURRENCY_TOKENS,
            Self::MigrationWithoutProof => MIGRATION_TOKENS,
            Self::CompatibilityWithoutProof => COMPATIBILITY_TOKENS,
            Self::ElevatedRiskWithoutProof | Self::UnboundRequiredProof => &[],
        }
    }
}

/// One bounded readiness finding: a risk hypothesis with deterministic
/// identity, the structural gap it names and the severity it would carry when
/// the pre-audit contract already justified an elevated risk tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignReadinessFinding {
    /// Deterministic `dr1:`-prefixed identity (never an obligation id).
    pub id: String,
    pub kind: DesignReadinessKind,
    /// Bounded prose statement (the risk row's description when applied).
    pub statement: String,
    /// Bounded deterministic basis for the finding.
    pub basis: String,
    /// Proposed severity. [`apply_design_readiness`] clamps it to Medium for
    /// a calm pre-audit contract.
    pub severity: RiskSeverity,
}

/// The complete pure audit: bounded findings plus the in-memory focus
/// projection derived from the same contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesignReadinessAudit {
    pub findings: Vec<DesignReadinessFinding>,
    pub focus_terms: Vec<String>,
}

impl DesignReadinessAudit {
    /// TRUE when the audit found no structural gap.
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// What [`audit_compiled_design`] may inspect. Both the durable contract and
/// the full compile report (whose durable authority is its `contract`) satisfy
/// the seam; nothing outside the compiled contract is read.
pub trait DesignReadinessSource {
    fn design_contract(&self) -> &EngineeringContract;
}

impl DesignReadinessSource for EngineeringContract {
    fn design_contract(&self) -> &EngineeringContract {
        self
    }
}

impl DesignReadinessSource for CompiledContract {
    fn design_contract(&self) -> &EngineeringContract {
        &self.contract
    }
}

/// What [`apply_design_readiness`] may mutate: only the durable engineering
/// contract. The full compile report satisfies the seam through its
/// `contract` field; no other compile fact is touched.
pub trait DesignReadinessTarget {
    fn design_contract_mut(&mut self) -> &mut EngineeringContract;
}

impl DesignReadinessTarget for EngineeringContract {
    fn design_contract_mut(&mut self) -> &mut EngineeringContract {
        self
    }
}

impl DesignReadinessTarget for CompiledContract {
    fn design_contract_mut(&mut self) -> &mut EngineeringContract {
        &mut self.contract
    }
}

/// Audit the compiled design with zero model/tool calls. Pure, deterministic
/// and total: hostile content yields bounded findings/terms, never a panic.
pub fn audit_compiled_design(source: &impl DesignReadinessSource) -> DesignReadinessAudit {
    let contract = source.design_contract();
    let mut findings = Vec::new();

    // 1. Elevated risk without ANY claim-bound proof obligation. An obligation
    //    is claim-bound only when its binding names concrete evidence targets;
    //    the legacy Category fallback and empty target lists are not.
    if has_elevated_risk(contract) && !has_any_claim_bound_proof(contract) {
        let severity = highest_risk_severity(contract);
        let statement = format!(
            "{} risk is carried without a claim-bound proof obligation",
            severity.severity_str()
        );
        let basis = format!(
            "{} risk hypothesis(es) escalate; 0 of {} proof obligation(s) carry a non-empty claim binding",
            contract
                .risks
                .iter()
                .take(MAX_DESIGN_SOURCES)
                .filter(|risk| risk.severity.escalates())
                .count(),
            contract
                .proof_obligations
                .iter()
                .take(MAX_DESIGN_SOURCES)
                .count()
        );
        push_finding(
            &mut findings,
            DesignReadinessKind::ElevatedRiskWithoutProof,
            statement,
            basis,
            severity,
        );
    }

    // 2. Required obligations that nothing specific can discharge. Reporting a
    //    gap must never create or discharge the obligation itself.
    for obligation in contract.proof_obligations.iter().take(MAX_DESIGN_SOURCES) {
        if findings.len() >= MAX_DESIGN_READINESS_FINDINGS {
            break;
        }
        if obligation.requirement != Requirement::Required || is_claim_bound(obligation) {
            continue;
        }
        let statement = format!(
            "required proof obligation {} is not claim-bound: {}",
            obligation.id, obligation.claim
        );
        let basis = format!(
            "obligation {} requests {} evidence but its binding class is {}; no specific evidence can discharge it",
            obligation.id,
            obligation.evidence.as_str(),
            obligation.evidence_binding.class()
        );
        push_finding(
            &mut findings,
            DesignReadinessKind::UnboundRequiredProof,
            statement,
            basis,
            RiskSeverity::Medium,
        );
    }

    // 3. Concurrency, migration and compatibility concerns without matching
    //    claim-bound coverage. One bounded finding per concern class.
    for kind in [
        DesignReadinessKind::ConcurrencyWithoutProof,
        DesignReadinessKind::MigrationWithoutProof,
        DesignReadinessKind::CompatibilityWithoutProof,
    ] {
        if findings.len() >= MAX_DESIGN_READINESS_FINDINGS {
            break;
        }
        let Some(concern) = first_concern(contract, kind) else {
            continue;
        };
        if has_concern_coverage(contract, kind) {
            continue;
        }
        let statement = format!(
            "{} concern is not covered by a matching claim-bound proof obligation: {}",
            kind.concern_label(),
            concern
        );
        let basis = format!(
            "a contract entry names {} tokens, but no proof claim or binding target covers them",
            kind.concern_label()
        );
        push_finding(&mut findings, kind, statement, basis, RiskSeverity::Medium);
    }

    findings.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.statement.cmp(&b.statement))
            .then_with(|| a.id.cmp(&b.id))
    });
    findings.dedup_by(|a, b| a.kind == b.kind && a.statement == b.statement);
    findings.truncate(MAX_DESIGN_READINESS_FINDINGS);

    DesignReadinessAudit {
        findings,
        focus_terms: design_focus_terms(contract),
    }
}

/// Apply the audit to the durable contract.
///
/// This appends at most [`MAX_DESIGN_READINESS_FINDINGS`] risk hypotheses and
/// nothing else: proof obligations, constraints, invariants and existing risks
/// are never rewritten or removed. New rows are Medium for a calm contract;
/// when the pre-audit contract already carried an elevated risk, an elevated
/// finding may preserve that tier. Application is idempotent per finding
/// identity/description.
pub fn apply_design_readiness(
    target: &mut impl DesignReadinessTarget,
    audit: &DesignReadinessAudit,
) -> usize {
    let contract = target.design_contract_mut();
    let pre_audit_elevated = has_elevated_risk(contract);
    let mut applied = 0usize;
    let mut considered = 0usize;
    let mut seen_in_audit: BTreeSet<(DesignReadinessKind, String)> = BTreeSet::new();
    for finding in audit.findings.iter().take(MAX_DESIGN_SOURCES) {
        if considered >= MAX_DESIGN_READINESS_FINDINGS {
            break;
        }
        let description = bounded_text(&finding.statement, MAX_DESIGN_FINDING_BYTES);
        if description.is_empty() {
            continue;
        }
        if !seen_in_audit.insert((finding.kind, description.clone())) {
            continue;
        }
        considered += 1;
        let id = if finding.id.is_empty() {
            finding_id(finding.kind, &description)
        } else {
            bounded_text(&finding.id, MAX_DESIGN_FINDING_ID_BYTES)
        };
        if id.is_empty()
            || contract
                .risks
                .iter()
                .take(MAX_DESIGN_SOURCES)
                .any(|risk| risk.id == id || risk.description == description)
        {
            continue;
        }
        // The promotion clamp: a calm pre-audit contract can only receive
        // Medium rows. An already-elevated contract preserves an elevated
        // finding's tier; nothing here can raise a calm contract.
        let proposed = if pre_audit_elevated {
            finding.severity
        } else {
            RiskSeverity::Medium
        };
        let severity = match proposed {
            RiskSeverity::High => RiskSeverity::High,
            RiskSeverity::Unknown => RiskSeverity::Unknown,
            _ => RiskSeverity::Medium,
        };
        contract.risks.push(RiskHypothesis {
            id,
            description,
            severity,
            origin: ContractOrigin::Heuristic,
        });
        applied += 1;
    }
    applied
}

/// The bounded in-memory focus projection: sanitized intent-like terms from
/// the contract's proof bindings, risks and invariants. Unique and sorted.
pub fn design_focus_terms(contract: &EngineeringContract) -> Vec<String> {
    let mut terms: BTreeSet<String> = BTreeSet::new();

    for obligation in contract.proof_obligations.iter().take(MAX_DESIGN_SOURCES) {
        if terms.len() >= MAX_FOCUS_CANDIDATES {
            break;
        }
        for_each_binding_target(&obligation.evidence_binding, |target| {
            collect_source_terms(target, &mut terms);
        });
    }
    for risk in contract.risks.iter().take(MAX_DESIGN_SOURCES) {
        if terms.len() >= MAX_FOCUS_CANDIDATES {
            break;
        }
        collect_source_terms(&risk.description, &mut terms);
    }
    for invariant in contract.invariants.iter().take(MAX_DESIGN_SOURCES) {
        if terms.len() >= MAX_FOCUS_CANDIDATES {
            break;
        }
        collect_source_terms(&invariant.statement, &mut terms);
    }

    terms.into_iter().take(MAX_DESIGN_FOCUS_TERMS).collect()
}

/// The tokenizer-free economic gate for replacing duplicated retrieval-key
/// material. TRUE only when the focus projection is non-empty, at least 12.5%
/// smaller than the baseline AND saves at least [`MIN_DESIGN_FOCUS_SAVINGS_BYTES`].
pub fn design_focus_is_economic(baseline_bytes: usize, focus_terms: &[String]) -> bool {
    if focus_terms.is_empty() || baseline_bytes == 0 {
        return false;
    }
    if focus_terms.iter().any(String::is_empty) {
        return false;
    }
    let focus_bytes = focus_terms
        .iter()
        .fold(0usize, |total, term| total.saturating_add(term.len()));
    if focus_bytes >= baseline_bytes {
        return false;
    }
    let saved = baseline_bytes - focus_bytes;
    if saved < MIN_DESIGN_FOCUS_SAVINGS_BYTES {
        return false;
    }
    // saved >= 12.5% of baseline, in integer arithmetic and without overflow.
    (saved as u128) * 8 >= baseline_bytes as u128
}

// --------------------------------------------------------------- internals

fn push_finding(
    findings: &mut Vec<DesignReadinessFinding>,
    kind: DesignReadinessKind,
    statement: String,
    basis: String,
    severity: RiskSeverity,
) {
    if findings.len() >= MAX_DESIGN_READINESS_FINDINGS {
        return;
    }
    let statement = bounded_text(&statement, MAX_DESIGN_FINDING_BYTES);
    if statement.is_empty() {
        return;
    }
    let basis = bounded_text(&basis, MAX_DESIGN_BASIS_BYTES);
    findings.push(DesignReadinessFinding {
        id: finding_id(kind, &statement),
        kind,
        statement,
        basis,
        severity,
    });
}

fn finding_id(kind: DesignReadinessKind, statement: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor.design.readiness.dr1.v1\0");
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(statement.as_bytes());
    format!("dr1:{}", hasher.finalize().to_hex())
}

fn has_elevated_risk(contract: &EngineeringContract) -> bool {
    contract
        .risks
        .iter()
        .take(MAX_DESIGN_SOURCES)
        .any(|risk| risk.severity.escalates())
}

fn highest_risk_severity(contract: &EngineeringContract) -> RiskSeverity {
    let mut highest = RiskSeverity::Low;
    for risk in contract.risks.iter().take(MAX_DESIGN_SOURCES) {
        match risk.severity {
            RiskSeverity::Unknown => return RiskSeverity::Unknown,
            RiskSeverity::High => highest = RiskSeverity::High,
            RiskSeverity::Medium if highest == RiskSeverity::Low => {
                highest = RiskSeverity::Medium;
            }
            RiskSeverity::Low | RiskSeverity::Medium => {}
        }
    }
    highest
}

fn has_any_claim_bound_proof(contract: &EngineeringContract) -> bool {
    contract
        .proof_obligations
        .iter()
        .take(MAX_DESIGN_SOURCES)
        .any(is_claim_bound)
}

fn is_claim_bound(obligation: &ProofObligation) -> bool {
    obligation.evidence_binding.is_bound() && binding_has_target(&obligation.evidence_binding)
}

fn binding_has_target(binding: &EvidenceBinding) -> bool {
    match binding {
        EvidenceBinding::Category => false,
        EvidenceBinding::CheckIds(targets)
        | EvidenceBinding::TestSelectors(targets)
        | EvidenceBinding::ContractIds(targets)
        | EvidenceBinding::PropertyIds(targets)
        | EvidenceBinding::RuntimeAssertions(targets)
        | EvidenceBinding::TraceProperties(targets) => targets
            .iter()
            .take(MAX_FOCUS_BINDING_TARGETS)
            .any(|target| !target.is_empty()),
        EvidenceBinding::TestSelector(target) | EvidenceBinding::ProviderClaim(target) => {
            !target.is_empty()
        }
    }
}

fn for_each_binding_target(binding: &EvidenceBinding, mut visit: impl FnMut(&str)) {
    match binding {
        EvidenceBinding::Category => {}
        EvidenceBinding::CheckIds(targets)
        | EvidenceBinding::TestSelectors(targets)
        | EvidenceBinding::ContractIds(targets)
        | EvidenceBinding::PropertyIds(targets)
        | EvidenceBinding::RuntimeAssertions(targets)
        | EvidenceBinding::TraceProperties(targets) => {
            for target in targets.iter().take(MAX_FOCUS_BINDING_TARGETS) {
                visit(target);
            }
        }
        EvidenceBinding::TestSelector(target) | EvidenceBinding::ProviderClaim(target) => {
            visit(target);
        }
    }
}

fn first_concern(contract: &EngineeringContract, kind: DesignReadinessKind) -> Option<String> {
    let tokens = kind.concern_tokens();
    if tokens.is_empty() {
        return None;
    }
    for risk in contract.risks.iter().take(MAX_DESIGN_SOURCES) {
        if text_mentions_any(&risk.description, tokens) {
            return Some(bounded_text(&risk.description, MAX_DESIGN_FINDING_BYTES));
        }
    }
    for invariant in contract.invariants.iter().take(MAX_DESIGN_SOURCES) {
        if text_mentions_any(&invariant.statement, tokens) {
            return Some(bounded_text(&invariant.statement, MAX_DESIGN_FINDING_BYTES));
        }
    }
    for constraint in contract.constraints.iter().take(MAX_DESIGN_SOURCES) {
        if text_mentions_any(&constraint.statement, tokens) {
            return Some(bounded_text(
                &constraint.statement,
                MAX_DESIGN_FINDING_BYTES,
            ));
        }
    }
    None
}

fn text_mentions_any(text: &str, tokens: &[&str]) -> bool {
    let mut hit = false;
    scan_tokens(text, |token| {
        if token_names_any(token, tokens) {
            hit = true;
        }
        !hit
    });
    hit
}

fn has_concern_coverage(contract: &EngineeringContract, kind: DesignReadinessKind) -> bool {
    let tokens = kind.concern_tokens();
    if tokens.is_empty() {
        return false;
    }
    contract
        .proof_obligations
        .iter()
        .take(MAX_DESIGN_SOURCES)
        .any(|obligation| is_claim_bound(obligation) && proof_mentions(obligation, tokens))
}

fn proof_mentions(obligation: &ProofObligation, tokens: &[&str]) -> bool {
    if text_mentions_any(&obligation.claim, tokens) {
        return true;
    }
    let mut hit = false;
    for_each_binding_target(&obligation.evidence_binding, |target| {
        if !hit {
            hit = text_mentions_any(target, tokens);
        }
    });
    hit
}

/// TRUE when `token` is one of `keywords`, or one of its underscore-separated
/// identifier components is. Selectors such as
/// `auth::refresh::replay_is_rejected` are lowercase-ASCII runs that keep
/// underscores, so component matching is what makes their concern coverage
/// visible without substring heuristics.
fn token_names_any(token: &str, keywords: &[&str]) -> bool {
    keywords.contains(&token) || token.split('_').any(|part| keywords.contains(&part))
}

fn collect_source_terms(text: &str, terms: &mut BTreeSet<String>) {
    if terms.len() >= MAX_FOCUS_CANDIDATES {
        return;
    }
    scan_tokens(text, |token| {
        if !FOCUS_STOPWORDS.contains(&token) {
            terms.insert(token.to_string());
        }
        terms.len() < MAX_FOCUS_CANDIDATES
    });
}

/// Visit sanitized lowercase ASCII tokens (alphanumeric/underscore runs) from
/// at most [`MAX_FOCUS_SOURCE_BYTES`] of `text`, stopping when `visit` returns
/// false. Oversized runs are truncated at [`MAX_DESIGN_FOCUS_TERM_BYTES`] and
/// short runs are dropped; iteration never slices a UTF-8 boundary.
fn scan_tokens(text: &str, mut visit: impl FnMut(&str) -> bool) {
    let source = bounded_slice(text, MAX_FOCUS_SOURCE_BYTES);
    let mut token = String::new();
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if token.len() < MAX_DESIGN_FOCUS_TERM_BYTES {
                token.push(ch.to_ascii_lowercase());
            }
        } else if !token.is_empty() {
            let meaningful = token.len() >= MIN_DESIGN_FOCUS_TERM_BYTES;
            let keep_going = !meaningful || visit(&token);
            token.clear();
            if !keep_going {
                return;
            }
        }
    }
    if token.len() >= MIN_DESIGN_FOCUS_TERM_BYTES {
        let _ = visit(&token);
    }
}

fn bounded_slice(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    bounded_slice(text, max_bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        AcceptedStrength, Constraint, Invariant, ProofEvidenceKind, VerificationDepth,
    };

    fn obligation(
        claim: &str,
        requirement: Requirement,
        binding: EvidenceBinding,
    ) -> ProofObligation {
        EngineeringContract::obligation_bound(
            claim,
            ProofEvidenceKind::TestExecution,
            ContractOrigin::User,
            requirement,
            binding,
            AcceptedStrength::for_requirement(requirement),
        )
    }

    fn risk(description: &str, severity: RiskSeverity) -> RiskHypothesis {
        RiskHypothesis {
            id: format!("r1:test:{}", description.len()),
            description: description.to_string(),
            severity,
            origin: ContractOrigin::Heuristic,
        }
    }

    fn manual_finding(
        kind: DesignReadinessKind,
        statement: &str,
        severity: RiskSeverity,
    ) -> DesignReadinessFinding {
        DesignReadinessFinding {
            id: finding_id(kind, statement),
            kind,
            statement: statement.to_string(),
            basis: "test basis".to_string(),
            severity,
        }
    }

    fn audit_of(findings: Vec<DesignReadinessFinding>) -> DesignReadinessAudit {
        DesignReadinessAudit {
            findings,
            focus_terms: Vec::new(),
        }
    }

    fn focus_of_bytes(bytes: usize) -> Vec<String> {
        vec!["x".repeat(bytes)]
    }

    #[test]
    fn empty_contract_has_no_findings_terms_or_dividend() {
        let contract = EngineeringContract::default();
        let audit = audit_compiled_design(&contract);
        assert!(audit.is_empty());
        assert!(audit.findings.is_empty());
        assert!(audit.focus_terms.is_empty());
        assert_eq!(apply_design_readiness(&mut contract.clone(), &audit), 0);
        assert!(!design_focus_is_economic(0, &[]));
        assert!(!design_focus_is_economic(1 << 20, &[]));
    }

    #[test]
    fn hostile_oversized_contract_content_is_bounded_and_never_panics() {
        let huge = format!("{}\u{0}\n{}", "A".repeat(100_000), "é".repeat(50_000));
        let mut contract = EngineeringContract::default();
        contract.proof_obligations.push(obligation(
            &huge,
            Requirement::Required,
            EvidenceBinding::Category,
        ));
        contract.risks.push(risk(&huge, RiskSeverity::Unknown));
        contract.invariants.push(Invariant {
            id: "i1:hostile".into(),
            statement: huge.clone(),
            origin: ContractOrigin::User,
        });
        contract.constraints.push(Constraint {
            id: "c1:hostile".into(),
            statement: huge.clone(),
            origin: ContractOrigin::User,
        });

        let audit = audit_compiled_design(&contract);
        assert!(audit.findings.len() <= MAX_DESIGN_READINESS_FINDINGS);
        assert!(audit.focus_terms.len() <= MAX_DESIGN_FOCUS_TERMS);
        for finding in &audit.findings {
            assert!(finding.statement.len() <= MAX_DESIGN_FINDING_BYTES);
            assert!(finding.basis.len() <= MAX_DESIGN_BASIS_BYTES);
            assert!(finding.id.starts_with("dr1:"));
        }
        for term in &audit.focus_terms {
            assert!(term.len() >= MIN_DESIGN_FOCUS_TERM_BYTES);
            assert!(term.len() <= MAX_DESIGN_FOCUS_TERM_BYTES);
            assert!(term
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        }

        let before_obligations = contract.proof_obligations.clone();
        let existing_risks = contract.risks.len();
        let applied = apply_design_readiness(&mut contract, &audit);
        assert!(applied <= MAX_DESIGN_READINESS_FINDINGS);
        assert_eq!(contract.proof_obligations, before_obligations);
        for row in &contract.risks[existing_risks..] {
            assert!(row.description.len() <= MAX_DESIGN_FINDING_BYTES);
        }
    }

    #[test]
    fn duplicate_and_oversized_bindings_yield_unique_sorted_bounded_terms() {
        let oversized = "Check".repeat(200);
        let mut contract = EngineeringContract::default();
        contract.proof_obligations.push(obligation(
            "claim",
            Requirement::Required,
            EvidenceBinding::TestSelectors(vec![
                "auth::refresh".into(),
                "auth::refresh".into(),
                oversized.clone(),
                oversized,
                String::new(),
            ]),
        ));
        contract.proof_obligations.push(obligation(
            "claim",
            Requirement::Required,
            EvidenceBinding::CheckIds((0..10_000).map(|index| format!("check_{index}")).collect()),
        ));

        let terms = design_focus_terms(&contract);
        assert!(terms.len() <= MAX_DESIGN_FOCUS_TERMS);
        assert!(terms.windows(2).all(|pair| pair[0] < pair[1]));
        for term in &terms {
            assert!(term.len() >= MIN_DESIGN_FOCUS_TERM_BYTES);
            assert!(term.len() <= MAX_DESIGN_FOCUS_TERM_BYTES);
            assert!(term
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        }
        assert_eq!(terms, design_focus_terms(&contract));
    }

    #[test]
    fn unbound_required_proof_is_reported_without_mutating_obligations() {
        let mut contract = EngineeringContract::default();
        contract.proof_obligations.push(obligation(
            "the replay claim holds",
            Requirement::Required,
            EvidenceBinding::Category,
        ));
        let audit = audit_compiled_design(&contract);
        assert!(audit
            .findings
            .iter()
            .any(|finding| finding.kind == DesignReadinessKind::UnboundRequiredProof));
        assert!(audit.focus_terms.is_empty());

        let obligations_before = contract.proof_obligations.clone();
        let constraints_before = contract.constraints.clone();
        let invariants_before = contract.invariants.clone();
        let applied = apply_design_readiness(&mut contract, &audit);
        assert!(applied > 0);
        assert_eq!(contract.proof_obligations, obligations_before);
        assert_eq!(contract.constraints, constraints_before);
        assert_eq!(contract.invariants, invariants_before);
        for row in &contract.risks {
            assert_eq!(row.severity, RiskSeverity::Medium);
            assert_eq!(row.origin, ContractOrigin::Heuristic);
            assert!(row.description.starts_with("required proof obligation "));
        }
        assert!(!contract.has_elevated_risk());
        assert_eq!(
            contract.verification_depth(),
            VerificationDepth::Standard,
            "readiness alone must never raise the verification floor"
        );

        // Idempotent: the same finding identity/description is never duplicated.
        assert_eq!(apply_design_readiness(&mut contract, &audit), 0);
        assert_eq!(contract.risks.len(), applied);
    }

    #[test]
    fn promotion_rule_clamps_new_findings_on_a_calm_contract() {
        let mut calm = EngineeringContract::default();
        let audit = audit_of(vec![manual_finding(
            DesignReadinessKind::ElevatedRiskWithoutProof,
            "a high severity hypothesis",
            RiskSeverity::High,
        )]);
        assert_eq!(apply_design_readiness(&mut calm, &audit), 1);
        assert_eq!(calm.risks[0].severity, RiskSeverity::Medium);
        assert!(!calm.has_elevated_risk());
        assert_eq!(calm.verification_depth(), VerificationDepth::Standard);

        let mut already_high = EngineeringContract {
            risks: vec![risk("existing high risk", RiskSeverity::High)],
            ..EngineeringContract::default()
        };
        assert_eq!(apply_design_readiness(&mut already_high, &audit), 1);
        assert!(already_high
            .risks
            .iter()
            .any(|row| row.description == "a high severity hypothesis"
                && row.severity == RiskSeverity::High));

        let unknown_audit = audit_of(vec![manual_finding(
            DesignReadinessKind::ElevatedRiskWithoutProof,
            "an unknown severity hypothesis",
            RiskSeverity::Unknown,
        )]);
        let mut already_unknown = EngineeringContract {
            risks: vec![risk("existing unknown risk", RiskSeverity::Unknown)],
            ..EngineeringContract::default()
        };
        assert_eq!(
            apply_design_readiness(&mut already_unknown, &unknown_audit),
            1
        );
        assert!(already_unknown
            .risks
            .iter()
            .any(|row| row.description == "an unknown severity hypothesis"
                && row.severity == RiskSeverity::Unknown));
    }

    #[test]
    fn elevated_pre_audit_contract_preserves_either_elevated_finding_tier() {
        let high_contract = EngineeringContract {
            risks: vec![risk("existing high risk", RiskSeverity::High)],
            ..EngineeringContract::default()
        };
        let mut high_contract = high_contract;
        let unknown_finding = audit_of(vec![manual_finding(
            DesignReadinessKind::ElevatedRiskWithoutProof,
            "unknown finding on a high contract",
            RiskSeverity::Unknown,
        )]);
        assert_eq!(
            apply_design_readiness(&mut high_contract, &unknown_finding),
            1
        );
        assert_eq!(high_contract.risks[1].severity, RiskSeverity::Unknown);

        let mut unknown_contract = EngineeringContract {
            risks: vec![risk("existing unknown risk", RiskSeverity::Unknown)],
            ..EngineeringContract::default()
        };
        let high_finding = audit_of(vec![manual_finding(
            DesignReadinessKind::ElevatedRiskWithoutProof,
            "high finding on an unknown contract",
            RiskSeverity::High,
        )]);
        assert_eq!(
            apply_design_readiness(&mut unknown_contract, &high_finding),
            1
        );
        assert_eq!(unknown_contract.risks[1].severity, RiskSeverity::High);

        let mut calm = EngineeringContract::default();
        assert_eq!(apply_design_readiness(&mut calm, &high_finding), 1);
        assert_eq!(calm.risks[0].severity, RiskSeverity::Medium);
        assert!(!calm.has_elevated_risk());
    }

    #[test]
    fn mutation_witness_promotion_rule_would_escalate_without_the_clamp() {
        // Counterfactual witness: the raw finding severity is High, but the
        // calm-contract apply path must emit Medium. Deleting the clamp in
        // apply_design_readiness (`_ => RiskSeverity::Medium`) makes this fail.
        let mut contract = EngineeringContract::default();
        let audit = audit_of(vec![manual_finding(
            DesignReadinessKind::ElevatedRiskWithoutProof,
            "mutation witness: high finding on a calm contract",
            RiskSeverity::High,
        )]);
        apply_design_readiness(&mut contract, &audit);
        assert_eq!(contract.risks.len(), 1);
        assert_eq!(contract.risks[0].severity, RiskSeverity::Medium);
        assert!(!contract.has_elevated_risk());
    }

    #[test]
    fn economic_gate_boundaries_at_one_eighth_and_two_kib() {
        // 65536 baseline: the 12.5% boundary (8192 bytes) dominates.
        assert!(design_focus_is_economic(65_536, &focus_of_bytes(57_344)));
        assert!(!design_focus_is_economic(65_536, &focus_of_bytes(57_345)));
        // 8192 baseline: the 2 KiB boundary dominates.
        assert!(design_focus_is_economic(8_192, &focus_of_bytes(6_144)));
        assert!(!design_focus_is_economic(8_192, &focus_of_bytes(6_145)));
        // Degenerate inputs are refused.
        assert!(!design_focus_is_economic(0, &focus_of_bytes(0)));
        assert!(!design_focus_is_economic(4_096, &focus_of_bytes(4_096)));
        assert!(!design_focus_is_economic(4_096, &focus_of_bytes(8_192)));
        assert!(!design_focus_is_economic(4_096, &[]));
        // No overflow with hostile sizes.
        assert!(design_focus_is_economic(usize::MAX, &focus_of_bytes(2_048)));
        assert!(!design_focus_is_economic(usize::MAX, &focus_of_bytes(0)));
    }

    #[test]
    fn mutation_witness_economic_gate_rejects_each_boundary_crossing() {
        // Counterfactual witness: weakening either conjunct (drop `* 8 >=` or
        // MIN_DESIGN_FOCUS_SAVINGS_BYTES) makes one of these fail.
        assert!(design_focus_is_economic(65_536, &focus_of_bytes(57_344)));
        assert!(!design_focus_is_economic(65_536, &focus_of_bytes(57_345)));
        assert!(design_focus_is_economic(8_192, &focus_of_bytes(6_144)));
        assert!(!design_focus_is_economic(8_192, &focus_of_bytes(6_145)));
    }

    #[test]
    fn focus_terms_are_sanitized_bounded_unique_sorted_and_deterministic() {
        let mut contract = EngineeringContract::default();
        contract.proof_obligations.push(obligation(
            "existing test passes",
            Requirement::Required,
            EvidenceBinding::TestSelector("auth::refresh::replay_is_rejected".into()),
        ));
        contract.risks.push(risk(
            "Race in the CACHE\u{0007}\u{1F600}, replayed",
            RiskSeverity::High,
        ));
        contract.invariants.push(Invariant {
            id: "i1:migration".into(),
            statement: "The MIGRATION must be ATOMIC".into(),
            origin: ContractOrigin::User,
        });

        let terms = design_focus_terms(&contract);
        assert_eq!(terms, design_focus_terms(&contract));
        assert!(terms.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(terms.len() <= MAX_DESIGN_FOCUS_TERMS);
        for term in &terms {
            assert!(term.len() >= MIN_DESIGN_FOCUS_TERM_BYTES);
            assert!(term.len() <= MAX_DESIGN_FOCUS_TERM_BYTES);
            assert!(!term.is_empty());
            assert!(term
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        }
        for expected in [
            "atomic",
            "auth",
            "cache",
            "migration",
            "race",
            "refresh",
            "replay_is_rejected",
        ] {
            assert!(
                terms.iter().any(|term| term == expected),
                "missing {expected}"
            );
        }
        assert!(!terms.iter().any(|term| term == "the" || term == "must"));
    }

    #[test]
    fn concern_classes_need_matching_claim_bound_coverage() {
        let concurrency = "the matched surface may contain a race under replayed execution";
        let migration = "the migration may corrupt or strand existing data";
        let compatibility = "the public API surface stays source-compatible";

        for (kind, text) in [
            (DesignReadinessKind::ConcurrencyWithoutProof, concurrency),
            (DesignReadinessKind::MigrationWithoutProof, migration),
            (
                DesignReadinessKind::CompatibilityWithoutProof,
                compatibility,
            ),
        ] {
            let mut uncovered = EngineeringContract::default();
            uncovered.risks.push(risk(text, RiskSeverity::Medium));
            uncovered.proof_obligations.push(obligation(
                "the component still compiles",
                Requirement::Advisory,
                EvidenceBinding::CheckIds(vec!["rust_check".into()]),
            ));
            let audit = audit_compiled_design(&uncovered);
            assert!(
                audit.findings.iter().any(|finding| finding.kind == kind),
                "{kind:?} must be reported without matching coverage"
            );
            assert!(audit
                .findings
                .iter()
                .all(|finding| finding.severity == RiskSeverity::Medium));

            let mut covered = uncovered.clone();
            covered.proof_obligations.push(obligation(
                text,
                Requirement::Required,
                EvidenceBinding::TestSelector(format!("{}::regression", kind.as_str())),
            ));
            let audit = audit_compiled_design(&covered);
            assert!(
                !audit.findings.iter().any(|finding| finding.kind == kind),
                "{kind:?} must be covered by a matching claim-bound proof"
            );
        }
    }

    #[test]
    fn one_entry_can_ground_every_concern_class_it_names() {
        let mut contract = EngineeringContract::default();
        contract.risks.push(risk(
            "a concurrent replay during migration of the public API",
            RiskSeverity::Medium,
        ));
        let audit = audit_compiled_design(&contract);
        for kind in [
            DesignReadinessKind::ConcurrencyWithoutProof,
            DesignReadinessKind::MigrationWithoutProof,
            DesignReadinessKind::CompatibilityWithoutProof,
        ] {
            assert!(
                audit.findings.iter().any(|finding| finding.kind == kind),
                "{kind:?} must be grounded by a shared entry"
            );
        }

        contract.proof_obligations.push(obligation(
            "concurrent replay is rejected",
            Requirement::Required,
            EvidenceBinding::TestSelector("race_replay::migration::api".into()),
        ));
        let covered = audit_compiled_design(&contract);
        for kind in [
            DesignReadinessKind::ConcurrencyWithoutProof,
            DesignReadinessKind::MigrationWithoutProof,
            DesignReadinessKind::CompatibilityWithoutProof,
        ] {
            assert!(
                !covered.findings.iter().any(|finding| finding.kind == kind),
                "{kind:?} must be covered by the shared claim-bound proof"
            );
        }
    }

    #[test]
    fn elevated_risk_gap_requires_a_non_empty_claim_binding() {
        let mut contract = EngineeringContract {
            risks: vec![risk("a high risk", RiskSeverity::High)],
            ..EngineeringContract::default()
        };
        contract.proof_obligations.push(obligation(
            "unbound claim",
            Requirement::Required,
            EvidenceBinding::Category,
        ));
        let audit = audit_compiled_design(&contract);
        assert!(audit
            .findings
            .iter()
            .any(|finding| finding.kind == DesignReadinessKind::ElevatedRiskWithoutProof));

        let mut bound = contract.clone();
        bound.proof_obligations.push(obligation(
            "bound claim",
            Requirement::Required,
            EvidenceBinding::CheckIds(vec!["cargo-test".into()]),
        ));
        let audit = audit_compiled_design(&bound);
        assert!(!audit
            .findings
            .iter()
            .any(|finding| finding.kind == DesignReadinessKind::ElevatedRiskWithoutProof));

        let mut empty_targets = contract.clone();
        empty_targets.proof_obligations = vec![obligation(
            "empty target",
            Requirement::Required,
            EvidenceBinding::CheckIds(Vec::new()),
        )];
        let audit = audit_compiled_design(&empty_targets);
        assert!(audit
            .findings
            .iter()
            .any(|finding| finding.kind == DesignReadinessKind::ElevatedRiskWithoutProof));
    }

    #[test]
    fn apply_is_bounded_and_refuses_empty_or_duplicate_findings() {
        let mut findings = vec![
            manual_finding(
                DesignReadinessKind::UnboundRequiredProof,
                "",
                RiskSeverity::High,
            ),
            manual_finding(
                DesignReadinessKind::UnboundRequiredProof,
                "duplicate finding",
                RiskSeverity::Medium,
            ),
        ];
        findings.push(findings[1].clone());
        findings.extend((0..(MAX_DESIGN_READINESS_FINDINGS + 4)).map(|index| {
            manual_finding(
                DesignReadinessKind::UnboundRequiredProof,
                &format!("finding {index}"),
                RiskSeverity::Medium,
            )
        }));
        let audit = audit_of(findings);

        let mut contract = EngineeringContract::default();
        let applied = apply_design_readiness(&mut contract, &audit);
        assert_eq!(applied, MAX_DESIGN_READINESS_FINDINGS);
        assert_eq!(contract.risks.len(), MAX_DESIGN_READINESS_FINDINGS);
        assert!(contract.risks.iter().all(|row| !row.description.is_empty()));

        let before = contract.risks.len();
        assert_eq!(apply_design_readiness(&mut contract, &audit), 0);
        assert_eq!(contract.risks.len(), before);
    }
}
