//! TaskExecutor verification: tournament requests, proof bases and verdict composition (mechanically split from `task_executor`).

use super::*;

/// One tournament start: goal + criteria + candidate count + the
/// per-run knobs the one task-start authority understands. The strict
/// server DTO maps onto this; the programmatic/test boundary may set the
/// isolated root and crash seam directly.
#[derive(Debug, Clone)]
pub struct TournamentStartRequest {
    pub goal: String,
    pub criteria: Vec<String>,
    pub n: usize,
    /// Decoded for wire compatibility only: the sole decodable value is
    /// [`MutationMode::Shadow`] (a `direct_compat` value is a strict decode
    /// error naming the removal). Candidates are ALWAYS isolated
    /// worktrees — no value can change that.
    pub mutation_mode: Option<MutationMode>,
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    pub max_cost_micro: Option<u64>,
    /// Files attached to every candidate's ordinary drive.
    pub files: Vec<String>,
    /// Capability ceiling of the parent. Default: read+write on the whole
    /// workspace (candidate writes land ONLY in daemon-allocated isolated
    /// worktrees; integration stays explicit).
    pub parent_caps: CapabilitySet,
    pub ceilings: crate::runtime::Ceilings,
    /// Root under which isolated candidate workspaces are created. Empty =
    /// the executor's daemon-owned [`CandidateWorkspaceService`] allocates
    /// one (the wire never carries a path).
    pub isolated_root: PathBuf,
    /// Deterministic crash seam (adversarial tests only).
    pub crash_seam: Option<CrashSeam>,
}

impl Default for TournamentStartRequest {
    fn default() -> Self {
        Self {
            goal: String::new(),
            criteria: Vec::new(),
            n: 0,
            mutation_mode: None,
            model: None,
            max_tokens: None,
            max_cost_micro: None,
            files: Vec::new(),
            parent_caps: child_caps(WorkKind::Implementation),
            ceilings: crate::runtime::Ceilings::default(),
            isolated_root: PathBuf::new(),
            crash_seam: None,
        }
    }
}

impl TournamentStartRequest {
    /// Structural validation (the engine enforces the typed candidate band
    /// and criterion bounds; this only checks the request shape before any
    /// durable write).
    pub fn validate(&self) -> Result<(), ExecError> {
        if !(crate::tournament::MIN_CANDIDATES..=crate::tournament::MAX_CANDIDATES)
            .contains(&self.n)
        {
            return Err(ExecError::InvalidPlan(format!(
                "tournament candidate count {} is outside the supported band {}..={}",
                self.n,
                crate::tournament::MIN_CANDIDATES,
                crate::tournament::MAX_CANDIDATES
            )));
        }
        if self.goal.trim().is_empty() {
            return Err(ExecError::InvalidPlan("tournament goal is empty".into()));
        }
        if self.criteria.is_empty() {
            return Err(ExecError::InvalidPlan(
                "a tournament needs at least one criterion".into(),
            ));
        }
        self.ceilings.validate().map_err(ExecError::InvalidPlan)?;
        Ok(())
    }
}

/// The receipt of one accepted tournament start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TournamentReceipt {
    pub tournament_id: String,
    pub run_id: String,
    /// The deterministic candidate child ids (`child-0..child-{n-1}`).
    pub candidates: Vec<String>,
}

pub(crate) fn build_tournament_criteria(
    specs: &[String],
) -> Result<Vec<crate::tournament::Criterion>, ExecError> {
    let mut criteria = Vec::with_capacity(specs.len());
    for spec in specs {
        criteria.push(crate::tournament::Criterion::derive(spec).map_err(tournament_exec_error)?);
    }
    Ok(criteria)
}

/// Map the tournament engine's typed refusals onto the executor error
/// space (the HTTP layer maps these onto 400/404/409 exactly like every
/// other executor refusal).
pub(crate) fn tournament_exec_error(e: crate::tournament::TournamentError) -> ExecError {
    use crate::tournament::TournamentError as T;
    match &e {
        T::NotFound(_) => ExecError::NotFound(e.to_string()),
        T::Oversized(_) => ExecError::Oversized(e.to_string()),
        T::NotOpen(_) | T::DuplicateSettlement(_) | T::NoEligibleWinner(_) => {
            ExecError::Conflict(e.to_string())
        }
        T::Corrupt { .. } | T::Ledger(_) | T::Cleanup(_) => ExecError::Internal(e.to_string()),
        T::InvalidCandidateCount { .. }
        | T::InvalidCriteriaCount { .. }
        | T::InvalidCriterion(_)
        | T::DuplicateCriterion(_)
        | T::InvalidId(_)
        | T::UnknownCandidate(_)
        | T::IllegalSettlementState(_)
        | T::VerificationSpecDrift
        | T::ReviewNotIndependent(_) => ExecError::InvalidPlan(e.to_string()),
    }
}

