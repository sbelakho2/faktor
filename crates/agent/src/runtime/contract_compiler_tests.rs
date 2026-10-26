// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! `runtime::contract_compiler_tests`: adversarial covers of the drive-start
//! baseline contract compilation (audit item 5) — the bounded scanner, the
//! production merge through the durable task row, the idempotent replay and
//! the uncertainty-only contract when nothing is derivable.

use super::*;
use crate::runtime::contract_compiler::scan_repository_snapshot;
use crate::runtime::tests::*;
use crate::*;

use faktor_core::contract_compile::{
    compile_contract, intent_tokens, AcquisitionGapKind, ContractCompileRequest, FindingsRequest,
    FindingsResponse, IntelligenceFuture, IntelligenceQuestion, ProviderFinding,
    RepositoryAcquisition, SystemIntelligenceProvider,
};

const REFRESH_SOURCE: &str = r#"
use std::sync::Mutex;

pub struct RefreshStore {
    lock: Mutex<u64>,
}

pub fn refresh(store: &RefreshStore) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_rejected() {
        assert!(true);
    }

    #[tokio::test]
    async fn concurrent_refresh_yields_one_token() {}
}
"#;

fn write_refresh_repo(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"auth\"\n").unwrap();
    std::fs::write(root.join("src/auth/refresh.rs"), REFRESH_SOURCE).unwrap();
    std::fs::write(
        root.join("tests/refresh_tokens.rs"),
        "#[test]\nfn replayed_refresh_token_fails() {}\n",
    )
    .unwrap();
}

fn selector_targets(obligation: &faktor_core::ProofObligation) -> Vec<String> {
    match &obligation.evidence_binding {
        faktor_core::EvidenceBinding::TestSelector(selector) => vec![selector.clone()],
        faktor_core::EvidenceBinding::TestSelectors(selectors) => selectors.clone(),
        _ => Vec::new(),
    }
}

#[test]
fn scanner_is_bounded_deterministic_and_skips_build_output() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write_refresh_repo(root);
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("target/ignored.rs"), "// build output\n").unwrap();
    std::fs::write(root.join("src/unrelated.rs"), "pub fn other() {}\n").unwrap();

    let tokens = intent_tokens("fix the refresh token race");
    let first = scan_repository_snapshot(root, &tokens);
    let second = scan_repository_snapshot(root, &tokens);
    assert_eq!(
        first, second,
        "two scans of an unchanged tree are identical"
    );
    assert_eq!(first.acquisition, RepositoryAcquisition::Complete);
    assert!(!first.files.iter().any(|file| file.path.contains("target/")));
    let paths: Vec<&String> = first.files.iter().map(|file| &file.path).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "the snapshot is path-sorted");
    let refresh = first
        .files
        .iter()
        .find(|file| file.path == "src/auth/refresh.rs")
        .unwrap();
    assert!(
        refresh.content.is_some(),
        "an intent-matched source file is read"
    );
    let manifest = first
        .files
        .iter()
        .find(|file| file.path == "Cargo.toml")
        .unwrap();
    assert!(manifest.content.is_some(), "manifests are always read");
    let unrelated = first
        .files
        .iter()
        .find(|file| file.path == "src/unrelated.rs")
        .unwrap();
    assert!(
        unrelated.content.is_some(),
        "an eligible source inside the read budget is read in priority order"
    );
    // An unreadable root is UNAVAILABLE with a typed reason, never an empty
    // complete snapshot: "verified there is nothing" is not "could not read".
    let missing = scan_repository_snapshot(std::path::Path::new("/nonexistent-kp-root"), &[]);
    match &missing.acquisition {
        RepositoryAcquisition::Unavailable { reason } => {
            assert!(
                reason.contains("/nonexistent-kp-root"),
                "the typed reason names the root: {reason}"
            );
            assert!(
                reason.contains("could not be read"),
                "the typed reason names the cause: {reason}"
            );
        }
        other => panic!("an unreadable root must be Unavailable, got {other:?}"),
    }
    assert!(missing.files.is_empty());
}

#[test]
fn honest_empty_repository_is_complete_not_unavailable() {
    let dir = tempdir().unwrap();
    let snapshot = scan_repository_snapshot(dir.path(), &[]);
    assert_eq!(
        snapshot.acquisition,
        RepositoryAcquisition::Complete,
        "a readable repository with nothing in it is COMPLETE"
    );
    assert!(snapshot.files.is_empty());
}

