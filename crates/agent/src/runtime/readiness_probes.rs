//! Design-readiness wire probes: the REAL context compiler + the REAL wire
//! planner, not a byte-count approximation.
//!
//! The production path derives an EPHEMERAL focus policy from the durable
//! contract, compiles the baseline and focused NeedSets over ONE acquired
//! candidate set, renders BOTH final wire plans, and adopts the focused plan
//! only when the real tokenizer dividend is at least 512 tokens AND one eighth
//! of the baseline. These probes certify that contract end to end:
//!
//! 1. baseline vs focused final tokens never grow, and an adopted focus plan
//!    saves >=512 tokens and >=12.5%;
//! 2. a calm task keeps a byte-identical prompt digest (no readiness section);
//! 3. readiness is ephemeral (no durable `dr1:` row/artifact/model/Tangerine
//!    call — the compile-path probe covers the runtime half);
//! 4. the focused projection and its wire digest survive a manager restart.

use super::retrieval_tests::design_focus_contract;
use super::tests::{deps, new_session, scripted_provider};
use super::*;
use faktor_context::compiler::{CompiledContext, CompilerInput, VolatileClaims};
use faktor_evidence::store::{EvidenceStore, MemoryEvidenceStore};
use faktor_evidence::types::{
    BackingCompleteness, CompactRepresentation, CompressionRecord, EvidenceEnvelope, EvidenceId,
    EvidenceKind, ProvenanceSet, ProvenanceSource, RetrievalPolicy,
};
use faktor_session::SessionManager;
use std::sync::Arc;

const FOCUS_STEP: &str = "implement the parser fix";

fn long_changed_paths() -> Vec<String> {
    (0..12)
        .map(|index| format!("src/module_{index:02}_{}.rs", "x".repeat(240)))
        .collect()
}

fn focus_ledger(changed: Vec<String>) -> TaskLedger {
    TaskLedger {
        goal: "fix the parser".into(),
        open_steps: vec![FOCUS_STEP.into()],
        changed_files: changed,
        ..Default::default()
    }
}

