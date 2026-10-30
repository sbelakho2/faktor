// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Daemon tools for the closed Engineering -> Supply loop.
//!
//! The tools are thin, bounded adapters over `faktor-supply-engineering`:
//! they parse one JSON object, run the deterministic kernel and return the
//! typed result as JSON. They make no model calls, touch no disk or network,
//! and never send an RFQ/message or create an order.

use std::sync::Arc;

use faktor_agent::{RecoveryHint, Tool, ToolOutcome, ToolRunCtx};
use faktor_approval::{
    ApprovalAuthority, ApprovalDraft, ApprovalError, ApprovalKind, Principal, PrincipalKind,
    RiskLevel,
};
use faktor_core::error::Error;
use faktor_core::resource::ResourceClass;
use serde_json::{json, Value};

/// Tool name: run the closed loop and emit a SupplyDecision.
pub const SUPPLY_DECISION_TOOL: &str = "supply_decision";
/// Tool name: engineering analysis of every candidate.
pub const ENGINEERING_ANALYSE_TOOL: &str = "engineering_analyse";
/// Tool name: verify a stored loop result.
pub const ENGINEERING_VERIFY_TOOL: &str = "engineering_verify";
/// Tool name: propose a consequential action for human approval. This tool
/// can request but never decide.
pub const APPROVAL_REQUEST_TOOL: &str = "approval_request";
/// Tool name: read one durable approval request/decision.
pub const APPROVAL_STATUS_TOOL: &str = "approval_status";

/// Maximum approval request lifetime offered by a tool (24 hours).
const MAX_TOOL_APPROVAL_LIFETIME_MS: u64 = 24 * 60 * 60 * 1_000;

/// Maximum bytes in one tool input.
pub const MAX_TOOL_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum bytes returned to the model.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;

fn bounded_output(value: Value) -> Result<ToolOutcome, Error> {
    let text = serde_json::to_string(&value)
        .map_err(|error| Error::internal(format!("cannot encode result: {error}")))?;
    if text.len() > MAX_TOOL_OUTPUT_BYTES {
        return Err(Error::oversized(format!(
            "result exceeds the {MAX_TOOL_OUTPUT_BYTES}-byte tool output bound"
        )));
    }
    Ok(ToolOutcome {
        text,
        exit_code: Some(0),
        ..Default::default()
    })
}