#[test]
fn invalid_utf8_selected_file_is_partial_and_compiles_with_uncertainty() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write_refresh_repo(root);
    // A token-matched source selected for content reading that is not UTF-8.
    std::fs::write(
        root.join("src/auth/refresh.rs"),
        [0x66, 0x6e, 0x20, 0xff, 0xfe],
    )
    .unwrap();
    let tokens = intent_tokens("fix the refresh token race");
    let first = scan_repository_snapshot(root, &tokens);
    let second = scan_repository_snapshot(root, &tokens);
    assert_eq!(first, second, "partial scans stay deterministic");
    let gaps = match &first.acquisition {
        RepositoryAcquisition::Partial { reasons } => reasons.clone(),
        other => panic!("invalid UTF-8 must make the snapshot Partial, got {other:?}"),
    };
    let gap = gaps
        .iter()
        .find(|gap| gap.kind == AcquisitionGapKind::InvalidUtf8)
        .expect("a typed invalid-UTF-8 reason");
    assert_eq!(gap.path.as_deref(), Some("src/auth/refresh.rs"));
    assert!(gap.proof_relevant, "a matched source is proof-relevant");

    let compiled = compile_contract(&ContractCompileRequest {
        goal: "Fix the refresh-token race".to_string(),
        repository: first,
    });
    assert!(
        compiled
            .uncertainties
            .iter()
            .any(|uncertainty| uncertainty.statement.contains("partial")),
        "a partial snapshot never compiles to a silent full analysis: {:?}",
        compiled.uncertainties
    );
    assert!(compiled
        .uncertainties
        .iter()
        .any(|uncertainty| uncertainty.basis.contains("src/auth/refresh.rs")));
}

#[cfg(unix)]
#[test]
fn unreadable_selected_file_is_partial_with_typed_reason() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let root = dir.path();
    write_refresh_repo(root);
    let locked = root.join("src/auth/refresh.rs");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root (CI containers) can read through mode 0; the test then has no
    // unreadable file to assert and says so loudly.
    if std::fs::read(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("skipping unreadable-file assertion: running with permission bypass");
        return;
    }
    let tokens = intent_tokens("fix the refresh token race");
    let snapshot = scan_repository_snapshot(root, &tokens);
    let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644));
    match &snapshot.acquisition {
        RepositoryAcquisition::Partial { reasons } => {
            let gap = reasons
                .iter()
                .find(|gap| gap.kind == AcquisitionGapKind::UnreadableFile)
                .expect("a typed unreadable-file reason");
            assert_eq!(gap.path.as_deref(), Some("src/auth/refresh.rs"));
            assert!(gap.proof_relevant, "a matched source is proof-relevant");
            assert!(
                gap.detail.contains("could not be read"),
                "the reason names the cause: {}",
                gap.detail
            );
        }
        other => panic!("an unreadable selected file must be Partial, got {other:?}"),
    }
    let compiled = compile_contract(&ContractCompileRequest {
        goal: "Fix the refresh-token race".to_string(),
        repository: snapshot,
    });
    assert!(
        compiled.uncertainties.iter().any(|uncertainty| {
            uncertainty.statement.contains("partial")
                && uncertainty.basis.contains("src/auth/refresh.rs")
        }),
        "the unread file is surfaced as an uncertainty: {:?}",
        compiled.uncertainties
    );
}

#[tokio::test]
async fn drive_start_compiles_persists_and_replays_the_baseline_contract() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_refresh_repo(&repo);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };

    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    assert!(
        !task.contract.is_empty(),
        "the refresh-token goal must never leave an empty contract"
    );
    assert!(
        !task.contract.proof_obligations.is_empty(),
        "at least one deterministic obligation: {:?}",
        task.contract
    );
    let mut selectors: Vec<String> = Vec::new();
    for obligation in &task.contract.proof_obligations {
        assert!(
            obligation.evidence_binding.is_bound(),
            "a compiled obligation never carries the legacy Category binding"
        );
        assert_eq!(obligation.state, faktor_core::ClaimState::Unknown);
        selectors.extend(selector_targets(obligation));
    }
    assert!(
        selectors.contains(&"auth::refresh::tests::replay_is_rejected".to_string()),
        "the discovered test must be the obligation's binding: {selectors:?}"
    );

    // Replayed drive start: the deterministic compile merges to a no-op and
    // the durable revision does not move.
    let revision = handle.task_revision(task_id).unwrap();
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    assert_eq!(handle.task_revision(task_id).unwrap(), revision);
    let replayed = handle.get_task(task_id).unwrap().unwrap();
    assert_eq!(replayed.contract, task.contract);

    // The compiled contract rides the durable row and re-validates on a
    // repeated union merge (the session's own contract validator).
    handle.merge_task_contract(task_id, &task.contract).unwrap();
}