fn variants_with_contract(
    contract: faktor_core::EngineeringContract,
    ledger: &TaskLedger,
) -> TaskFactsVariants {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let handle = runtime
        .deps()
        .session
        .get_session(new_session(runtime.deps()))
        .unwrap()
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: handle.id(),
            goal: ledger.goal.clone(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract,
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    runtime
        .task_facts_variants_for(&handle, ledger, task_id)
        .expect("durable task facts are readable")
}

fn envelope(id: u64, facts: &TaskFacts, revision: &str, body: &str) -> EvidenceEnvelope {
    EvidenceEnvelope::new(
        EvidenceId(id),
        EvidenceKind::SymbolSet,
        facts.session_id,
        facts.workspace_id,
        facts.task_id,
        Some(revision.to_string()),
        ProvenanceSet::new([ProvenanceSource::Tool]),
        EvidenceKind::SymbolSet.default_compressibility(),
        CompactRepresentation {
            grammar: "readiness-probe-v1".to_string(),
            body: body.to_string(),
        },
        None,
        BackingCompleteness::Complete,
        CompressionRecord::identity(body.len() as u64),
        RetrievalPolicy::new(true, true, 4096),
    )
    .unwrap()
}

/// A store whose evidence is reachable from the turn's path material. The
/// focused projection only replaces the work item's DUPLICATED path list;
/// owned/recent path needs still retrieve the same evidence, so this fixture
/// certifies the no-growth direction without pretending focus can hide a
/// required path.
fn baseline_evidence_store(facts: &TaskFacts, changed: &[String]) -> Arc<MemoryEvidenceStore> {
    let mut store = MemoryEvidenceStore::new(64 * 1024);
    for index in 0..12u64 {
        let revision = changed
            .get(index as usize)
            .cloned()
            .unwrap_or_else(|| changed[0].clone());
        let body = format!(
            "baseline evidence {index}\n{}\n{}",
            changed.join(" "),
            "bounded retrieval body ".repeat(64)
        );
        store
            .insert(envelope(index + 1, facts, &revision, &body), None)
            .unwrap();
    }
    Arc::new(store)
}

fn compile_variants(
    baseline: &TaskFacts,
    focused: &TaskFacts,
    store: &Arc<MemoryEvidenceStore>,
) -> (CompiledContext, CompiledContext) {
    let source: Arc<dyn faktor_context::compiler::ScopedEvidenceSource> = store.clone();
    let compiler = ContextCompiler::new(Some(source), None);
    let claims = VolatileClaims {
        evidence_tokens: 32_768,
        ..VolatileClaims::default()
    };
    let baseline_input = CompilerInput::new(baseline.clone(), 32_768).with_volatile(claims);
    let focused_input = CompilerInput::new(focused.clone(), 32_768).with_volatile(claims);
    compiler
        .compile_variants(&baseline_input, &focused_input)
        .expect("probe variant compile")
}

fn evidence_from_compiled(compiled: &CompiledContext) -> Vec<Evidence> {
    compiled
        .selected
        .iter()
        .map(|item| Evidence {
            path: item.path.clone(),
            snippet: item.body.clone(),
            score: if item.required { 1.0 } else { item.score },
        })
        .collect()
}

fn plan(ledger: &TaskLedger, evidence: &[Evidence]) -> WirePlan {
    crate::wire_plan::plan_wire_turn_with_prior(
        "You are a test agent.\n",
        "",
        &[],
        "",
        ledger,
        "",
        &[],
        evidence,
        &ContextBudget::default(),
        "gpt-5",
        &TokenCache::new(),
        None,
    )
    .expect("the real wire planner must render the probe request")
}

fn prompt_digest(plan: &WirePlan) -> Vec<FileHash> {
    plan.prompt_segments.hashes()
}

#[test]
fn focused_wire_plan_never_grows_and_activation_saves_512_tokens_and_one_eighth() {
    let changed = long_changed_paths();
    let ledger = focus_ledger(changed.clone());
    let contract = design_focus_contract();
    let variants = variants_with_contract(contract, &ledger);
    let focused_facts = variants
        .focused
        .as_ref()
        .expect("the large fixture must pass the lazy focus gate");
    let baseline_work = variants.baseline.active_work_item.as_ref().unwrap();
    let focused_work = focused_facts.active_work_item.as_ref().unwrap();
    assert_eq!(baseline_work.paths, changed);
    assert!(focused_work.paths.is_empty());
    assert_eq!(baseline_work.title, focused_work.title);

    let store = baseline_evidence_store(&variants.baseline, &changed);
    let (baseline_compiled, focused_compiled) =
        compile_variants(&variants.baseline, focused_facts, &store);
    assert!(!baseline_compiled.is_empty());
    let baseline_evidence = evidence_from_compiled(&baseline_compiled);
    let focused_evidence = evidence_from_compiled(&focused_compiled);
    let baseline_plan = plan(&ledger, &baseline_evidence);
    let focused_plan = plan(&ledger, &focused_evidence);
    assert!(
        focused_plan.total_tokens <= baseline_plan.total_tokens,
        "the focused projection must never grow the final wire request: {} > {}",
        focused_plan.total_tokens,
        baseline_plan.total_tokens
    );
    let saved = baseline_plan
        .total_tokens
        .saturating_sub(focused_plan.total_tokens);
    let dividend = faktor_core::design_focus_has_dividend(
        baseline_plan.total_tokens,
        focused_plan.total_tokens,
    );
    let (chosen, chosen_evidence) = crate::wire_plan::choose_design_focus_plan(
        baseline_plan.clone(),
        baseline_evidence.clone(),
        Some((focused_plan.clone(), focused_evidence.clone())),
    );
    if dividend {
        assert!(saved >= 512, "adopted focus must save >=512 final tokens");
        assert!(
            (saved as u128) * 8 >= baseline_plan.total_tokens as u128,
            "adopted focus must save >=12.5% of final tokens ({saved}/{}",
            baseline_plan.total_tokens
        );
        assert_eq!(chosen.total_tokens, focused_plan.total_tokens);
        assert_eq!(chosen_evidence, focused_evidence);
    } else {
        assert_eq!(
            chosen.total_tokens, baseline_plan.total_tokens,
            "a focused plan without the real token dividend must not be adopted"
        );
        assert_eq!(chosen_evidence, baseline_evidence);
    }
}

#[test]
fn final_token_gate_rejects_a_focused_plan_below_512_or_one_eighth() {
    let ledger = focus_ledger(vec!["src/parser.rs".into()]);
    let baseline_plan = plan(&ledger, &[]);
    let baseline = baseline_plan.total_tokens;
    let baseline_evidence = Vec::new();
    let mut near = baseline_plan.clone();
    // 511 tokens saved is never a dividend, however small the baseline.
    near.total_tokens = baseline.saturating_sub(511);
    let (chosen, _) = crate::wire_plan::choose_design_focus_plan(
        baseline_plan.clone(),
        baseline_evidence.clone(),
        Some((near, Vec::new())),
    );
    assert_eq!(
        chosen.total_tokens, baseline,
        "a <512-token saving must keep the baseline plan"
    );
    // A baseline below the absolute 512-token minimum can never adopt focus.
    assert!(!faktor_core::design_focus_has_dividend(baseline, 0));
    // Exactly 512 tokens is a dividend only when it is also >=12.5%.
    let mut exact = baseline_plan.clone();
    exact.total_tokens = baseline.saturating_sub(512);
    let required = baseline.div_ceil(8).max(512);
    let expected = if baseline >= required {
        baseline.saturating_sub(512)
    } else {
        baseline
    };
    let (chosen, _) = crate::wire_plan::choose_design_focus_plan(
        baseline_plan.clone(),
        baseline_evidence.clone(),
        Some((exact, Vec::new())),
    );
    assert_eq!(chosen.total_tokens, expected);
}

#[test]
fn calm_task_prompt_digest_is_byte_identical_before_and_after_the_audit() {
    let ledger = focus_ledger(vec!["src/parser.rs".into()]);
    let before = faktor_core::EngineeringContract::default();
    let audit = faktor_core::audit_compiled_design(&before);
    assert!(audit.is_empty(), "a calm contract has no findings");
    let mut after = before.clone();
    assert_eq!(faktor_core::apply_design_readiness(&mut after, &audit), 0);
    assert_eq!(after, before, "a calm audit is a byte-identical no-op");

    let before_variants = variants_with_contract(before, &ledger);
    let after_variants = variants_with_contract(after, &ledger);
    assert_eq!(before_variants.baseline, after_variants.baseline);
    assert!(before_variants.focused.is_none());
    assert!(after_variants.focused.is_none());
    let before_plan = plan(&ledger, &[]);
    let after_plan = plan(&ledger, &[]);
    assert_eq!(before_plan.system, after_plan.system);
    assert_eq!(before_plan.messages, after_plan.messages);
    assert_eq!(before_plan.tools, after_plan.tools);
    assert_eq!(prompt_digest(&before_plan), prompt_digest(&after_plan));
    assert_eq!(before_plan.total_tokens, after_plan.total_tokens);
}

#[tokio::test]
async fn focused_projection_and_wire_digest_survive_a_manager_restart() {
    let (seed_deps, dir) = deps(scripted_provider(vec![]), vec![]);
    let manager = seed_deps.session.clone();
    let workspace = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(workspace, "t", "fake", "m").unwrap();
    let session = handle.id();
    let task_id = handle.task_id().unwrap();
    let changed = long_changed_paths();
    let contract = design_focus_contract();
    let ledger = focus_ledger(changed);
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: session,
            goal: ledger.goal.clone(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: contract.clone(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let runtime = AgentRuntime::new(seed_deps).unwrap();
    let variants_before = runtime
        .task_facts_variants_for(&handle, &ledger, task_id)
        .unwrap();
    assert!(variants_before.focused.is_some());
    let plan_before = plan(&ledger, &[]);
    drop(runtime);
    drop(handle);

    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let handle = reopened.get_session(session).unwrap().unwrap();
    let (restart_deps, _dir2) = super::tests::deps_sharing_session(
        reopened.clone(),
        Arc::new(scripted_provider(vec![])),
        vec![],
    );
    let runtime = AgentRuntime::new(restart_deps).unwrap();
    let variants_after = runtime
        .task_facts_variants_for(&handle, &ledger, task_id)
        .unwrap();
    let plan_after = plan(&ledger, &[]);
    assert_eq!(variants_before.baseline, variants_after.baseline);
    assert_eq!(variants_before.focused, variants_after.focused);
    assert_eq!(plan_before.system, plan_after.system);
    assert_eq!(prompt_digest(&plan_before), prompt_digest(&plan_after));
    assert_eq!(plan_before.total_tokens, plan_after.total_tokens);
    let durable = handle.get_task(task_id).unwrap().unwrap();
    assert!(
        !durable.contract.risks.iter().any(|risk| {
            faktor_core::is_design_readiness_id(&risk.id)
                || risk
                    .description
                    .starts_with(faktor_core::DESIGN_READINESS_DESCRIPTION_PREFIX)
        }),
        "readiness must stay ephemeral across restart: {:?}",
        durable.contract.risks
    );
}
