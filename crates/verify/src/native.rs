//! @authority-collapse: authority-bearing
//!
//! The NATIVE verification provider: a real baseline over the existing
//! verification machinery (`exec.rs`, `derive.rs`, `inventory.rs`).
//!
//! Mapping table (op → existing machinery):
//!
//! | [`VerificationOp`] | native machinery |
//! |---|---|
//! | `Build` | derived `Compile` checks (`cargo check`, `tsc --noEmit`, `mvn compile`, cmake/make/meson builds, ...) |
//! | `Typecheck` | derived `Compile` checks (for Rust `cargo check` IS the typecheck; other families' compile checks are their type gates) |
//! | `Lint` | derived `Lint` checks plus a root-aware `cargo clippy` spec for every Cargo component owning a selected path |
//! | `RunTests` | derived `Test` checks (`cargo test`, `npm test`, `pytest`, ...) |
//! | `RunGeneratedTests` | NOT derivable natively (honest `Unavailable`; the provider that generated the tests must run them) |
//! | `CheckContracts` / `CheckCompatibility` | NOT derivable natively (honest `Unavailable`) |
//! | `RunSecurityChecks` / `RunPerformanceChecks` / `RunRuntimeChecks` | NOT derivable natively (honest `Unavailable`) |
//! | `VerifyClaim` | NOT derivable natively (honest `Unavailable`: the native baseline holds no claim semantics) |
//!
//! Every check executes through the workspace's ONE
//! [`faktor_terminal::ProcessSupervisor`] via [`AsyncCheckExecutor`] (typed
//! `(program, argv)` specs, never `sh -c`), against the exact task-owned
//! worktree resolved by [`VerificationRootResolver`], under the request
//! deadline and cancellation token, with bounded output. Inventory discovery
//! uses the same supervisor and the same completeness discipline: ANY
//! non-Complete inventory refuses a `Passed` verdict.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_core::contract::{ClaimState, ProofBasis, ProofEvidenceKind, ProofScope};
use faktor_core::{SessionId, WorkspaceId};

use crate::claims::{
    evidence_binding_for_check, ClaimVerificationResult, EvidenceStrength, MAX_COUNTEREXAMPLES,
};
use crate::derive::{
    component_id, derive_checks, detect_project_profile, BuildSystem, ProjectProfile,
};
use crate::exec::{
    AsyncCheckExecutor, CheckCategory, CheckKind, CheckRunStatus, CheckSpec, VerificationContext,
};
use crate::inventory::{discover_repo_inventory_with_supervisor, InventoryBudget};
use crate::provider::{
    now_epoch_ms, tail_bytes, ArtifactRef, BoxFuture, VerificationCapabilities, VerificationError,
    VerificationOp, VerificationOutcome, VerificationProvider, VerificationProviderId,
    VerificationRequest, VerificationResponse, VerificationTarget, MAX_SUMMARY_BYTES,
};

/// Default aggregate wall budget of one native verification call when the
/// request carries no deadline.
pub const DEFAULT_NATIVE_CHECK_WALL: Duration = Duration::from_secs(600);
/// Default inventory discovery budget of one native verification call.
pub const DEFAULT_NATIVE_INVENTORY_WALL: Duration = Duration::from_secs(120);
/// Bound on one evidence detail derived from a check summary.
const EVIDENCE_DETAIL_BYTES: usize = 2048;