#[test]
fn readiness_seam_only_appends_bounded_medium_risks_to_the_compiled_contract() {
    let contract = faktor_core::EngineeringContract {
        proof_obligations: vec![faktor_core::EngineeringContract::obligation_bound(
            "the legacy category claim holds",
            faktor_core::ProofEvidenceKind::TestExecution,
            faktor_core::ContractOrigin::User,
            faktor_core::Requirement::Required,
            faktor_core::EvidenceBinding::Category,
            faktor_core::AcceptedStrength::for_requirement(faktor_core::Requirement::Required),
        )],
        ..Default::default()
    };
    let mut compiled = faktor_core::CompiledContract {
        contract,
        provenance: Vec::new(),
        uncertainties: Vec::new(),
        intent_tokens: vec!["legacy".into()],
        build_systems: vec!["cargo".into()],
        touched_files: vec!["src/lib.rs".into()],
        touched_symbols: Vec::new(),
        repository_digest: "repo:readiness-test".into(),
        repository_acquisition: faktor_core::RepositoryAcquisition::Complete,
        enrichments: Vec::new(),
    };
    let before = compiled.clone();
    let audit = faktor_core::audit_compiled_design(&compiled);
    let applied = faktor_core::apply_design_readiness(&mut compiled, &audit);

    assert!(applied >= 1, "the unbound required proof is a real gap");
    assert!(
        applied <= faktor_core::MAX_DESIGN_READINESS_FINDINGS,
        "application stays bounded"
    );
    assert_eq!(
        compiled.contract.proof_obligations, before.contract.proof_obligations,
        "readiness never creates, rewrites or discharges a proof obligation"
    );
    assert_eq!(
        compiled.contract.verification_depth(),
        before.contract.verification_depth(),
        "a calm compiled contract is never newly promoted to Deep verification"
    );
    assert_eq!(
        compiled.contract.verification_depth(),
        faktor_core::VerificationDepth::Standard
    );
    for risk in &compiled.contract.risks[before.contract.risks.len()..] {
        assert_eq!(risk.severity, faktor_core::RiskSeverity::Medium);
        assert_eq!(risk.origin, faktor_core::ContractOrigin::Heuristic);
        assert!(risk.id.starts_with("dr1:"));
        assert!(risk.id.len() <= 96);
        assert!(!risk.description.is_empty());
        assert!(risk.description.len() <= 320);
    }
    assert_eq!(compiled.provenance, before.provenance);
    assert_eq!(compiled.uncertainties, before.uncertainties);
    assert_eq!(compiled.intent_tokens, before.intent_tokens);
    assert_eq!(compiled.build_systems, before.build_systems);
    assert_eq!(compiled.touched_files, before.touched_files);
    assert_eq!(compiled.touched_symbols, before.touched_symbols);
    assert_eq!(compiled.repository_digest, before.repository_digest);
    assert_eq!(
        compiled.repository_acquisition,
        before.repository_acquisition
    );
    assert_eq!(compiled.enrichments, before.enrichments);
    assert!(audit.focus_terms.len() <= faktor_core::MAX_DESIGN_FOCUS_TERMS);
    assert!(audit.focus_terms.iter().all(|term| {
        !term.is_empty()
            && term.len() <= faktor_core::MAX_DESIGN_FOCUS_TERM_BYTES
            && term
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }));
}

#[tokio::test]
async fn drive_start_records_uncertainty_when_nothing_is_derivable() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("empty-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };

    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    assert!(task.contract.proof_obligations.is_empty());
    assert!(
        !task.contract.is_empty(),
        "an uncertainty is recorded instead of an empty contract"
    );
    assert!(task.contract.risks.iter().any(|risk| risk
        .description
        .contains("no deterministic proof obligation")));
}

#[tokio::test]
async fn an_existing_contract_is_never_recompiled_over() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_refresh_repo(&repo);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let task_id = handle.task_id().unwrap();
    let user_contract = faktor_core::EngineeringContract {
        constraints: vec![faktor_core::Constraint {
            id: "c1:user".to_string(),
            statement: "no new dependencies".to_string(),
            origin: faktor_core::ContractOrigin::User,
        }],
        ..Default::default()
    };
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id,
            session_id: handle.id(),
            goal: "Fix the refresh-token race".to_string(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: user_contract.clone(),
            budget: Default::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    assert_eq!(
        task.contract, user_contract,
        "a user-authored contract is authoritative: the baseline is not merged over it"
    );
}

