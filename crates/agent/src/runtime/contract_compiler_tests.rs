//! `runtime::contract_compiler_tests`: adversarial covers of the drive-start
//! baseline contract compilation (audit item 5) — the bounded scanner, the
//! production merge through the durable task row, the idempotent replay and
//! the uncertainty-only contract when nothing is derivable.

use super::*;
use crate::runtime::contract_compiler::scan_repository_snapshot;
use crate::runtime::tests::*;
use crate::*;

use faktor_core::contract_compile::{
    compile_contract, intent_tokens, AcquisitionGapKind, ContractCompileRequest, EnrichmentKind,
    EnrichmentRequest, EnrichmentStatement, RepositoryAcquisition, SystemIntelligenceProvider,
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
    let ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };

    runtime.restore_task_rows(&handle, &ledger).unwrap();
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
    runtime.restore_task_rows(&handle, &ledger).unwrap();
    assert_eq!(handle.task_revision(task_id).unwrap(), revision);
    let replayed = handle.get_task(task_id).unwrap().unwrap();
    assert_eq!(replayed.contract, task.contract);

    // The compiled contract rides the durable row and re-validates on a
    // repeated union merge (the session's own contract validator).
    handle.merge_task_contract(task_id, &task.contract).unwrap();
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
    let ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };

    runtime.restore_task_rows(&handle, &ledger).unwrap();
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
    let ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime.restore_task_rows(&handle, &ledger).unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();
    assert_eq!(
        task.contract, user_contract,
        "a user-authored contract is authoritative: the baseline is not merged over it"
    );
}

/// A spy that proposes ONE additive disclosure risk: the production seam's
/// happy path.
struct AddingIntel;

impl SystemIntelligenceProvider for AddingIntel {
    fn provider_id(&self) -> &str {
        "spy-intel"
    }

    fn enrich(&self, request: &EnrichmentRequest<'_>) -> Result<Vec<EnrichmentStatement>, String> {
        if request.max_statements == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![EnrichmentStatement {
            kind: EnrichmentKind::Risk,
            text: "the drive-start runtime consulted the installed system-intelligence provider"
                .to_string(),
            confidence_bps: 8_800,
            basis: "runtime contract-compiler test spy".to_string(),
            evidence: None,
            binding: None,
            severity: Some(faktor_core::RiskSeverity::Low),
            advisory: false,
        }])
    }
}

/// A provider that refuses every consult: the contract must stay native-only
/// and the drive must not fail.
struct RefusingIntel;

impl SystemIntelligenceProvider for RefusingIntel {
    fn provider_id(&self) -> &str {
        "refusing-intel"
    }

    fn enrich(&self, _request: &EnrichmentRequest<'_>) -> Result<Vec<EnrichmentStatement>, String> {
        Err("no intelligence capability".to_string())
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
async fn installed_provider_enriches_the_durable_contract_additively() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_refresh_repo(&repo);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_system_intelligence_provider(Arc::new(AddingIntel));
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime.restore_task_rows(&handle, &ledger).unwrap();
    let task_id = handle.task_id().unwrap();
    let task = handle.get_task(task_id).unwrap().unwrap();

    // The provider's statement is durable with `external:<provider>`
    // provenance, and the native remainder is byte-identical to the native
    // compile: enrichment is strictly additive.
    let external = task
        .contract
        .risks
        .iter()
        .find(|risk| {
            matches!(
                &risk.origin,
                faktor_core::ContractOrigin::External { provider } if provider == "spy-intel"
            )
        })
        .expect("the installed provider's risk statement is durable");
    assert!(external
        .description
        .contains("system-intelligence provider"));
    assert_eq!(external_count(&task.contract), 1);
    let native = native_contract_for(&repo, &ledger.goal);
    assert_eq!(
        task.contract.constraints, native.constraints,
        "native constraints are byte-identical under enrichment"
    );
    assert_eq!(task.contract.invariants, native.invariants);
    assert_eq!(
        strip_external(&task.contract),
        native,
        "removing external statements yields the exact native contract"
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
    let ws = manager.create_workspace(repo.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    // The drive is not failed by the refusal...
    runtime.restore_task_rows(&handle, &ledger).unwrap();
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
    let ledger = TaskLedger {
        goal: "Fix the refresh-token race".to_string(),
        ..Default::default()
    };
    runtime.restore_task_rows(&handle, &ledger).unwrap();
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