/// The native provider's workspace-root authority: resolves the bounded,
/// task-owned worktree root for one (session, workspace) at request time.
/// The daemon wires its ONE workspace table here; single-root hosts and
/// tests wire [`FixedRootResolver`].
pub trait VerificationRootResolver: Send + Sync {
    fn resolve_root(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<PathBuf, VerificationError>;
}

impl<F> VerificationRootResolver for F
where
    F: Fn(SessionId, WorkspaceId) -> Result<PathBuf, VerificationError> + Send + Sync,
{
    fn resolve_root(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<PathBuf, VerificationError> {
        self(session_id, workspace_id)
    }
}

/// A fixed-root resolver (single-root hosts and tests).
pub struct FixedRootResolver {
    root: PathBuf,
}

impl FixedRootResolver {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl VerificationRootResolver for FixedRootResolver {
    fn resolve_root(
        &self,
        _session_id: SessionId,
        _workspace_id: WorkspaceId,
    ) -> Result<PathBuf, VerificationError> {
        Ok(self.root.clone())
    }
}

/// The operations the native baseline serves. Everything else is an
/// honest capability boundary (see the module table).
pub fn native_capabilities() -> VerificationCapabilities {
    VerificationCapabilities::of(VerificationOp::Build)
        .union(VerificationCapabilities::of(VerificationOp::Typecheck))
        .union(VerificationCapabilities::of(VerificationOp::Lint))
        .union(VerificationCapabilities::of(VerificationOp::RunTests))
}

/// The in-process native verification baseline.
pub struct NativeVerificationProvider {
    executor: Arc<AsyncCheckExecutor>,
    roots: Arc<dyn VerificationRootResolver>,
    max_check_wall: Duration,
    inventory_wall: Duration,
}

impl NativeVerificationProvider {
    pub fn new(
        executor: Arc<AsyncCheckExecutor>,
        roots: Arc<dyn VerificationRootResolver>,
    ) -> Self {
        Self {
            executor,
            roots,
            max_check_wall: DEFAULT_NATIVE_CHECK_WALL,
            inventory_wall: DEFAULT_NATIVE_INVENTORY_WALL,
        }
    }

    /// Bound every check's aggregate wall budget (default
    /// [`DEFAULT_NATIVE_CHECK_WALL`]).
    pub fn with_max_check_wall(mut self, wall: Duration) -> Self {
        self.max_check_wall = wall;
        self
    }

    /// Bound inventory discovery (default [`DEFAULT_NATIVE_INVENTORY_WALL`]).
    pub fn with_inventory_wall(mut self, wall: Duration) -> Self {
        self.inventory_wall = wall;
        self
    }

    async fn run(
        &self,
        request: VerificationRequest,
    ) -> Result<VerificationResponse, VerificationError> {
        let started = Instant::now();
        request.validate()?;
        if request.cancellation.is_cancelled() {
            return Err(VerificationError::Timeout(
                "native verification cancelled before start".to_string(),
            ));
        }
        let now_ms = now_epoch_ms();
        if request.is_expired(now_ms) {
            return Err(VerificationError::Timeout(
                "native verification deadline expired before start".to_string(),
            ));
        }
        if !native_capabilities().supports(request.op) {
            return finish(
                &request,
                unavailable(&request, started, native_boundary_message(request.op)),
            );
        }

        let root = match self
            .roots
            .resolve_root(request.session_id, request.workspace_id)
        {
            Ok(root) => root,
            Err(err) => {
                return finish(
                    &request,
                    unavailable(
                        &request,
                        started,
                        format!("native verification root unavailable: {err}"),
                    ),
                )
            }
        };
        match std::fs::metadata(&root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return finish(
                    &request,
                    unavailable(
                        &request,
                        started,
                        format!(
                            "native verification root {} is not a directory",
                            root.display()
                        ),
                    ),
                )
            }
            Err(error) => {
                return finish(
                    &request,
                    unavailable(
                        &request,
                        started,
                        format!(
                            "native verification root {} is not readable: {error}",
                            root.display()
                        ),
                    ),
                )
            }
        }

        let remaining = request
            .deadline_ms
            .map(|deadline_ms| Duration::from_millis((deadline_ms - now_epoch_ms()).max(1) as u64));
        let inventory_wall = remaining
            .map(|remaining| self.inventory_wall.min(remaining))
            .unwrap_or(self.inventory_wall);
        let budget = InventoryBudget {
            max_wall: inventory_wall,
            ..InventoryBudget::default()
        };
        let inventory =
            discover_repo_inventory_with_supervisor(&root, &budget, self.executor.supervisor());
        if let Some(reason) = inventory.refusal_reason() {
            return finish(
                &request,
                unavailable(
                    &request,
                    started,
                    format!("native verification cannot certify the repository: {reason}"),
                ),
            );
        }
        let profile = detect_project_profile(&root, &inventory.files);
        let selection: Vec<PathBuf> = match &request.target {
            VerificationTarget::Workspace => inventory.files.iter().map(PathBuf::from).collect(),
            VerificationTarget::Paths(paths) => paths.iter().map(PathBuf::from).collect(),
        };
        let derived = match derive_checks(&profile, &selection) {
            Ok(checks) => checks,
            Err(error) => {
                return finish(
                    &request,
                    unavailable(
                        &request,
                        started,
                        format!("native check derivation refused: {error}"),
                    ),
                )
            }
        };
        let specs = select_specs(request.op, derived, &profile, &selection);
        if specs.is_empty() {
            return finish(
                &request,
                unavailable(
                    &request,
                    started,
                    format!(
                        "native verification derived no {} checks for this target",
                        request.op.as_str()
                    ),
                ),
            );
        }

        let check_wall = remaining
            .map(|remaining| self.max_check_wall.min(remaining))
            .unwrap_or(self.max_check_wall);
        let ctx = VerificationContext {
            session_id: request.session_id.raw(),
            // The op envelope carries no task/worktree identity yet; the
            // context labels stay unset (the executor uses them for owner
            // attribution only).
            task_id: 0,
            operation_id: request.op_id.raw(),
            workspace_id: request.workspace_id.raw(),
            worktree_id: 0,
            root: root.clone(),
            deadline: Instant::now() + check_wall,
            cancellation: request.cancellation.clone(),
        };

        let mut executed: Vec<ExecutedCheck> = Vec::with_capacity(specs.len());
        for spec in &specs {
            if request.cancellation.is_cancelled() {
                executed.push(ExecutedCheck {
                    id: spec.id.clone(),
                    kind: spec.kind,
                    status: CheckRunStatus::Unavailable,
                    exit: None,
                    detail: Some("cancelled before this check started".to_string()),
                });
                break;
            }
            match self.executor.run_check(spec, &ctx).await {
                Ok(outcome) => executed.push(ExecutedCheck {
                    id: spec.id.clone(),
                    kind: spec.kind,
                    status: outcome.status,
                    exit: outcome.exit,
                    detail: outcome.summary,
                }),
                Err(error) => {
                    return finish(
                        &request,
                        unavailable(
                            &request,
                            started,
                            format!("native check {} could not run: {error}", spec.id),
                        ),
                    )
                }
            }
        }

        // Claim batch (the completion spine): answer EVERY claim with an
        // explicit per-claim verdict derived from the claim-bound checks this
        // op executed — never a silent omission, never a fabricated verdict.
        if !request.claims.is_empty() {
            return finish(
                &request,
                aggregate_claims(&request, started, &specs, &executed),
            );
        }

        finish(&request, aggregate(&request, started, &executed))
    }
}

impl VerificationProvider for NativeVerificationProvider {
    fn id(&self) -> VerificationProviderId {
        VerificationProviderId::native()
    }

    fn is_native(&self) -> bool {
        true
    }

    fn capabilities(&self) -> VerificationCapabilities {
        native_capabilities()
    }

    fn verify(
        &self,
        request: VerificationRequest,
    ) -> BoxFuture<'_, Result<VerificationResponse, VerificationError>> {
        Box::pin(async move { self.run(request).await })
    }
}

/// One executed (or skipped) check as the aggregate consumes it.
struct ExecutedCheck {
    id: String,
    kind: CheckKind,
    status: CheckRunStatus,
    exit: Option<i32>,
    detail: Option<String>,
}

/// Select the derived specs one op runs (see the module mapping table).
fn select_specs(
    op: VerificationOp,
    derived: Vec<CheckSpec>,
    profile: &ProjectProfile,
    selection: &[PathBuf],
) -> Vec<CheckSpec> {
    match op {
        VerificationOp::Build | VerificationOp::Typecheck => derived
            .into_iter()
            .filter(|spec| spec.kind == CheckKind::Compile)
            .collect(),
        VerificationOp::RunTests => derived
            .into_iter()
            .filter(|spec| spec.kind == CheckKind::Test)
            .collect(),
        VerificationOp::Lint => {
            let mut specs: Vec<CheckSpec> = derived
                .into_iter()
                .filter(|spec| spec.kind == CheckKind::Lint)
                .collect();
            specs.extend(lint_specs(profile, selection));
            specs
        }
        _ => Vec::new(),
    }
}

/// The lint specs natively derivable: `cargo clippy` for every Cargo
/// component that owns at least one selected path. Non-Rust components have
/// no natively derivable lint check here (a configured linter is not
/// guessed); the caller sees an honest `Unavailable` for those targets.
fn lint_specs(profile: &ProjectProfile, selection: &[PathBuf]) -> Vec<CheckSpec> {
    let mut owners: BTreeSet<PathBuf> = BTreeSet::new();
    for file in selection {
        if let Some(component) = profile.owning_component(file) {
            if component.build_systems.contains(&BuildSystem::Cargo) {
                owners.insert(component.root.clone());
            }
        }
    }
    let mut specs = Vec::new();
    for component in &profile.components {
        if !owners.contains(&component.root) {
            continue;
        }
        specs.push(CheckSpec {
            id: component_id(component, "rust_clippy"),
            kind: CheckKind::Lint,
            category: CheckCategory::Quick,
            program: "cargo".into(),
            args: vec!["clippy".into()],
            cwd_rel: component.root.clone(),
            affects: Vec::new(),
            required: true,
        });
    }
    specs
}

/// One claim-bound native verdict derived from the checks this op executed.
/// Matching is structural: the claim's evidence kind must equal the check's
/// taxonomy kind and the claim's binding must intersect the check's binding
/// (its id, plus its test selectors). Unmatched or unavailable checks are an
/// explicit `Unknown`, never a fabricated verdict.
fn adjudicate_claim(
    claim: &crate::claims::ClaimRequest,
    request: &VerificationRequest,
    specs: &[CheckSpec],
    executed: &[ExecutedCheck],
) -> ClaimVerificationResult {
    let mut evidence: Vec<ArtifactRef> = Vec::new();
    let mut counterexamples: Vec<String> = Vec::new();
    let mut saw_pass = false;
    let mut saw_fail = false;
    for spec in specs {
        let Some(run) = executed.iter().find(|run| run.id == spec.id) else {
            continue;
        };
        if evidence_kind(spec.kind) != claim.evidence_kind {
            continue;
        }
        let program = spec.program.to_string_lossy();
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let bindings =
            evidence_binding_for_check(&spec.id, &program, &args, spec.kind == CheckKind::Test);
        let matched = !claim.binding.is_bound()
            || bindings
                .iter()
                .any(|binding| claim.binding.intersects(binding));
        if !matched {
            continue;
        }
        match run.status {
            CheckRunStatus::Passed => saw_pass = true,
            CheckRunStatus::Failed => {
                saw_fail = true;
                if counterexamples.len() < MAX_COUNTEREXAMPLES {
                    let detail = run.detail.as_deref().unwrap_or("check failed");
                    counterexamples.push(tail_bytes(detail, EVIDENCE_DETAIL_BYTES));
                }
            }
            CheckRunStatus::Unavailable => continue,
        }
        let detail = run
            .detail
            .as_deref()
            .map(|detail| tail_bytes(detail, EVIDENCE_DETAIL_BYTES))
            .unwrap_or_else(|| match run.status {
                CheckRunStatus::Passed => "passed".to_string(),
                CheckRunStatus::Failed => "failed".to_string(),
                CheckRunStatus::Unavailable => "unavailable".to_string(),
            });
        evidence.push(ArtifactRef {
            kind: evidence_kind(spec.kind),
            source: run.id.clone(),
            detail,
        });
    }
    let (state, strength) = if saw_fail {
        (ClaimState::Disproven, EvidenceStrength::Observed)
    } else if saw_pass {
        (ClaimState::Supported, EvidenceStrength::Observed)
    } else {
        (ClaimState::Unknown, EvidenceStrength::None)
    };
    let mut result = ClaimVerificationResult::verdict(
        VerificationProviderId::native(),
        claim.obligation_id.clone(),
        state,
        strength,
        evidence,
        counterexamples,
    );
    result.basis = ProofBasis {
        subject: request.subject_snapshot.clone(),
        scope: ProofScope::SnapshotBound,
    };
    if let Some(snapshot) = &request.subject_snapshot {
        result.subject_snapshot = Some(snapshot.clone());
    }
    result
}

/// Aggregate the per-claim verdicts of a claim batch: one result per claim,
/// `Failed` when any claim is disproven, `Passed` only when every claim is
/// adjudicated, `Unavailable` otherwise (honest partial — the explicit
/// per-claim states still ride the response).
fn aggregate_claims(
    request: &VerificationRequest,
    started: Instant,
    specs: &[CheckSpec],
    executed: &[ExecutedCheck],
) -> VerificationResponse {
    let claims: Vec<ClaimVerificationResult> = request
        .claims
        .iter()
        .map(|claim| adjudicate_claim(claim, request, specs, executed))
        .collect();
    let disproven = claims.iter().filter(|c| c.state.is_disproven()).count();
    let adjudicated = claims
        .iter()
        .filter(|c| c.state != ClaimState::Unknown)
        .count();
    let outcome = if disproven > 0 {
        VerificationOutcome::Failed
    } else if adjudicated == claims.len() {
        VerificationOutcome::Passed
    } else {
        VerificationOutcome::Unavailable
    };
    let summary = format!(
        "native {} claim batch: {} claim(s): {} adjudicated, {} disproven, {} explicitly unknown",
        request.op.as_str(),
        claims.len(),
        adjudicated,
        disproven,
        claims.len() - adjudicated
    );
    let mut response =
        VerificationResponse::new(VerificationProviderId::native(), request.op, outcome)
            .with_summary(summary)
            .with_claims(claims)
            .with_duration_ms(started.elapsed().as_millis() as u64);
    response.summary = tail_bytes(
        &response.summary,
        MAX_SUMMARY_BYTES.min(request.max_output_bytes),
    );
    response
}

/// Aggregate executed checks into the bounded response.
fn aggregate(
    request: &VerificationRequest,
    started: Instant,
    executed: &[ExecutedCheck],
) -> VerificationResponse {
    let mut passed = 0usize;
    let mut failed_count = 0usize;
    let mut unavailable_count = 0usize;
    let mut first_failure: Option<(String, Option<i32>)> = None;
    for check in executed {
        match check.status {
            CheckRunStatus::Passed => passed += 1,
            CheckRunStatus::Failed => {
                failed_count += 1;
                if first_failure.is_none() {
                    first_failure = Some((check.id.clone(), check.exit));
                }
            }
            CheckRunStatus::Unavailable => unavailable_count += 1,
        }
    }
    let outcome = if failed_count > 0 {
        VerificationOutcome::Failed
    } else if unavailable_count > 0 {
        VerificationOutcome::Unavailable
    } else {
        VerificationOutcome::Passed
    };
    let exit_code = match (&first_failure, outcome) {
        (Some((_, exit)), _) => exit.or(Some(1)),
        (None, VerificationOutcome::Passed) => Some(0),
        (None, _) => None,
    };

    let mut summary = format!(
        "native {} verification: {} check(s): {} passed, {} failed, {} unavailable",
        request.op.as_str(),
        executed.len(),
        passed,
        failed_count,
        unavailable_count
    );
    for check in executed {
        let status = match check.status {
            CheckRunStatus::Passed => "passed",
            CheckRunStatus::Failed => "failed",
            CheckRunStatus::Unavailable => "unavailable",
        };
        summary.push_str(&format!("\n{}: {status}", check.id));
        if let Some(exit) = check.exit {
            summary.push_str(&format!(" (exit {exit})"));
        }
        if let Some(detail) = &check.detail {
            if check.status != CheckRunStatus::Passed {
                summary.push_str(": ");
                summary.push_str(&tail_bytes(detail, EVIDENCE_DETAIL_BYTES));
            }
        }
    }

    let evidence: Vec<ArtifactRef> = executed
        .iter()
        .map(|check| {
            let detail = check
                .detail
                .as_deref()
                .map(|detail| tail_bytes(detail, EVIDENCE_DETAIL_BYTES))
                .unwrap_or_else(|| match check.status {
                    CheckRunStatus::Passed => "passed".to_string(),
                    CheckRunStatus::Failed => "failed".to_string(),
                    CheckRunStatus::Unavailable => "unavailable".to_string(),
                });
            ArtifactRef {
                kind: evidence_kind(check.kind),
                source: check.id.clone(),
                detail,
            }
        })
        .collect();

    let mut response =
        VerificationResponse::new(VerificationProviderId::native(), request.op, outcome)
            .with_exit_code(exit_code)
            .with_summary(summary)
            .with_evidence(evidence)
            .with_duration_ms(started.elapsed().as_millis() as u64);
    // The summary already carries bounded per-check tails; keep it within
    // the interface bound before fitting the total output.
    response.summary = tail_bytes(
        &response.summary,
        MAX_SUMMARY_BYTES.min(request.max_output_bytes),
    );
    response
}

/// An honest capability-boundary response.
fn unavailable(
    request: &VerificationRequest,
    started: Instant,
    summary: impl Into<String>,
) -> VerificationResponse {
    VerificationResponse::new(
        VerificationProviderId::native(),
        request.op,
        VerificationOutcome::Unavailable,
    )
    .with_summary(summary)
    .with_duration_ms(started.elapsed().as_millis() as u64)
}

/// Fit and validate one response before it leaves the provider.
fn finish(
    request: &VerificationRequest,
    mut response: VerificationResponse,
) -> Result<VerificationResponse, VerificationError> {
    response.fit_output(request.max_output_bytes);
    response.validate(request)?;
    Ok(response)
}

fn native_boundary_message(op: VerificationOp) -> String {
    match op {
        VerificationOp::VerifyClaim => {
            "native verification cannot evaluate arbitrary semantic claims: the native baseline \
             holds no claim semantics; configure an external verification provider (Kiwi) for \
             claim verification"
                .to_string()
        }
        VerificationOp::RunGeneratedTests => {
            "native verification holds no generated-test inventory: generated tests must be run \
             by the provider that generated them; an external verification provider is required"
                .to_string()
        }
        VerificationOp::CheckContracts | VerificationOp::CheckCompatibility => {
            "native verification derives compile/test/lint checks only; contract and \
             compatibility checks require an external verification provider"
                .to_string()
        }
        VerificationOp::RunSecurityChecks
        | VerificationOp::RunPerformanceChecks
        | VerificationOp::RunRuntimeChecks => format!(
            "native verification derives compile/test/lint checks only; {} requires an external \
             verification provider",
            op.as_str()
        ),
        _ => "native verification does not serve this operation".to_string(),
    }
}

fn evidence_kind(kind: CheckKind) -> ProofEvidenceKind {
    match kind {
        // Lint is static source analysis: it discharges a compile-kind
        // obligation in the frozen taxonomy (there is no lint kind).
        CheckKind::Compile | CheckKind::Lint => ProofEvidenceKind::Compile,
        CheckKind::Test => ProofEvidenceKind::TestExecution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::{CancellationToken, OpId};
    use std::fs;
    use std::path::Path;

    fn cargo_fixture(name: &str, lib_rs: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), lib_rs).unwrap();
        dir
    }