/// A spy that ANSWERS every bounded question with a real finding bound to
/// the request's own repository identity, records which questions it was
/// asked, and counts consults: the production seam's happy path.
#[derive(Default)]
struct AnsweringIntel {
    consults: std::sync::atomic::AtomicUsize,
    questions: std::sync::Mutex<Vec<&'static str>>,
}

impl SystemIntelligenceProvider for AnsweringIntel {
    fn provider_id(&self) -> &str {
        "spy-intel"
    }

    fn findings<'a>(
        &'a self,
        request: &'a FindingsRequest<'a>,
    ) -> IntelligenceFuture<'a, Result<FindingsResponse, String>> {
        Box::pin(async move {
            self.consults
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            {
                let mut seen = self
                    .questions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                for question in request.questions {
                    seen.push(question.as_str());
                }
            }
            let findings = request
                .questions
                .iter()
                .map(|question| ProviderFinding {
                    question: *question,
                    kind: question.expected_kind(),
                    text: format!("the installed provider answered {}", question.as_str()),
                    basis: "runtime contract-compiler test spy".to_string(),
                    confidence_bps: 8_800,
                    repository_digest: request.repository_digest.to_string(),
                    provider: "spy-intel".to_string(),
                    revision: request.source_revision.map(str::to_string),
                    severity: None,
                })
                .collect();
            Ok(FindingsResponse {
                findings,
                diagnostics: Vec::new(),
            })
        })
    }
}

/// A wired value gate that grants every entitlement (the commercial
/// authority is present and entitled): the paid organization-contracts
/// consult is not what these tests exercise.
#[derive(Debug)]
struct GrantingGate;

impl faktor_core::entitlement::ValueEntitlementGate for GrantingGate {
    fn check_value_entitlement(
        &self,
        _entitlement: faktor_core::entitlement::ValueEntitlement,
    ) -> Result<(), faktor_core::entitlement::ValueGateRefusal> {
        Ok(())
    }
}

fn granting_gate() -> faktor_core::entitlement::ValueGate {
    faktor_core::entitlement::ValueGate::wired(Arc::new(GrantingGate))
}

/// A provider that refuses every consult: the contract must stay native-only
/// and the drive must not fail.
struct RefusingIntel;

impl SystemIntelligenceProvider for RefusingIntel {
    fn provider_id(&self) -> &str {
        "refusing-intel"
    }

    fn findings<'a>(
        &'a self,
        _request: &'a FindingsRequest<'a>,
    ) -> IntelligenceFuture<'a, Result<FindingsResponse, String>> {
        Box::pin(async move { Err("no intelligence capability".to_string()) })
    }
}

/// The native contract the runtime's own deterministic scanner compiles for
/// `repo` + `goal` (the exact bytes the no-provider path must produce).
fn native_contract_for(repo: &std::path::Path, goal: &str) -> faktor_core::EngineeringContract {
    let tokens = intent_tokens(goal);
    let repository = scan_repository_snapshot(repo, &tokens);
    compile_contract(&ContractCompileRequest {
        goal: goal.to_string(),
        repository,
    })
    .contract
}

/// Remove every `external:<provider>` statement: the native remainder must be
/// byte-identical to the native compile.
fn strip_external(contract: &faktor_core::EngineeringContract) -> faktor_core::EngineeringContract {
    let mut kept = contract.clone();
    kept.constraints
        .retain(|c| !matches!(&c.origin, faktor_core::ContractOrigin::External { .. }));
    kept.invariants
        .retain(|i| !matches!(&i.origin, faktor_core::ContractOrigin::External { .. }));
    kept.risks
        .retain(|r| !matches!(&r.origin, faktor_core::ContractOrigin::External { .. }));
    kept.proof_obligations
        .retain(|o| !matches!(&o.origin, faktor_core::ContractOrigin::External { .. }));
    kept
}

fn external_count(contract: &faktor_core::EngineeringContract) -> usize {
    let mut count = 0usize;
    count += contract
        .constraints
        .iter()
        .filter(|c| matches!(&c.origin, faktor_core::ContractOrigin::External { .. }))
        .count();
    count += contract
        .invariants
        .iter()
        .filter(|i| matches!(&i.origin, faktor_core::ContractOrigin::External { .. }))
        .count();
    count += contract
        .risks
        .iter()
        .filter(|r| matches!(&r.origin, faktor_core::ContractOrigin::External { .. }))
        .count();
    count += contract
        .proof_obligations
        .iter()
        .filter(|o| matches!(&o.origin, faktor_core::ContractOrigin::External { .. }))
        .count();
    count
}