/// The default effective capability grant of one work item's child:
/// read-only items read the workspace and may run bounded process
/// verification (Read + Execute); mutating items also write it (the loud
/// spawn check refuses Write on a `NoWrites` item). Narrower policies are
/// declared per item on top of this default, and the sandbox, semantic
/// restrictions and permission requester still gate every actual tool call.
pub fn child_caps(kind: WorkKind) -> CapabilitySet {
    let mut grants = vec![
        CapabilityGrant::new(
            LatticeCap::ReadWorkspace,
            ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
        ),
        CapabilityGrant::new(
            LatticeCap::ExecuteShell,
            ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
        ),
    ];
    if kind.is_mutating() {
        grants.push(CapabilityGrant::new(
            LatticeCap::WriteWorkspace,
            ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
        ));
    }
    CapabilitySet::from_grants(grants).expect("wildcard grants are sane")
}

/// The read-only file capability of an item whose writes are NOT
/// file-level: read-only items (all of them) and semantic-entity items
/// (their writes are provider-scoped; file-level WriteWorkspace would
/// exceed the ownership their compile assigned).
pub(crate) fn read_child_caps() -> CapabilitySet {
    CapabilitySet::from_grants([CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
    )])
    .expect("wildcard grants are sane")
}

/// Write one linkage row under the session (bounded value; loud refusal
/// when the row would exceed the memory-fact cap).
pub(crate) fn put_run_row(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
    row: &TaskRunRow,
) -> Result<(), ExecError> {
    if run_id.is_empty()
        || run_id.len() > MAX_RUN_ID_CHARS
        || !run_id.is_ascii()
        || run_id.contains('/')
    {
        return Err(ExecError::Oversized(format!(
            "run id must be 1..={MAX_RUN_ID_CHARS} ASCII characters without '/'"
        )));
    }
    let value = serde_json::to_string(row)
        .map_err(|e| ExecError::Internal(format!("run row serialization: {e}")))?;
    if value.len() > MAX_TASK_RUN_ROW_BYTES {
        return Err(ExecError::Oversized(format!(
            "task run row of {} bytes exceeds the {MAX_TASK_RUN_ROW_BYTES}-byte bound",
            value.len()
        )));
    }
    handle
        .upsert_memory_fact(TASK_RUN_ROW_KIND, run_id, &value)
        .map_err(|e| ExecError::Internal(format!("task run row write: {}", e.message)))?;
    Ok(())
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Record one run's accepted completion contract before its first model
/// call (P2 record-first): the durable `CompletionContractSet` row lands at
/// the task row's CURRENT revision and is immutable per revision. `None`
/// and the explicit all-false contract write nothing — the default
/// completion path stays byte-identical.
pub(crate) fn record_completion_contract(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
    contract: Option<CompletionContract>,
) -> Result<(), ExecError> {
    let Some(contract) = contract.filter(|c| !c.is_default()) else {
        return Ok(());
    };
    let revision = handle
        .task_revision(task_id)
        .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
    handle
        .set_completion_contract(task_id, revision, contract)
        .map(|_seq| ())
        .map_err(|e| match e {
            faktor_session::TaskError::CompletionContractImmutable { .. }
            | faktor_session::TaskError::RevisionMismatch { .. } => {
                ExecError::Conflict(format!("completion contract: {e}"))
            }
            other => ExecError::Internal(format!("completion contract seed: {other}")),
        })
}

/// TRUE when the durable verification fact of the session's latest genuine
/// end says the deterministic verification PASSED. UI/reporting ONLY (the
/// `SettlementOutcome.verified` projection): it is NEVER an authorization
/// for a completion side effect — those run exclusively through
/// [`CompletionStepRunner::run_completion_steps`] with an immutable
/// verification-record proof.
pub(crate) fn verification_passed(handle: &faktor_session::SessionHandle) -> bool {
    // Documented UI-only projection (never authorization). A store read
    // failure still reports not-passed, but it is VISIBLE as a typed log
    // instead of being indistinguishable from a recorded failure.
    let facts = match handle.memory_facts() {
        Ok(facts) => facts,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "verification_passed: fact store unreadable; reporting not-passed"
            );
            return false;
        }
    };
    let Some((_, _, last)) = facts
        .iter()
        .find(|(kind, key, _)| kind == "verification" && key == "last")
    else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(last) else {
        return false;
    };
    value.get("status").and_then(|s| s.as_str()) == Some("passed")
}