    fn provider(root: &Path) -> NativeVerificationProvider {
        let executor = Arc::new(AsyncCheckExecutor::try_shared().expect("test supervisor"));
        NativeVerificationProvider::new(executor, Arc::new(FixedRootResolver::new(root)))
    }

    fn request(op: VerificationOp) -> VerificationRequest {
        VerificationRequest::new(
            OpId::new(42),
            SessionId::new(1),
            WorkspaceId::new(1),
            now_epoch_ms(),
            CancellationToken::new(),
            op,
        )
    }

    #[tokio::test]
    async fn native_build_passes_on_a_passing_cargo_fixture() {
        let dir = cargo_fixture(
            "verify_native_ok",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::Build))
            .await
            .expect("build verification");
        assert_eq!(
            response.outcome,
            VerificationOutcome::Passed,
            "{response:?}"
        );
        assert_eq!(response.provider.as_str(), "native");
        assert_eq!(response.exit_code, Some(0));
        assert!(
            response
                .evidence
                .iter()
                .any(|artifact| artifact.source == "rust_check"),
            "the derived cargo check must be the evidence: {:?}",
            response.evidence
        );
    }

    #[tokio::test]
    async fn native_run_tests_passes_on_a_passing_cargo_fixture() {
        let dir = cargo_fixture(
            "verify_native_tests_ok",
            "#[test]\nfn it_works() {\n    assert_eq!(2 + 2, 4);\n}\n",
        );
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::RunTests))
            .await
            .expect("test verification");
        assert_eq!(
            response.outcome,
            VerificationOutcome::Passed,
            "{response:?}"
        );
        assert_eq!(response.exit_code, Some(0));
        assert!(
            response
                .evidence
                .iter()
                .any(|artifact| artifact.source.contains("rust_test")),
            "{:?}",
            response.evidence
        );
    }

    #[tokio::test]
    async fn native_build_fails_on_a_broken_cargo_fixture() {
        let dir = cargo_fixture(
            "verify_native_broken",
            "pub fn broken() -> i32 {\n    nope\n}\n",
        );
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::Build))
            .await
            .expect("build verification");
        assert_eq!(
            response.outcome,
            VerificationOutcome::Failed,
            "{response:?}"
        );
        assert!(
            response.exit_code.is_some_and(|exit| exit != 0),
            "a real failure carries a non-zero exit: {:?}",
            response.exit_code
        );
        assert!(
            response.summary.contains("rust_check"),
            "{}",
            response.summary
        );
    }

    #[tokio::test]
    async fn native_run_tests_fails_on_a_failing_test() {
        let dir = cargo_fixture(
            "verify_native_tests_bad",
            "#[test]\nfn nope() {\n    assert_eq!(1, 2);\n}\n",
        );
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::RunTests))
            .await
            .expect("test verification");
        assert_eq!(
            response.outcome,
            VerificationOutcome::Failed,
            "{response:?}"
        );
        assert!(response.exit_code.is_some_and(|exit| exit != 0));
    }

    #[tokio::test]
    async fn native_lint_runs_clippy_for_cargo_components() {
        let dir = cargo_fixture(
            "verify_native_lint",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::Lint))
            .await
            .expect("lint verification");
        assert!(
            response
                .evidence
                .iter()
                .any(|artifact| artifact.source.contains("rust_clippy")),
            "the derived clippy check must be selected: {:?}",
            response.evidence
        );
        // CI always carries clippy; a host without it must degrade HONESTLY
        // (Unavailable naming the missing tool), never pass silently.
        match response.outcome {
            VerificationOutcome::Passed => {}
            VerificationOutcome::Unavailable => {
                assert!(
                    response.summary.contains("rust_clippy"),
                    "{}",
                    response.summary
                );
            }
            other => panic!("lint must pass or be honestly unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn native_lint_is_honestly_unavailable_without_a_derivable_linter() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{\"name\":\"web\"}").unwrap();
        fs::write(dir.path().join("app.js"), "console.log(1);\n").unwrap();
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::Lint))
            .await
            .expect("lint verification");
        assert_eq!(
            response.outcome,
            VerificationOutcome::Unavailable,
            "no natively derivable lint check exists for this target: {response:?}"
        );
        assert!(response.summary.contains("no"), "{}", response.summary);
    }

    #[tokio::test]
    async fn native_verify_claim_is_honestly_unavailable() {
        let dir = cargo_fixture("verify_native_claim", "pub fn f() {}\n");
        let provider = provider(dir.path());
        let response = provider
            .verify(request(VerificationOp::VerifyClaim).with_claim("replayed tokens fail"))
            .await
            .expect("claim verification returns an honest response");
        assert_eq!(response.outcome, VerificationOutcome::Unavailable);
        assert!(
            response
                .summary
                .contains("cannot evaluate arbitrary semantic claims"),
            "{}",
            response.summary
        );
        assert!(response.evidence.is_empty());
        assert_eq!(response.exit_code, None);
    }

    #[tokio::test]
    async fn native_hostile_bounds_expiry_and_targets_are_refused_typed() {
        let dir = cargo_fixture("verify_native_bounds", "pub fn f() {}\n");
        let provider = provider(dir.path());

        let mut zero = request(VerificationOp::Build);
        zero.max_output_bytes = 0;
        assert!(matches!(
            provider.verify(zero).await,
            Err(VerificationError::Malformed(_))
        ));

        let mut over = request(VerificationOp::Build);
        over.max_output_bytes = crate::provider::MAX_OUTPUT_BYTES + 1;
        assert!(matches!(
            provider.verify(over).await,
            Err(VerificationError::OutputTooLarge { .. })
        ));

        let hostile = VerificationTarget::Paths(vec!["../escape.rs".to_string()]);
        let err = provider
            .verify(request(VerificationOp::Build).with_target(hostile))
            .await
            .expect_err("hostile targets must be refused before any process");
        assert!(matches!(err, VerificationError::Malformed(_)), "{err:?}");

        let now = now_epoch_ms();
        let mut expired = request(VerificationOp::Build);
        expired.created_ms = now - 10;
        expired.deadline_ms = Some(now - 1);
        let err = provider
            .verify(expired)
            .await
            .expect_err("an expired deadline is terminal");
        assert!(matches!(err, VerificationError::Timeout(_)), "{err:?}");
    }

    #[tokio::test]
    async fn native_selection_respects_the_bounded_target() {
        // A path target that names no Rust file derives no compile check;
        // the response is honestly Unavailable instead of building anyway.
        let dir = cargo_fixture("verify_native_target", "pub fn f() {}\n");
        fs::write(dir.path().join("README.md"), "# docs\n").unwrap();
        let provider = provider(dir.path());
        let target = VerificationTarget::paths(["README.md"]).unwrap();
        let response = provider
            .verify(request(VerificationOp::Build).with_target(target))
            .await
            .expect("target-scoped verification");
        assert_eq!(
            response.outcome,
            VerificationOutcome::Unavailable,
            "{response:?}"
        );
        assert!(response.summary.contains("derived no build checks"));
    }

    #[tokio::test]
    async fn native_claim_batch_is_answered_per_claim_never_omitted() {
        use crate::claims::ClaimRequest;
        use faktor_core::contract::{AcceptedStrength, EvidenceBinding, Requirement};

        let claim = |id: &str, check_id: &str| ClaimRequest {
            obligation_id: id.to_string(),
            claim: format!("claim {id}"),
            evidence_kind: ProofEvidenceKind::Compile,
            requirement: Requirement::Required,
            binding: EvidenceBinding::CheckIds(vec![check_id.to_string()]),
            accepted_strength: AcceptedStrength::Supported,
        };
        let dir = cargo_fixture("verify_native_claims", "pub fn f() {}\n");
        let ok_provider = provider(dir.path());
        let batch = request(VerificationOp::Build).with_claims(vec![
            claim("po1:builds", "rust_check"),
            claim("po1:unrelated", "check_that_never_ran"),
        ]);
        let response = ok_provider.verify(batch).await.expect("claim batch");
        // One explicit result per claim; the batch is honestly Unavailable
        // overall because one claim could not be adjudicated.
        assert_eq!(
            response.outcome,
            VerificationOutcome::Unavailable,
            "{response:?}"
        );
        assert_eq!(response.claims.len(), 2, "{:?}", response.claims);
        let builds = response
            .claims
            .iter()
            .find(|claim| claim.obligation_id == "po1:builds")
            .unwrap();
        assert_eq!(builds.state, ClaimState::Supported);
        assert!(builds.evidence.iter().any(|e| e.source == "rust_check"));
        let unrelated = response
            .claims
            .iter()
            .find(|claim| claim.obligation_id == "po1:unrelated")
            .unwrap();
        assert_eq!(unrelated.state, ClaimState::Unknown);
        assert!(unrelated.evidence.is_empty());
        assert_eq!(unrelated.evidence_strength, EvidenceStrength::None);

        // The broken fixture disproves the same claim with a bounded
        // counterexample.
        let broken = cargo_fixture("verify_native_claims_bad", "pub fn f() -> i32 { nope }\n");
        let broken_provider = provider(broken.path());
        let batch =
            request(VerificationOp::Build).with_claims(vec![claim("po1:builds", "rust_check")]);
        let response = broken_provider.verify(batch).await.expect("claim batch");
        assert_eq!(response.outcome, VerificationOutcome::Failed);
        assert_eq!(response.claims[0].state, ClaimState::Disproven);
        assert_eq!(
            response.claims[0].evidence_strength,
            EvidenceStrength::Observed
        );
        assert!(!response.claims[0].counterexample.is_empty());
    }
}
