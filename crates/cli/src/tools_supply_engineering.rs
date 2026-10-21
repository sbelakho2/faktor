//! Daemon tools for the closed Engineering -> Supply loop.
//!
//! The tools are thin, bounded adapters over `faktor-supply-engineering`:
//! they parse one JSON object, run the deterministic kernel and return the
//! typed result as JSON. They make no model calls, touch no disk or network,
//! and never send an RFQ/message or create an order.

use std::sync::Arc;

use faktor_agent::{RecoveryHint, Tool, ToolOutcome, ToolRunCtx};
use faktor_core::error::Error;
use faktor_core::resource::ResourceClass;
use serde_json::{json, Value};

/// Tool name: run the closed loop and emit a SupplyDecision.
pub const SUPPLY_DECISION_TOOL: &str = "supply_decision";
/// Tool name: engineering analysis of every candidate.
pub const ENGINEERING_ANALYSE_TOOL: &str = "engineering_analyse";
/// Tool name: verify a stored loop result.
pub const ENGINEERING_VERIFY_TOOL: &str = "engineering_verify";

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

pub(crate) fn execute_loop(args: Value) -> Result<Value, Error> {
    let bytes = parse_input(&args)?;
    let request: faktor_supply_engineering::LoopRequest = serde_json::from_slice(&bytes)
        .map_err(|error| Error::malformed(format!("invalid loop request: {error}")))?;
    let outcome = faktor_supply_engineering::run(&request)
        .map_err(|error| Error::malformed(format!("loop refused: {error}")))?;
    serde_json::to_value(outcome)
        .map_err(|error| Error::internal(format!("cannot encode loop outcome: {error}")))
}

pub(crate) fn execute_verify(args: Value) -> Result<Value, Error> {
    let bytes = parse_input(&args)?;
    let outcome: faktor_supply_engineering::LoopOutcome = serde_json::from_slice(&bytes)
        .map_err(|error| Error::malformed(format!("invalid loop outcome: {error}")))?;
    let verification = outcome
        .verify()
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
pub(crate) fn register(registry: &mut faktor_agent::ToolRegistry) {
    registry.register(supply_decision_tool());
    registry.register(engineering_analyse_tool());
    registry.register(engineering_verify_tool());
}

fn schema() -> Value {
    json!({
        "type": "object",
        "description": "A typed Faktor Supply/Engineering JSON document. See docs/specs/supply-engineering.md; unknown fields are refused.",
        "additionalProperties": true
    })
}

/// The `supply_decision` tool.
pub fn supply_decision_tool() -> Tool {
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
        input_schema: schema(),
        resource_class: ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx: ToolRunCtx, args| {
            Box::pin(async move { bounded_output(execute_loop(args)?) })
        }),
    }
}

/// The `engineering_analyse` tool.
pub fn engineering_analyse_tool() -> Tool {
    Tool {
        name: ENGINEERING_ANALYSE_TOOL.into(),
        description:
            "Evaluate every supply candidate/alternate against a frozen engineering \
                      requirement set: electrical, thermal, tolerance, EMC and reliability limits \
                      with unit/dimension checks, model validity, derived falsifiable tests, the \
                      11-gate response and an analysis receipt. Deterministic and side-effect free."
                .into(),
        input_schema: schema(),
        resource_class: ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx: ToolRunCtx, args| {
            Box::pin(async move { bounded_output(execute_loop(args)?) })
        }),
    }
}

/// The `engineering_verify` tool.
pub fn engineering_verify_tool() -> Tool {
    Tool {
        name: ENGINEERING_VERIFY_TOOL.into(),
        description: "Verify a stored closed-loop result: decision and requirement-set bounds, \
                      analysis-receipt binding, recomputed result digest, 11-gate readiness and the \
                      absence of ungated order/message actions."
            .into(),
        input_schema: schema(),
        resource_class: ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx: ToolRunCtx, args| {
            Box::pin(async move { bounded_output(execute_verify(args)?) })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_loop_input_is_refused() {
        let error = execute_loop(json!({"not": "a loop request"})).unwrap_err();
        assert!(error.to_string().contains("invalid loop request"));
    }

    #[test]
    fn malformed_verification_input_is_refused() {
        let error = execute_verify(json!({"not": "an outcome"})).unwrap_err();
        assert!(error.to_string().contains("invalid loop outcome"));
    }
}