#[tokio::test]
async fn installed_provider_answers_the_bounded_questions_into_the_durable_contract() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_refresh_repo(&repo);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let spy = Arc::new(AnsweringIntel::default());
    runtime.set_system_intelligence_provider(spy.clone());
    runtime.set_value_gate(granting_gate());
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();

    // The runtime actually ASKED the provider the bounded questions EXACTLY
    // once: readiness is a pure seam over the single neutral consult, never a
    // second paid model call.
    assert_eq!(
        spy.consults.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one compile must consult the system-intelligence provider exactly once"
    );
    let asked: Vec<&str> = spy
        .questions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    for question in IntelligenceQuestion::ALL {
        assert!(
            asked.contains(&question.as_str()),
            "question {} was never asked: {asked:?}",
            question.as_str()
        );
    }

    // The answers are durable `external:<provider>` statements of their
    // typed kinds, NOT capability descriptions; the native remainder is
    // byte-identical: enrichment is strictly additive.
    for (label, question) in [
        (
            "affected component: ",
            IntelligenceQuestion::AffectedComponents,
        ),
        (
            "hidden dependency: ",
            IntelligenceQuestion::HiddenDependencies,
        ),
        ("blast radius: ", IntelligenceQuestion::BlastRadius),
        (
            "historical failure: ",
            IntelligenceQuestion::HistoricalFailures,
        ),
    ] {
        let expected = format!(
            "{label}the installed provider answered {}",
            question.as_str()
        );
        let risk = task
            .contract
            .risks
            .iter()
            .find(|risk| risk.description == expected)
            .unwrap_or_else(|| panic!("missing typed risk statement {expected:?}"));
        assert_eq!(
            risk.origin,
            faktor_core::ContractOrigin::External {
                provider: "spy-intel".into()
            }
        );
    }
    let invariant = format!(
        "invariant that must survive: the installed provider answered {}",
        IntelligenceQuestion::Invariants.as_str()
    );
    assert!(
        task.contract
            .invariants
            .iter()
            .any(|entry| entry.statement == invariant
                && matches!(entry.origin, faktor_core::ContractOrigin::External { .. })),
        "the invariant answer must be a durable external invariant: {:?}",
        task.contract.invariants
    );
    assert_eq!(external_count(&task.contract), 5);
    assert!(!task
        .contract
        .risks
        .iter()
        .any(|risk| risk.description.contains("declares")));
    let native = native_contract_for(&repo, &ledger.goal);
    assert_eq!(
        strip_external(&task.contract),
        native,
        "removing external findings yields the exact native contract"
    );
    assert!(
        !native.proof_obligations.is_empty(),
        "the native proof obligations still compile: {native:?}"
    );
}

#[tokio::test]
async fn refusing_provider_leaves_the_native_contract_and_never_fails_the_drive() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_refresh_repo(&repo);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_system_intelligence_provider(Arc::new(RefusingIntel));
    runtime.set_value_gate(granting_gate());
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    // The drive is not failed by the refusal...
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    // ...and the durable contract is exactly the native one.
    assert_eq!(external_count(&task.contract), 0);
    assert_eq!(
        task.contract,
        native_contract_for(&repo, &ledger.goal),
        "a refusing provider never alters the native contract"
    );
}