fn parse_input(args: &Value) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(args)
        .map_err(|error| Error::malformed(format!("invalid tool input: {error}")))?;
    if bytes.len() > MAX_TOOL_INPUT_BYTES {
        return Err(Error::oversized(format!(
            "input exceeds the {MAX_TOOL_INPUT_BYTES}-byte bound"
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
pub(crate) fn execute_loop(args: Value) -> Result<Value, Error> {
    let signer = engineering_core::ReceiptSigner::generate();
    let directory = std::env::temp_dir().join(format!(
        "faktor-tool-approval-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let authority = ApprovalAuthority::open(&directory)
        .map_err(|error| Error::internal(format!("approval authority: {error}")))?;
    execute_loop_with(args, &signer, &authority)
}

pub(crate) fn execute_loop_with(
    args: Value,
    signer: &engineering_core::ReceiptSigner,
    authority: &ApprovalAuthority,
) -> Result<Value, Error> {
    let bytes = parse_input(&args)?;
    let request: faktor_supply_engineering::LoopRequest = serde_json::from_slice(&bytes)
        .map_err(|error| Error::malformed(format!("invalid loop request: {error}")))?;
    match faktor_supply_engineering::run_with_authority(&request, signer, authority) {
        Ok(outcome) => serde_json::to_value(outcome)
            .map_err(|error| Error::internal(format!("cannot encode loop outcome: {error}"))),
        Err(error) => match error.approval_json() {
            Some(approval) => Ok(approval),
            None => Err(Error::malformed(format!("loop refused: {error}"))),
        },
    }
}

pub(crate) fn execute_verify_with(
    args: Value,
    verifier: &engineering_core::ReceiptVerifier,
) -> Result<Value, Error> {
    let bytes = parse_input(&args)?;
    let outcome: faktor_supply_engineering::LoopOutcome = serde_json::from_slice(&bytes)
        .map_err(|error| Error::malformed(format!("invalid loop outcome: {error}")))?;
    let verification = outcome
        .verify_with_verifier(verifier)
        .map_err(|error| Error::malformed(format!("verification refused: {error}")))?;
    let verified = verification.decision_valid
        && verification.requirement_set_valid
        && verification.receipt_valid
        && verification.result_digest_matches
        && verification.response_ready
        && verification.no_ungated_actions;
    Ok(json!({
        "verified": verified,
        "verification": verification,
    }))
}

/// Register every closed-loop tool on the daemon registry.
///
/// Every tool here exposes a COMPLETE generated schema (174 definitions for
/// the loop request, 173 for the loop outcome). Those complete schemas do
/// not fit a small-local context when several are advertised together: the
/// request schema alone is ~18.8K tokens and the outcome schema ~20.7K, so
/// the three eager tools cost ~60K tokens against the 32K profile's 25K
/// context maximum. The tools therefore enter LAZILY with disjoint
/// deterministic signals: an ordinary turn pays zero schema bytes/tokens,
/// and one activated schema still fits the 25K budget on its own. The
/// schemas are never trimmed, stubbed or replaced by an open placeholder —
/// when a tool is active its full typed contract is advertised.
pub(crate) fn register(
    registry: &mut faktor_agent::ToolRegistry,
    signer: Arc<engineering_core::ReceiptSigner>,
    verifier: Arc<engineering_core::ReceiptVerifier>,
    approvals: Arc<ApprovalAuthority>,
) {
    registry.register_lazy(
        supply_decision_tool(signer.clone(), approvals.clone()),
        supply_decision_exposure(),
    );
    registry.register_lazy(
        engineering_analyse_tool(signer, approvals.clone()),
        engineering_analyse_exposure(),
    );
    registry.register_lazy(
        engineering_verify_tool(verifier),
        engineering_verify_exposure(),
    );
    registry.register_lazy(
        approval_request_tool(approvals.clone()),
        approval_exposure(),
    );
    registry.register_lazy(approval_status_tool(approvals), approval_exposure());
}

fn loop_phase_mask() -> faktor_agent::PhaseMask {
    use faktor_agent::{PhaseMask, RouterPhase};
    PhaseMask::of([
        RouterPhase::Plan,
        RouterPhase::Explore,
        RouterPhase::Implement,
        RouterPhase::Review,
    ])
}

fn signal_exposure(signals: &[&str]) -> faktor_agent::ToolExposure {
    use faktor_agent::ToolTrigger;
    faktor_agent::ToolExposure::lazy(
        loop_phase_mask(),
        signals
            .iter()
            .map(|phrase| ToolTrigger::signal(*phrase))
            .collect(),
    )
}

/// Supply-decision signals. Disjoint from the analysis signals so one turn
/// never advertises two complete request schemas at once.
fn supply_decision_exposure() -> faktor_agent::ToolExposure {
    signal_exposure(&[
        "supply decision",
        "should-cost",
        "should cost",
        "landed cost",
        "total cost of ownership",
        "incoterm",
        "safety stock",
        "purchase order",
        "supplier selection",
        "supplier qualification",
        "bom pricing",
        "component pricing",
        "supply chain",
    ])
}

/// Engineering-analysis signals (the same complete request schema as
/// `supply_decision`, so the two are never co-activated by one text).
fn engineering_analyse_exposure() -> faktor_agent::ToolExposure {
    signal_exposure(&[
        "engineering analysis",
        "engineering analyse",
        "engineering analyze",
        "tolerance analysis",
        "thermal analysis",
        "emc analysis",
        "fatigue analysis",
        "worst-case analysis",
        "derating",
        "finite element",
        "flyback",
        "analog stability",
        "thermal network",
        "material selection",
        "reliability analysis",
    ])
}

/// Verification signals: deliberately narrower than a bare "verify" (which
/// belongs to the ordinary code-verification path).
fn engineering_verify_exposure() -> faktor_agent::ToolExposure {
    signal_exposure(&[
        "engineering verify",
        "verify the engineering",
        "verify the supply decision",
        "verify the loop",
        "analysis receipt",
        "receipt verification",
    ])
}

fn approval_exposure() -> faktor_agent::ToolExposure {
    signal_exposure(&[
        "approval",
        "approve",
        "approval request",
        "constraint relaxation",
        "relax tolerance",
        "waiver",
        "should-cost",
        "supplier qualification",
        "purchase order",
        "safety stock",
        "tariff classification",
    ])
}

fn request_schema() -> Value {
    faktor_supply_engineering::tool_schema::loop_request_schema()
}

fn outcome_schema() -> Value {
    faktor_supply_engineering::tool_schema::loop_outcome_schema()
}

/// The `supply_decision` tool.
pub fn supply_decision_tool(
    signer: Arc<engineering_core::ReceiptSigner>,
    approvals: Arc<ApprovalAuthority>,
) -> Tool {
    Tool {
        name: SUPPLY_DECISION_TOOL.into(),
        description: "Run the deterministic closed Engineering -> Supply loop over a typed JSON \
                      request: revision-bound engineering requirements and constraints, candidate \
                      facts, knowledge-pack authority, supplier qualification, exact should-cost, \
                      risk-adjusted TCO, logistics and inventory effects. Returns one SupplyDecision \
                      with evidence, assumptions, uncertainty, counterfactuals and a typed \
                      recommendation. Orders and messages are never sent and remain authorization-\
                      gated. Unknown facts stay unavailable."
            .into(),
        input_schema: request_schema(),
        resource_class: ResourceClass::Cpu,
        // AUTHORITY (audit item 9): deterministic in-process kernel;
        // orders and messages are never sent. Read-only on every axis.
        effects: faktor_provider::ToolEffects::READ_ONLY,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx: ToolRunCtx, args| {
            let signer = signer.clone();
            let approvals = approvals.clone();
            Box::pin(async move { bounded_output(execute_loop_with(args, &signer, &approvals)?) })
        }),
    }
}

/// The `engineering_analyse` tool.
pub fn engineering_analyse_tool(
    signer: Arc<engineering_core::ReceiptSigner>,
    approvals: Arc<ApprovalAuthority>,
) -> Tool {
    Tool {
        name: ENGINEERING_ANALYSE_TOOL.into(),
        description:
            "Evaluate every supply candidate/alternate against a frozen engineering \
                      requirement set: electrical, thermal, tolerance, EMC and reliability limits \
                      with unit/dimension checks, model validity, derived falsifiable tests, the \
                      11-gate response and an analysis receipt. Deterministic and side-effect free."
                .into(),
        input_schema: request_schema(),
        resource_class: ResourceClass::Cpu,
        // AUTHORITY (audit item 9): deterministic in-process kernel;
        // orders and messages are never sent. Read-only on every axis.
        effects: faktor_provider::ToolEffects::READ_ONLY,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx: ToolRunCtx, args| {
            let signer = signer.clone();
            let approvals = approvals.clone();
            Box::pin(async move { bounded_output(execute_loop_with(args, &signer, &approvals)?) })
        }),
    }
}

/// The `engineering_verify` tool.
pub fn engineering_verify_tool(verifier: Arc<engineering_core::ReceiptVerifier>) -> Tool {
    Tool {
        name: ENGINEERING_VERIFY_TOOL.into(),
        description: "Verify a stored closed-loop result: decision and requirement-set bounds, \
                      analysis-receipt binding, recomputed result digest, 11-gate readiness and the \
                      absence of ungated order/message actions."
            .into(),
        input_schema: outcome_schema(),
        resource_class: ResourceClass::Cpu,
        // AUTHORITY (audit item 9): deterministic in-process kernel;
        // orders and messages are never sent. Read-only on every axis.
        effects: faktor_provider::ToolEffects::READ_ONLY,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx: ToolRunCtx, args| {
            let verifier = verifier.clone();
            Box::pin(async move { bounded_output(execute_verify_with(args, &verifier)?) })
        }),
    }
}

/// The `approval_request` tool: agents propose; only a human with the
/// operator key can decide through the CLI.
pub fn approval_request_tool(approvals: Arc<ApprovalAuthority>) -> Tool {
    Tool {
        name: APPROVAL_REQUEST_TOOL.into(),
        description: "Propose a consequential Engineering/Supply action for a human operator. \
                      The request is durable and typed; this tool can never approve it, and \
                      no tool in the registry can decide it. Consequential execution remains \
                      blocked until a registered human signs the exact revision/digest."
            .into(),
        input_schema: json!({
            "type": "object",
            "required": ["kind", "subject", "proposed_change", "risk", "requester_id", "bound_revision", "bound_digest", "expires_in_ms"],
            "additionalProperties": false,
            "properties": {
                "kind": {"type": "string", "enum": [
                    "engineering_constraint_relaxation","engineering_acceptance_criteria_change",
                    "engineering_proof_obligation_waiver","engineering_model_validity_override",
                    "engineering_verified_constraint_change","supply_should_cost_assumption_override",
                    "supply_supplier_qualification","supply_avl_change","supply_rfq_acceptance",
                    "supply_purchase_order","supply_supplier_message","supply_payment_terms",
                    "supply_contract_terms","supply_tariff_classification","supply_inventory_policy",
                    "supply_plan_proceed"
                ]},
                "subject": {"type": "string"},
                "evidence": {"type": "array", "items": {"type": "string"}},
                "proposed_change": {"type": "string"},
                "risk": {"type": "string", "enum": ["low","medium","high","critical"]},
                "requester_id": {"type": "string"},
                "bound_revision": {"type": "string"},
                "bound_digest": {"type": "string"},
                "expires_in_ms": {"type": "integer", "minimum": 1}
            }
        }),
        resource_class: ResourceClass::Cpu,
        // AUTHORITY (audit item 9): proposes a DURABLE human-approval
        // request (an out-of-model store append). Never rides an Ask.
        effects: faktor_provider::ToolEffects {
            external_mutation: true,
            ..faktor_provider::ToolEffects::READ_ONLY
        },
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx: ToolRunCtx, args| {
            let approvals = approvals.clone();
            Box::pin(async move {
                let bytes = parse_input(&args)?;
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct RequestArgs {
                    kind: ApprovalKind,
                    subject: String,
                    #[serde(default)]
                    evidence: Vec<String>,
                    proposed_change: String,
                    risk: RiskLevel,
                    requester_id: String,
                    bound_revision: String,
                    bound_digest: String,
                    expires_in_ms: u64,
                }
                let args: RequestArgs = serde_json::from_slice(&bytes).map_err(|error| {
                    Error::malformed(format!("invalid approval request: {error}"))
                })?;
                if args.expires_in_ms == 0 || args.expires_in_ms > MAX_TOOL_APPROVAL_LIFETIME_MS {
                    return Err(Error::malformed(format!(
                        "expires_in_ms must be 1..={MAX_TOOL_APPROVAL_LIFETIME_MS}"
                    )));
                }
                let now = faktor_approval::unix_now_ms();
                let draft = ApprovalDraft {
                    kind: args.kind,
                    subject: args.subject,
                    evidence: args.evidence,
                    proposed_change: args.proposed_change,
                    risk: args.risk,
                    requester: Principal::new(args.requester_id, PrincipalKind::Agent)
                        .map_err(|error| Error::malformed(error.to_string()))?,
                    bound_revision: args.bound_revision,
                    bound_digest: args.bound_digest,
                    expires_at_ms: now.saturating_add(args.expires_in_ms),
                };
                let request = match approvals.propose(draft, now) {
                    Ok(request) => request,
                    // The bounded pending queue is a typed refusal, not a
                    // malformed call: the model must see the code/capacity.
                    Err(error @ ApprovalError::PendingLimit { .. }) => {
                        return bounded_output(error.to_json())
                    }
                    Err(error) => return Err(Error::malformed(error.to_string())),
                };
                bounded_output(serde_json::json!({
                    "status": "approval_pending",
                    "request": request,
                }))
            })
        }),
    }
}

/// The `approval_status` tool: read-only durable status.
pub fn approval_status_tool(approvals: Arc<ApprovalAuthority>) -> Tool {
    Tool {
        name: APPROVAL_STATUS_TOOL.into(),
        description: "Read one durable human-approval request and decision. Read-only: it \
                      cannot create, approve or deny anything."
            .into(),
        input_schema: json!({
            "type": "object",
            "required": ["request_id"],
            "additionalProperties": false,
            "properties": {"request_id": {"type": "string"}}
        }),
        resource_class: ResourceClass::Cpu,
        // AUTHORITY (audit item 9): deterministic in-process kernel;
        // orders and messages are never sent. Read-only on every axis.
        effects: faktor_provider::ToolEffects::READ_ONLY,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx: ToolRunCtx, args| {
            let approvals = approvals.clone();
            Box::pin(async move {
                let request_id = args
                    .get("request_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::malformed("request_id is required"))?;
                let status = approvals
                    .status(request_id)
                    .map_err(|error| Error::malformed(error.to_string()))?;
                bounded_output(serde_json::to_value(status).map_err(|error| {
                    Error::internal(format!("cannot encode approval status: {error}"))
                })?)
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_agent::ToolRegistry;
    use faktor_core::model::{ModelCapabilities, RouterPhase};

    fn builtin_registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(crate::tools::read_file_tool());
        tools.register(crate::tools::write_file_tool());
        tools.register(crate::tools::edit_file_tool());
        tools.register(crate::tools::list_dir_tool());
        tools.register(crate::tools::search_tool());
        tools.register(crate::tools::run_command_tool());
        tools
    }

    /// Audit item 9: the loop/approval seeds declare authority from ACTUAL
    /// behavior — the deterministic kernels and the read-only status read
    /// as conversational-clean; the durable approval REQUEST (an
    /// out-of-model store append) is mutating and never rides an Ask; the
    /// builtin core follows the same rule as in `crate::tools`.
    #[test]
    fn loop_and_approval_effect_seeds_declare_authority_from_actual_behavior() {
        let (registry, _dir) = loop_registry();
        for name in [
            SUPPLY_DECISION_TOOL,
            ENGINEERING_ANALYSE_TOOL,
            ENGINEERING_VERIFY_TOOL,
            APPROVAL_STATUS_TOOL,
        ] {
            assert!(
                registry
                    .get(name)
                    .expect("registered")
                    .effects
                    .is_read_only(),
                "{name} must be seeded read-only"
            );
        }
        let approval = registry.get(APPROVAL_REQUEST_TOOL).expect("registered");
        assert!(
            approval.effects.external_mutation,
            "proposing a durable approval request mutates an out-of-model store"
        );
        assert!(
            !approval.effects.is_read_only(),
            "approval_request must never ride an Ask"
        );
        assert!(registry
            .get(crate::tools::read_file_tool().name.as_str())
            .expect("registered")
            .effects
            .is_read_only());
        assert!(!registry
            .get(crate::tools::write_file_tool().name.as_str())
            .expect("registered")
            .effects
            .is_read_only());
        assert!(
            registry
                .get(crate::tools::run_command_tool().name.as_str())
                .expect("registered")
                .effects
                .process_exec
        );
    }

    fn loop_registry() -> (ToolRegistry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = builtin_registry();
        register(
            &mut tools,
            Arc::new(engineering_core::ReceiptSigner::generate()),
            Arc::new(engineering_core::ReceiptVerifier::self_attested()),
            Arc::new(ApprovalAuthority::open(dir.path()).unwrap()),
        );
        (tools, dir)
    }

    #[test]
    fn loop_tools_are_lazy_and_an_unactivated_turn_advertises_none_of_them() {
        let (registry, _dir) = loop_registry();
        for name in [
            SUPPLY_DECISION_TOOL,
            ENGINEERING_ANALYSE_TOOL,
            ENGINEERING_VERIFY_TOOL,
            APPROVAL_REQUEST_TOOL,
            APPROVAL_STATUS_TOOL,
        ] {
            assert!(
                registry.is_lazy(name),
                "{name} must not carry its complete schema on every turn"
            );
        }
        let caps = ModelCapabilities {
            tools: true,
            ..Default::default()
        };
        let bundle = registry.bundle_for_phase(RouterPhase::Implement, &caps);
        for name in [
            SUPPLY_DECISION_TOOL,
            ENGINEERING_ANALYSE_TOOL,
            ENGINEERING_VERIFY_TOOL,
            APPROVAL_REQUEST_TOOL,
            APPROVAL_STATUS_TOOL,
        ] {
            assert!(
                !bundle.tool_names().contains(&name),
                "an unactivated Implement turn must not advertise {name}: {:?}",
                bundle.tool_names()
            );
        }
        let budget = faktor_context::budget::ContextBudget::for_capabilities(&caps).context_max();
        let tokens = faktor_context::wire_plan::measure_wire_request("", &[], &bundle.tools);
        assert!(
            tokens <= budget,
            "the ordinary Implement bundle is {tokens} tokens against a {budget} budget"
        );
    }

    /// The complete schemas are intentionally large (the request schema has
    /// 174 definitions and the outcome schema 173); each must still fit the
    /// 32K profile's context maximum when it is the only activated
    /// engineering tool. If this ever stops holding, the surface must be
    /// split — never trimmed into an open placeholder.
    #[test]
    fn each_complete_loop_schema_fits_the_small_local_context_alone() {
        let max = faktor_context::budget::ContextBudget::for_capabilities(
            &ModelCapabilities::small_local(),
        )
        .context_max();
        let dir = tempfile::tempdir().unwrap();
        let approvals = Arc::new(ApprovalAuthority::open(dir.path()).unwrap());
        for tool in [
            supply_decision_tool(
                Arc::new(engineering_core::ReceiptSigner::generate()),
                approvals.clone(),
            ),
            engineering_analyse_tool(
                Arc::new(engineering_core::ReceiptSigner::generate()),
                approvals,
            ),
            engineering_verify_tool(Arc::new(engineering_core::ReceiptVerifier::self_attested())),
        ] {
            let tokens = faktor_context::wire_plan::measure_wire_request("", &[], &[tool.spec()]);
            assert!(
                tokens <= max,
                "the complete {} schema needs {tokens} tokens but the 32K profile holds {max}",
                tool.name
            );
        }
    }

    #[test]
    fn the_two_request_tools_have_disjoint_activation_signals() {
        fn signals(exposure: faktor_agent::ToolExposure) -> Vec<String> {
            exposure
                .triggers()
                .iter()
                .filter_map(|trigger| match trigger {
                    faktor_agent::ToolTrigger::Signal(phrase) => Some(phrase.clone()),
                    _ => None,
                })
                .collect()
        }
        let supply = signals(supply_decision_exposure());
        let analysis = signals(engineering_analyse_exposure());
        for phrase in &supply {
            assert!(
                !analysis.contains(phrase),
                "{phrase:?} activates both complete request schemas on one turn"
            );
        }
    }

    #[test]
    fn malformed_loop_input_is_refused() {
        let error = execute_loop(json!({"not": "a loop request"})).unwrap_err();
        assert!(error.to_string().contains("invalid loop request"));
    }

    #[test]
    fn malformed_verification_input_is_refused() {
        let verifier =
            engineering_core::ReceiptVerifier::trusting_key_id(&"11".repeat(32)).unwrap();
        let error = execute_verify_with(json!({"not": "an outcome"}), &verifier).unwrap_err();
        assert!(error.to_string().contains("invalid loop outcome"));
    }

    #[test]
    fn tool_schemas_are_generated_from_the_authoritative_types() {
        let request = request_schema();
        assert_eq!(
            request,
            faktor_supply_engineering::tool_schema::loop_request_schema(),
            "the Supply/Engineering request tools must expose the generated LoopRequest schema"
        );
        assert_eq!(request["type"], "object");
        assert_eq!(request["additionalProperties"], false);
        assert_eq!(request["properties"]["candidates"]["minItems"], 1);
        assert!(request["properties"].get("decision_id").is_some());
        assert!(request["properties"].get("engineering").is_some());
        assert!(request["properties"].get("inventory").is_some());
        assert!(request["properties"].get("negotiation").is_some());

        let outcome = outcome_schema();
        assert_eq!(
            outcome,
            faktor_supply_engineering::tool_schema::loop_outcome_schema(),
            "engineering_verify must expose the generated LoopOutcome schema"
        );
        assert_eq!(outcome["additionalProperties"], false);
        assert!(outcome["properties"].get("decision").is_some());
        assert!(outcome["properties"].get("receipt").is_some());
        assert!(outcome["properties"].get("response").is_some());

        // Compact but complete: the full graph stays below the tool metadata
        // budget instead of being replaced by an open placeholder.
        for (name, schema) in [("request", &request), ("outcome", &outcome)] {
            let bytes = serde_json::to_vec(schema).unwrap().len();
            assert!(
                bytes < 160 * 1024,
                "{name} schema is {bytes} bytes; keep it compact"
            );
        }
    }

    /// Measured token economics of the lazy surface: the unactivated
    /// Implement bundle is byte-identical to a daemon without the loop
    /// tools, and one supply/analysis/verify signal activates exactly ONE
    /// complete schema. Numbers are real `cl100k_base` BPE counts (the
    /// local OpenAI vocabulary; Ollama prompt_eval_count is measured
    /// separately) plus the runtime's conservative estimator.
    #[test]
    fn lazy_activation_adds_one_complete_schema_and_zero_on_ordinary_turns() {
        use faktor_provider::TokenizerId;
        let (registry, _dir) = loop_registry();
        let baseline = builtin_registry();
        let caps = ModelCapabilities {
            tools: true,
            ..Default::default()
        };
        let ordinary = registry.bundle_for_phase(RouterPhase::Implement, &caps);
        let reference = baseline.bundle_for_phase(RouterPhase::Implement, &caps);
        assert_eq!(
            serde_json::to_vec(&ordinary.tools).unwrap(),
            serde_json::to_vec(&reference.tools).unwrap(),
            "an unactivated loop tool must not change one bundle byte"
        );

        let tokenizers = faktor_context::TokenizerRegistry::with_builtin_backends();
        let count = |specs: &[faktor_provider::ToolSpec]| {
            let text = serde_json::to_string(specs).unwrap();
            let exact = tokenizers.count(TokenizerId::CL100K_BASE, &text).count;
            let conservative = faktor_context::Estimator.estimate_tokens(&text);
            (text.len(), exact, conservative)
        };
        let (base_bytes, base_exact, base_est) = count(&ordinary.tools);

        for (phrase, expected) in [
            (
                "choose a supplier and run the supply decision",
                SUPPLY_DECISION_TOOL,
            ),
            (
                "run an engineering analysis on the tolerance stack",
                ENGINEERING_ANALYSE_TOOL,
            ),
            ("verify the loop analysis receipt", ENGINEERING_VERIFY_TOOL),
        ] {
            let activation =
                registry.activation_for_text(&faktor_agent::ToolActivationSet::new(), phrase);
            let active = registry.bundle_for_phase_with_activation(
                RouterPhase::Implement,
                &caps,
                &activation,
            );
            assert!(
                active.tool_names().contains(&expected),
                "{phrase:?} must activate {expected}: {:?}",
                active.tool_names()
            );
            let request_tools = [SUPPLY_DECISION_TOOL, ENGINEERING_ANALYSE_TOOL]
                .iter()
                .filter(|name| active.tool_names().contains(*name))
                .count();
            assert!(
                request_tools <= 1,
                "one turn must never advertise two complete request schemas: {:?}",
                active.tool_names()
            );
            let (bytes, exact, est) = count(&active.tools);
            eprintln!(
                "TOOLBURN phrase={phrase:?} base={base_bytes}B/{base_exact}exact/{base_est}est active={bytes}B/{exact}exact/{est}est delta={}B/{}exact/{}est",
                bytes - base_bytes,
                exact - base_exact,
                est - base_est
            );
            assert!(exact > base_exact, "activation must add schema tokens");
        }
    }
}
