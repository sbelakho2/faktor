//! `runtime::contract_compiler_tests`: adversarial covers of the drive-start
//! baseline contract compilation (audit item 5) — the bounded scanner, the
//! production merge through the durable task row, the idempotent replay and
//! the uncertainty-only contract when nothing is derivable.

use super::*;
use crate::runtime::contract_compiler::scan_repository_snapshot;
use crate::runtime::tests::*;
use crate::*;

use faktor_core::contract_compile::intent_tokens;

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
    assert!(first.complete);
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
    // A missing root is an empty COMPLETE snapshot (nothing to analyze is
    // disclosed by the compiler's own uncertainties, never a scan failure).
    let missing = scan_repository_snapshot(std::path::Path::new("/nonexistent-kp-root"), &[]);
    assert!(missing.complete);
    assert!(missing.files.is_empty());
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