/// The ONE composed root verification verdict of one orchestrated run: the
/// authoritative status of `verify_prepared_integration` and the only
/// authorization a [`VerifiedRunIntegration`] may carry.
///
/// `Passed` ONLY when EVERY required check passed AND every required
/// criterion passed. A failed required check/criterion is `Failed`; a
/// required check/criterion that produced no verdict — explicitly
/// [`CriterionBinding::Unavailable`], a required-check binding that resolves
/// to nothing, or a non-pass with no evidence — is `Unavailable`, never
/// `Pending`. Advisory (`Preferred`) criteria, optional checks and their
/// verdicts never block.
pub fn compose_root_verification_status(
    checks: &[CheckExecution],
    criteria: &[CriterionVerification],
) -> VerificationStatus {
    let check_verdicts = checks
        .iter()
        .filter(|check| check.required)
        .map(required_check_verdict);
    let criterion_verdicts = criteria
        .iter()
        .filter(|criterion| criterion_is_required(criterion))
        .map(|criterion| criterion_verification_status(checks, criterion));
    merge_verdict_sides(check_verdicts.chain(criterion_verdicts))
}

/// The STRICTER no-op rule of an EMPTY aggregate change set: there is no
/// check evidence to lean on, so EVERY criterion verdict — advisory included
/// — must pass before the empty run may compose `Passed`. A non-passing
/// advisory criterion degrades this verdict to `Failed`/`Unavailable`; it
/// never silently passes.
pub fn compose_no_op_root_verification_status(
    checks: &[CheckExecution],
    criteria: &[CriterionVerification],
) -> VerificationStatus {
    let check_verdicts = checks
        .iter()
        .filter(|check| check.required)
        .map(required_check_verdict);
    merge_verdict_sides(
        check_verdicts.chain(
            criteria
                .iter()
                .map(|criterion| criterion_verification_status(checks, criterion)),
        ),
    )
}

/// The verdict of ONE required check row: an unknown/pending status is
/// missing evidence (`Unavailable`) — never a pass and never a `Pending`.
pub(crate) fn required_check_verdict(check: &CheckExecution) -> VerificationStatus {
    match check.status {
        VerificationStatus::Passed => VerificationStatus::Passed,
        VerificationStatus::Failed => VerificationStatus::Failed,
        _ => VerificationStatus::Unavailable,
    }
}

/// Whether one criterion verdict belongs to a REQUIRED criterion. The key
/// is the persisted task entry: typed V2 entries decode to their
/// requirement; every other entry migrates as Required
/// ([`faktor_session::task::Criterion::legacy`]), so plain legacy text can
/// never be silently demoted to advisory.
pub(crate) fn criterion_is_required(criterion: &CriterionVerification) -> bool {
    match faktor_session::task::Criterion::decode(&criterion.criterion_key) {
        Some(typed) => typed.requirement.is_required(),
        None => true,
    }
}

/// The typed verdict of ONE criterion verification. A pass is a pass; a
/// `RequiredCheck` binding resolves against the EXECUTED check rows exactly
/// like the evaluator does (id + command digest) — a check that ran and
/// failed is `Failed`, an unresolved/ambiguous/missing binding is
/// `Unavailable`. The explicit honest-unknown binding and a non-pass with no
/// evidence are `Unavailable` (missing proof, never a failure conclusion and
/// never `Pending`); every other non-pass with evidence is `Failed`.
pub(crate) fn criterion_verification_status(
    checks: &[CheckExecution],
    criterion: &CriterionVerification,
) -> VerificationStatus {
    if criterion.passed {
        return VerificationStatus::Passed;
    }
    match &criterion.binding {
        Some(CriterionBinding::Unavailable { .. }) => return VerificationStatus::Unavailable,
        Some(CriterionBinding::RequiredCheck {
            check_id,
            command_digest,
        }) => return required_check_binding_verdict(checks, check_id, command_digest),
        _ => {}
    }
    if criterion
        .evidence
        .as_deref()
        .is_none_or(|evidence| evidence.trim().is_empty())
    {
        return VerificationStatus::Unavailable;
    }
    VerificationStatus::Failed
}