/// Design-readiness INTEGRATION (not just the pure seam): the real compile
/// path applies the audit exactly once over the single provider consult,
/// appends bounded heuristic risks, creates NO proof obligation and NO
/// second artifact, and a replayed drive neither re-consults nor mutates the
/// durable revision.
#[tokio::test]
async fn compile_path_applies_readiness_once_without_side_effects() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("empty-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let spy = Arc::new(AnsweringIntel::default());
    runtime.set_system_intelligence_provider(spy.clone());
    runtime.set_value_gate(granting_gate());
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    let native = native_contract_for(&repo, &ledger.goal);
    assert_eq!(
        spy.consults.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the compile path consults the provider exactly once"
    );
    assert_eq!(
        task.contract.proof_obligations, native.proof_obligations,
        "readiness and enrichment create/rewrite NO proof obligation"
    );
    let mut seen = std::collections::HashSet::new();
    let readiness: Vec<_> = task
        .contract
        .risks
        .iter()
        .filter(|risk| risk.id.starts_with("dr1:"))
        .collect();
    assert!(
        !readiness.is_empty(),
        "the compile path applied the readiness audit (native gap: {native:?})"
    );
    assert!(
        readiness.len() <= faktor_core::MAX_DESIGN_READINESS_FINDINGS,
        "readiness stays bounded"
    );
    for risk in readiness {
        assert_eq!(risk.origin, faktor_core::ContractOrigin::Heuristic);
        assert!(
            seen.insert(risk.id.clone()),
            "a finding is appended exactly once, never duplicated: {risk:?}"
        );
    }
    assert_eq!(
        external_count(&task.contract),
        5,
        "readiness creates no extra artifact/statement"
    );
    let revision = handle.task_revision(task_id).unwrap();
    let contract = task.contract.clone();
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    assert_eq!(
        spy.consults.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a replayed compile is a no-op, never a second model call"
    );
    assert_eq!(handle.task_revision(task_id).unwrap(), revision);
    assert_eq!(
        handle.get_task(task_id).unwrap().unwrap().contract,
        contract
    );
}

#[tokio::test]
async fn no_provider_is_byte_identical_to_the_native_compile() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_refresh_repo(&repo);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let mut ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime
        .restore_task_rows(&handle, &mut ledger)
        .await
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    assert_eq!(external_count(&task.contract), 0);
    assert_eq!(
        task.contract,
        native_contract_for(&repo, &ledger.goal),
        "the default runtime path compiles exactly the native contract"
    );
    assert_eq!(
        serde_json::to_vec(&task.contract).unwrap(),
        serde_json::to_vec(&native_contract_for(&repo, &ledger.goal)).unwrap(),
        "byte-identical, not merely equal"
    );
}

/// A wired value gate that DENIES the organization-contracts entitlement
/// (the plan does not map it): the typed refusal names the entitlement and
/// plan.
#[derive(Debug)]
struct DenyingContracts;

impl faktor_core::entitlement::ValueEntitlementGate for DenyingContracts {
    fn check_value_entitlement(
        &self,
        entitlement: faktor_core::entitlement::ValueEntitlement,
    ) -> Result<(), faktor_core::entitlement::ValueGateRefusal> {
        Err(faktor_core::entitlement::ValueGateRefusal {
            entitlement,
            hook: entitlement.enforcement_hook(),
            cause: faktor_core::entitlement::ValueRefusalCause::NotInPlan,
            plan_id: Some("free".into()),
            organization_id: Some("org_local".into()),
            message: format!(
                "value entitlement {} (hook {}) not granted for organization org_local: not_in_plan (plan Some(\"free\"))",
                entitlement.as_str(),
                entitlement.hook_tag()
            ),
        })
    }
}

/// The organization-contract enrichment BOUNDARY: a denied plan (and the
/// offline absent gate) must refuse the enrichment TYPED at the runtime —
/// the provider is NEVER consulted, the native contract stands byte-for-byte,
/// and no `external:` provenance is minted. The local compile path is
/// untouched.
#[tokio::test]
async fn denied_organization_contracts_refuse_enrichment_typed_before_the_provider() {
    for (label, gate) in [
        (
            "denied plan",
            faktor_core::entitlement::ValueGate::wired(Arc::new(DenyingContracts)),
        ),
        (
            "offline absent gate",
            faktor_core::entitlement::ValueGate::absent(),
        ),
    ] {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        write_refresh_repo(&repo);
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps, _keep) =
            deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
        let runtime = AgentRuntime::new(deps).unwrap();
        let spy = Arc::new(AnsweringIntel::default());
        runtime.set_system_intelligence_provider(spy.clone());
        runtime.set_value_gate(gate);
        let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
        let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
        let mut ledger = TaskLedger {
            goal: "Fix the refresh-token race".to_string(),
            ..Default::default()
        };
        runtime
            .restore_task_rows(&handle, &mut ledger)
            .await
            .unwrap();
        let task_id = handle.task_id().unwrap();
        let task = handle.get_task(task_id).unwrap().unwrap();
        assert_eq!(
            spy.consults.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{label}: the paid provider consult must never happen under a refusal"
        );
        assert_eq!(external_count(&task.contract), 0, "{label}");
        assert_eq!(
            task.contract,
            native_contract_for(&repo, &ledger.goal),
            "{label}: the native contract stands"
        );
    }
}