/// Resolve ONE required-check binding against the executed check rows using
/// the evaluator's exact identity (check id when present, canonical command
/// digest always). Exactly one match is required: a failed resolution is
/// `Failed`; anything unresolved or ambiguous is missing evidence
/// (`Unavailable`) — never a pass.
pub(crate) fn required_check_binding_verdict(
    checks: &[CheckExecution],
    check_id: &str,
    command_digest: &str,
) -> VerificationStatus {
    // A legacy 64-bit FNV digest can be viewed but never resolves: the
    // binding must be re-derived under the canonical BLAKE3 identity.
    if classify_authority_digest(command_digest) == AuthorityDigestKind::LegacyFnv {
        return VerificationStatus::Unavailable;
    }
    let matches: Vec<&CheckExecution> = checks
        .iter()
        .filter(|check| {
            (check_id.is_empty() || check.check == check_id)
                && command_binding_digest_parts(&check.program, &check.args, None, &[])
                    == command_digest
        })
        .collect();
    match matches.as_slice() {
        [only] => match only.status {
            VerificationStatus::Failed => VerificationStatus::Failed,
            VerificationStatus::Passed => {
                // The criterion verdict itself says not-passed; a passing row
                // cannot re-authorize it, so the contradiction blocks.
                VerificationStatus::Failed
            }
            _ => VerificationStatus::Unavailable,
        },
        _ => VerificationStatus::Unavailable,
    }
}

/// Fold verdict sides into ONE status with failure dominance: any failure
/// wins, otherwise any unavailable/missing side makes the whole
/// `Unavailable`; only all-passing sides compose `Passed`.
pub(crate) fn merge_verdict_sides(
    verdicts: impl Iterator<Item = VerificationStatus>,
) -> VerificationStatus {
    let mut unavailable = false;
    for verdict in verdicts {
        match verdict {
            VerificationStatus::Passed => {}
            VerificationStatus::Failed => return VerificationStatus::Failed,
            _ => unavailable = true,
        }
    }
    if unavailable {
        VerificationStatus::Unavailable
    } else {
        VerificationStatus::Passed
    }
}

/// The strictest of two verdicts on the same evidence (failure beats
/// unavailable beats pass): the agent's check-side acceptance never softens
/// the composed criterion verdict and vice versa.
pub(crate) fn merge_verification_status(
    left: VerificationStatus,
    right: VerificationStatus,
) -> VerificationStatus {
    let rank = |status: VerificationStatus| match status {
        VerificationStatus::Passed => 0u8,
        VerificationStatus::Unavailable
        | VerificationStatus::Pending
        | VerificationStatus::Running => 1,
        VerificationStatus::Failed => 2,
    };
    match rank(left).max(rank(right)) {
        0 => VerificationStatus::Passed,
        1 => VerificationStatus::Unavailable,
        _ => VerificationStatus::Failed,
    }
}

/// The durable fact tag of one composed verdict. `Pending`/`Running` never
/// compose, but map honestly to the legacy `pending` fact tag.
pub(crate) fn verification_status_tag(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Passed => "passed",
        VerificationStatus::Failed => "failed",
        VerificationStatus::Unavailable => "unavailable",
        VerificationStatus::Pending | VerificationStatus::Running => "pending",
    }
}

/// The domain separator of the canonical proof-basis digest (version 3):
/// the digest can never collide with an incidental serde/JCS encoding of the
/// same value, and the version is folded in so a future encoding change
/// invalidates every older digest instead of silently reusing it.
pub const PROOF_BASIS_DIGEST_DOMAIN: &[u8] = b"FAKTOR_PROOF_BASIS\0";
/// The canonical proof-basis encoding version.
pub const PROOF_BASIS_DIGEST_VERSION: u64 = 3;

/// The canonical, TOTAL proof-basis payload: `FAKTOR_PROOF_BASIS\0` + the
/// version + every basis field in a fixed order with length-prefixed legs.
/// Total by construction (no serde, no `Result`, no `unwrap_or_default`):
/// every `String`/`u64`/`Vec` has exactly one encoding, so a serialization
/// failure is impossible and the digest is stable under value-level equality.
pub fn canonical_proof_basis_payload(basis: &ProofBasis) -> Vec<u8> {
    fn put_u64(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn put_str(out: &mut Vec<u8>, value: &str) {
        put_u64(out, value.len() as u64);
        out.extend_from_slice(value.as_bytes());
    }
    fn put_opt_str(out: &mut Vec<u8>, value: Option<&str>) {
        match value {
            Some(value) => {
                out.push(1);
                put_str(out, value);
            }
            None => out.push(0),
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(PROOF_BASIS_DIGEST_DOMAIN);
    put_u64(&mut out, PROOF_BASIS_DIGEST_VERSION);
    put_u64(&mut out, basis.task_id);
    put_u64(&mut out, basis.task_revision);
    put_str(&mut out, &basis.task_contract_digest);
    put_str(&mut out, &basis.candidate_snapshot);
    put_str(&mut out, &basis.integration_sources_digest);
    put_str(&mut out, &basis.changed_files_digest);
    put_u64(&mut out, basis.checks.len() as u64);
    for check in &basis.checks {
        put_str(&mut out, &check.check_id);
        put_str(&mut out, &check.program);
        put_u64(&mut out, check.args.len() as u64);
        for arg in &check.args {
            put_str(&mut out, arg);
        }
    }
    put_str(&mut out, &basis.verification_impl_version);
    put_u64(&mut out, basis.tool_versions.len() as u64);
    for tool in &basis.tool_versions {
        put_str(&mut out, &tool.tool);
        put_str(&mut out, &tool.version);
    }
    put_u64(&mut out, basis.env_projection.len() as u64);
    for (key, value) in &basis.env_projection {
        put_str(&mut out, key);
        put_str(&mut out, value);
    }
    match basis.instruction_epoch {
        Some(epoch) => {
            out.push(1);
            put_u64(&mut out, epoch);
        }
        None => out.push(0),
    }
    put_u64(&mut out, basis.criteria.len() as u64);
    for criterion in &basis.criteria {
        put_str(&mut out, &criterion.criterion_id);
        put_opt_str(&mut out, criterion.binding_digest.as_deref());
    }
    put_opt_str(&mut out, basis.reviewer_digest.as_deref());
    put_u64(&mut out, basis.evidence_digests.len() as u64);
    for digest in &basis.evidence_digests {
        put_str(&mut out, digest);
    }
    out
}

/// The canonical domain/version-separated proof-basis digest — the ONLY
/// digest the orchestrator writes into a record fingerprint or compares for
/// reuse. A fingerprint carrying the retired serde-bytes digest (or no
/// digest at all) is never reusable.
pub fn canonical_proof_basis_digest(basis: &ProofBasis) -> String {
    let payload = canonical_proof_basis_payload(basis);
    format!("blake3:{}", blake3::hash(&payload).to_hex())
}

/// The canonical-basis reuse consult: a candidate record is reusable ONLY
/// when its persisted proof-basis digest equals the canonical digest of the
/// current basis. Refusals carry the recorded vs current digests, and an
/// absent/legacy digest is an honest unknown — never a license.
pub(crate) fn canonical_proof_reuse(
    handle: &faktor_session::SessionHandle,
    record_id: VerificationRecordId,
    basis_digest: &str,
) -> Result<ProofReuse, ExecError> {
    let Some(record) = handle
        .get_verification_record(record_id)
        .map_err(|e| ExecError::Internal(format!("proof reuse record read: {e}")))?
    else {
        return Ok(ProofReuse::Refused {
            reason: format!("record {record_id} does not exist"),
        });
    };
    // Legacy 64-bit FNV identities inside a recorded fingerprint can be
    // VIEWED but never authorize reuse: the typed detection forces a fresh
    // record (reverification) instead of a proof reuse.
    if let Some(fingerprint) = record.environment_fingerprint.as_ref() {
        for (what, value) in [
            ("task-contract", fingerprint.task_contract_hash.as_str()),
            ("check-basis", fingerprint.check_argv_cwd_env_hash.as_str()),
        ] {
            if classify_authority_digest(value) == AuthorityDigestKind::LegacyFnv {
                return Ok(ProofReuse::Refused {
                    reason: format!(
                        "record {record_id} carries the legacy FNV {what} digest {value}; \
                         it can be viewed but never reused — reverify under the canonical \
                         BLAKE3 authority digests"
                    ),
                });
            }
        }
    }
    let Some(recorded) = record
        .environment_fingerprint
        .as_ref()
        .and_then(|f| f.proof_basis_digest.as_deref())
    else {
        return Ok(ProofReuse::Refused {
            reason: format!("record {record_id} carries no proof basis; it is never reusable"),
        });
    };
    if recorded == basis_digest {
        Ok(ProofReuse::Allowed)
    } else {
        Ok(ProofReuse::Refused {
            reason: format!(
                "record {record_id} is bound to proof basis {recorded}, but the current \
                 canonical basis is {basis_digest}; reuse requires an identical basis"
            ),
        })
    }
}

/// The bounded v20 environment fingerprint of one orchestrated ROOT record:
/// the proof-basis digest (the reuse key), the task-contract digest and the
/// check-basis digest, all deterministic for identical inputs. Empty tool/
/// manifest/env projections are HONEST absences at the executor layer (the
/// agent's attempt records carry their own observed fingerprint).
pub(crate) fn root_verification_fingerprint(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
    basis: &ProofBasis,
) -> Result<EnvironmentFingerprint, ExecError> {
    let task = handle
        .get_task(task_id)
        .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
        .ok_or_else(|| ExecError::Internal(format!("root task {task_id} missing")))?;
    Ok(EnvironmentFingerprint {
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        toolchain_versions: Vec::new(),
        manifest_hashes: Vec::new(),
        lockfile_hashes: Vec::new(),
        instruction_epoch: None,
        base_tree_hash: None,
        task_contract_hash: acceptance_criteria_authority_digest(&task.acceptance_criteria),
        check_argv_cwd_env_hash: check_basis_authority_digest(basis),
        verification_impl_version: faktor_agent::runtime::VERIFICATION_IMPL_VERSION.to_string(),
        proof_basis_digest: Some(canonical_proof_basis_digest(basis)),
    })
}

/// The compact candidate-proof reference of one orchestrated ROOT record:
/// the immutable run base, the verified candidate snapshot, the integration
/// sources digest and the changed-file digest, so both IDEs can show what
/// the proof was based on and completion can re-derive it.
pub(crate) fn root_verification_candidate_proof(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
    revision: TaskRevision,
    final_snapshot: &str,
    prepared: &PreparedRunIntegration,
    run: &faktor_agent::IntegratedRootVerification,
) -> Result<CandidateProofRef, ExecError> {
    let accounting = handle
        .accounting_snapshot_digest(task_id)
        .map_err(|e| ExecError::Internal(format!("accounting snapshot digest: {e}")))?;
    Ok(CandidateProofRef {
        task_revision: revision,
        base_manifest_hash: authority_digest_hex(DOMAIN_CANDIDATE_MANIFEST, 1, ()),
        candidate_manifest_hash: authority_digest_hex(DOMAIN_CANDIDATE_MANIFEST, 1, final_snapshot),
        source_diff_evidence: None,
        risk_report_evidence: None,
        accounting_snapshot_digest: accounting,
        run_id: Some(prepared.run_id.clone()),
        run_base_snapshot: Some(prepared.base_snapshot.clone()),
        candidate_snapshot: Some(final_snapshot.to_string()),
        sources_digest: (!prepared.sources_digest.is_empty())
            .then(|| prepared.sources_digest.clone()),
        changed_files_digest: (!run.changed.is_empty())
            .then(|| changed_files_authority_digest(&run.changed)),
    })
}

/// Write the REAL root verification fact of an orchestrated run (the SAME
/// `verification`/`last` shape the agent's genuine ends write): the executed
/// checks with their real pass/fail verdicts and the integrated change. The
/// fact is the UI/reporting projection of the run's verification state — the
/// production authorization for completion steps is exclusively the
/// immutable verification-record proof, never this advisory row.
/// The durable fact kind/key under which one orchestrated run records the
/// EXACT verification attempt its root settlement consumes. Keyed by run id
/// so two runs of the same parent session never share an attempt.
pub(crate) const ROOT_ATTEMPT_FACT_KIND: &str = "verification_attempt";

pub(crate) fn root_attempt_op_key(run_id: &str) -> String {
    format!("root:{run_id}")
}

/// The exact attempt op persisted for `run_id` (None before the first
/// attempt of the run — never a guess). FIX 2: a failed store read and a
/// PRESENT-but-malformed attempt fact are errors — collapsing either into
/// `None` would mint a superseding attempt over a live verification.
pub(crate) fn read_root_attempt_op(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
) -> Result<Option<u64>, ExecError> {
    let key = root_attempt_op_key(run_id);
    let facts = handle
        .memory_facts()
        .map_err(|e| ExecError::from(classify_session_read("verification-attempt facts", e)))?;
    let Some((_, _, value)) = facts
        .iter()
        .find(|(kind, k, _)| kind == ROOT_ATTEMPT_FACT_KIND && k == &key)
    else {
        return Ok(None);
    };
    let op: u64 = value.parse().map_err(|_| {
        ExecError::from(DurableStateError::CorruptDurableState {
            what: format!("verification-attempt fact {key}"),
            detail: format!("stored attempt op {value:?} is not a u64"),
        })
    })?;
    if op == 0 {
        return Err(ExecError::from(DurableStateError::CorruptDurableState {
            what: format!("verification-attempt fact {key}"),
            detail: "stored attempt op is zero".into(),
        }));
    }
    Ok(Some(op))
}

/// Persist the exact verification attempt op of `run_id` durably BEFORE the
/// attempt settles, so every later settlement consumes that exact attempt.
pub(crate) fn persist_root_attempt_op(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
    op: u64,
) -> Result<(), ExecError> {
    handle
        .upsert_memory_fact(
            ROOT_ATTEMPT_FACT_KIND,
            &root_attempt_op_key(run_id),
            &op.to_string(),
        )
        .map_err(|e| ExecError::Internal(format!("verification attempt fact write: {e}")))
}

pub(crate) fn persist_root_verification_fact(
    handle: &faktor_session::SessionHandle,
    status: &str,
    checks: &[faktor_core::state::CheckExecution],
    changed: &[String],
) -> Result<(), ExecError> {
    let last = serde_json::json!({
        "status": status,
        "checks": checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.check,
                    "passed": c.status == VerificationStatus::Passed,
                })
            })
            .collect::<Vec<_>>(),
        "changed": changed,
    });
    handle
        .upsert_memory_fact("verification", "last", &last.to_string())
        .map(|_| ())
        .map_err(|e| ExecError::Internal(format!("root verification fact write: {}", e.message)))
}

/// The canonical acceptance-criteria contract digest: the ordered criteria
/// are length-prefixed fields (with their exact count), so two contracts can
/// never alias by concatenation and no 64-bit FNV fold remains.
pub(crate) fn acceptance_criteria_authority_digest(criteria: &[String]) -> String {
    authority_digest_hex(
        DOMAIN_TASK_CONTRACT,
        1,
        Fields::new().uint(criteria.len() as u64).list(criteria),
    )
}

/// The canonical changed-file-list digest; an EMPTY list is an honest
/// absence (empty string), exactly the former contract, while a non-empty
/// list is length-prefixed fields with its exact cardinality.
pub(crate) fn changed_files_authority_digest(changed: &[String]) -> String {
    if changed.is_empty() {
        return String::new();
    }
    authority_digest_hex(
        DOMAIN_CHANGED_FILES,
        1,
        Fields::new().uint(changed.len() as u64).list(changed),
    )
}

/// The canonical check-basis digest of the fingerprint's
/// `check_argv_cwd_env_hash`: per check the id, the program and every argv
/// element as its own length-prefixed field (never a joined string). No
/// cwd/env are recorded in this projection, so none participate.
pub(crate) fn check_basis_authority_digest(basis: &ProofBasis) -> String {
    let mut fields = Fields::new().uint(basis.checks.len() as u64);
    for check in &basis.checks {
        fields = fields
            .text(&check.check_id)
            .text(&check.program)
            .list(&check.args);
    }
    authority_digest_hex(DOMAIN_CHECK_BASIS, 1, fields)
}

pub(crate) fn truncate_bytes(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// The production reviewer digest of one root verification run: the reviewer
/// identity of every PASSED reviewer-bearing criterion (independent review /
/// aggregate goal), the ROUTED provider+model of the ACTUAL review call the
/// run rests on ([`faktor_agent::IntegratedRootVerification::review_model_identity`]),
/// and a structured-payload digest per verdict — folded deterministically
/// into one `blake3:` digest. `None` when no reviewer-bearing criterion
/// passed OR when the run records no actual review-model call: an honest
/// absence, never a fabricated reviewer row (the parent session's configured
/// pair is NEVER substituted for the real reviewer).
pub(crate) fn reviewer_proof_basis_digest(
    run: &faktor_agent::IntegratedRootVerification,
) -> Option<String> {
    let mut rows: Vec<String> = Vec::new();
    for verdict in &run.criteria {
        if !verdict.passed {
            continue;
        }
        let identity = match &verdict.binding {
            Some(CriterionBinding::IndependentReview { reviewer_id }) => {
                format!("independent_review|{reviewer_id}")
            }
            Some(CriterionBinding::AggregateGoal) => "aggregate_goal|final-reviewer".to_string(),
            _ => continue,
        };
        rows.push(format!(
            "{}|{}|{}",
            verdict.criterion_key,
            identity,
            evidence_text_digest(verdict.evidence.as_deref())
        ));
    }
    if rows.is_empty() {
        return None;
    }
    // The ACTUAL review call's routed pair — never the parent's configured
    // pair. No recorded identity means no review-model call was attempted:
    // there is no reviewer to name, so the digest is an honest absence.
    let review_identity = run.review_model_identity.as_ref()?;
    rows.sort();
    rows.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-reviewer-basis:v2\0");
    for part in [
        review_identity.provider.as_bytes(),
        review_identity.model.as_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    for row in &rows {
        hasher.update(&(row.len() as u64).to_le_bytes());
        hasher.update(row.as_bytes());
    }
    Some(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// One evidence text folded to a bounded, deterministic digest (the raw
/// evidence may be long; the digest is what the basis commits to).
pub(crate) fn evidence_text_digest(evidence: Option<&str>) -> String {
    match evidence {
        Some(text) => format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex()),
        None => "<none>".to_string(),
    }
}

/// Every IMMUTABLE evidence contributing to a criterion PASS of one
/// verification run: the criterion's binding digest, the bound check's
/// execution digest, file-state/immutable-evidence digests, integration
/// coverage sources and the criterion's own evidence text digest. Sorted,
/// deduplicated and bounded so two identical runs produce the identical
/// list (and thereby the identical basis digest).
pub(crate) fn criterion_pass_evidence_digests(
    prepared: &PreparedRunIntegration,
    run: &faktor_agent::IntegratedRootVerification,
) -> Vec<String> {
    let checks_by_id: HashMap<&str, &CheckExecution> = run
        .checks
        .iter()
        .map(|check| (check.check.as_str(), check))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for verdict in run
        .criteria
        .iter()
        .filter(|verdict| verdict.passed)
        .take(faktor_session::task::MAX_PROOF_BASIS_ENTRIES)
    {
        let key = &verdict.criterion_key;
        if let Some(binding) = &verdict.binding {
            out.push(format!("binding:{key}:{}", binding.content_digest()));
            match binding {
                CriterionBinding::RequiredCheck {
                    check_id,
                    command_digest,
                } => {
                    out.push(format!("command:{key}:{command_digest}"));
                    if let Some(check) = checks_by_id.get(check_id.as_str()) {
                        out.push(format!("check:{key}:{}", check_execution_digest(check)));
                    }
                }
                CriterionBinding::FileState {
                    path,
                    expected_digest,
                } => out.push(format!("file:{key}:{path}:{expected_digest}")),
                CriterionBinding::Evidence {
                    evidence_id,
                    evidence_digest,
                } => out.push(format!("immutable:{key}:{evidence_id}:{evidence_digest}")),
                CriterionBinding::IntegrationCoverage {
                    required_work_items,
                } => {
                    for item in required_work_items {
                        out.push(format!("coverage:{key}:{item}"));
                    }
                    for source in &prepared.sources {
                        out.push(format!(
                            "source:{key}:{}:{}",
                            source.child_id, source.candidate_root_hash
                        ));
                    }
                }
                CriterionBinding::IndependentReview { reviewer_id } => {
                    out.push(format!("review:{key}:{reviewer_id}"));
                }
                CriterionBinding::AggregateGoal => {
                    out.push(format!("review:{key}:aggregate-goal"));
                }
                CriterionBinding::Unavailable { .. } => {}
            }
        }
        if let Some(evidence) = &verdict.evidence {
            out.push(format!(
                "evidence:{key}:blake3:{}",
                blake3::hash(evidence.as_bytes()).to_hex()
            ));
        }
    }
    out.sort();
    out.dedup();
    out.truncate(faktor_session::task::MAX_PROOF_BASIS_ENTRIES);
    out
}

/// The deterministic digest of one executed check row (the immutable
/// evidence a check-bound criterion pass resolves to).
pub(crate) fn check_execution_digest(check: &CheckExecution) -> String {
    let status = format!("{:?}", check.status);
    let exit = check.exit.map(|code| code.to_string());
    authority_digest_hex(
        DOMAIN_CHECK_EXECUTION,
        1,
        Fields::new()
            .text(&check.check)
            .text(&check.program)
            .list(&check.args)
            .text(&status)
            .opt_text(exit.as_deref()),
    )
}
