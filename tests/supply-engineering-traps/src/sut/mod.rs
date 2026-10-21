//! The benchmark SUT.
//!
//! Engineering cases submit a fixture request to the real pipeline seam
//! (`engineering_core::run_engineering_pipeline`), which performs
//! requirement validation, authoritative model selection, kernel execution,
//! dimensional evaluation and the 11-gate response. Supply cases execute the
//! real supply cost/logistics/unit kernels. Both return an opaque
//! [`SutAnswer`]; neither can set an oracle verdict or a metric boolean.

mod engineering;
mod kernel;
mod supply;

use engineering_core::KernelStatus;
use engineering_core::{
    AuthorityRef, EngineeringResponse, FalsifiableTest, GateState, PipelineOutcome, PipelineRun,
    ReceiptValue,
};

use crate::contract::{
    ControlAnswer, GateRecord, GateStateKind, Observation, SutAnswer, SutOutcome, SutUncertainty,
};
use crate::BenchError;

/// Run one case through the SUT.
pub(crate) fn run(id: &str) -> Result<SutAnswer, BenchError> {
    match id {
        engineering::OPAMP_ID
        | engineering::LARGE_SIGNAL_ID
        | engineering::FLYBACK_ID
        | engineering::PCB_ID
        | engineering::ALUMINIUM_ID
        | engineering::FASTENER_ID
        | engineering::FATIGUE_ID
        | engineering::THERMAL_ID
        | engineering::MOLDING_ID
        | engineering::FEA_ID
        | engineering::EMC_ID => engineering::run(id),
        supply::SUPPLIER_ID | supply::HS_ID | supply::UNIT_ID | supply::UNKNOWN_COST_ID => {
            supply::run(id)
        }
        other => Err(BenchError::UnknownCase(other.to_string())),
    }
}

/// Build the opaque answer from a pipeline run. Gate states come from the
/// response the pipeline actually assessed; observations come from executed
/// receipts.
fn answer_from_run(
    id: &str,
    run: &PipelineRun,
    authority: &[AuthorityRef],
    experiments: Vec<FalsifiableTest>,
    controls: Vec<ControlAnswer>,
) -> SutAnswer {
    let observations = observations(&run.receipts);
    let outcome = match &run.outcome {
        PipelineOutcome::Solved => SutOutcome::Solved { observations },
        PipelineOutcome::Rejected { code, reasons } => SutOutcome::Rejected {
            code: code.clone(),
            reasons: reasons.clone(),
            observations,
        },
        PipelineOutcome::Refused {
            code,
            reason,
            required,
        } => SutOutcome::Refused {
            code: code.clone(),
            reason: reason.clone(),
            required: required.clone(),
        },
        PipelineOutcome::ResearchRequired {
            gate,
            reason,
            sources_required,
        } => SutOutcome::ResearchRequired {
            gate: *gate,
            reason: reason.clone(),
            sources_required: sources_required.clone(),
        },
        PipelineOutcome::Ambiguous {
            candidates,
            reason,
            sources_required,
        } => SutOutcome::Ambiguous {
            candidates: candidates.clone(),
            reason: reason.clone(),
            authority: sources_required.join("; "),
        },
        PipelineOutcome::Unavailable { reason, required } => SutOutcome::Unavailable {
            reason: reason.clone(),
            required: required.clone(),
        },
    };
    let uncertainty = run_uncertainty(run, &outcome);
    let mut missing_info = match &outcome {
        SutOutcome::ResearchRequired {
            sources_required, ..
        } => sources_required.clone(),
        SutOutcome::Refused { required, .. } | SutOutcome::Unavailable { required, .. } => {
            required.clone()
        }
        _ => Vec::new(),
    };
    if let GateState::Established(formulation) = &run.response.formulation.state {
        missing_info.extend(formulation.unknowns.clone());
    }
    let mut evidence = authority
        .iter()
        .map(|authority| {
            format!(
                "knowledge authority {}@{} digest {} scope {}",
                authority.pack_id,
                authority.version,
                authority.digest.to_hex(),
                authority.scope
            )
        })
        .collect::<Vec<_>>();
    evidence.extend(
        run.receipts
            .iter()
            .map(|receipt| {
                format!(
                    "executed receipt {}@{} digest {}",
                    receipt.identity.solver_id,
                    receipt.identity.solver_version,
                    receipt
                        .results_digest()
                        .map_or_else(|_| "<invalid>".to_string(), |digest| digest.to_hex())
                )
            })
            .collect::<Vec<_>>(),
    );
    let mut coverage = authority
        .iter()
        .map(|authority| authority.pack_id.clone())
        .collect::<Vec<_>>();
    for model in run.selection.unique_models() {
        if let Some(pack) = &model.pack_id {
            coverage.push(pack.clone());
        }
        coverage.extend(model.equation_ids.clone());
        coverage.extend(model.validity_domain_ids.clone());
    }
    let gate_states = gate_states(&run.response);
    SutAnswer {
        id: id.to_string(),
        outcome,
        gate_states,
        missing_info,
        boundary_conditions: run.boundary_conditions.clone(),
        uncertainty,
        assumption_coverage: coverage,
        evidence,
        experiments,
        controls,
    }
}

fn observations(receipts: &[engineering_core::AnalysisReceipt]) -> Vec<Observation> {
    let mut observations = Vec::new();
    for receipt in receipts {
        for result in &receipt.results {
            match &result.value {
                ReceiptValue::Quantity(quantity) => observations.push(Observation::quantity(
                    result.name.clone(),
                    quantity.value(),
                    quantity.unit().symbol(),
                )),
                ReceiptValue::Text(text) => {
                    observations.push(Observation::text(result.name.clone(), text.clone()))
                }
                ReceiptValue::Unavailable(_) => {}
            }
        }
    }
    observations
}

fn run_uncertainty(run: &PipelineRun, outcome: &SutOutcome) -> SutUncertainty {
    if matches!(
        outcome,
        SutOutcome::ResearchRequired { .. }
            | SutOutcome::Refused { .. }
            | SutOutcome::Unavailable { .. }
            | SutOutcome::Ambiguous { .. }
    ) {
        return SutUncertainty::NotEstimated {
            reason: "no bounded numerical result was established".to_string(),
        };
    }
    match &run.response.uncertainty.state {
        GateState::Established(statement) => match statement.summary {
            Some(interval) => SutUncertainty::Interval {
                lower: interval.lower(),
                upper: interval.upper(),
            },
            None => SutUncertainty::Exact,
        },
        _ => SutUncertainty::NotEstimated {
            reason: "the pipeline did not establish an uncertainty statement".to_string(),
        },
    }
}

fn gate_states(response: &EngineeringResponse) -> Vec<GateRecord> {
    let mut states = Vec::with_capacity(11);
    let mut push = |gate, state: GateStateKind| {
        states.push(GateRecord { gate, state });
    };
    push(
        engineering_core::GateId::Formulation,
        state_of(&response.formulation),
    );
    push(
        engineering_core::GateId::Authority,
        state_of(&response.authority),
    );
    push(engineering_core::GateId::Theory, state_of(&response.theory));
    push(
        engineering_core::GateId::Validity,
        state_of(&response.validity),
    );
    push(
        engineering_core::GateId::Dimensions,
        state_of(&response.dimensions),
    );
    push(
        engineering_core::GateId::ExecutedAnalysis,
        state_of(&response.executed_analysis),
    );
    push(
        engineering_core::GateId::CrossCheck,
        state_of(&response.cross_check),
    );
    push(
        engineering_core::GateId::Uncertainty,
        state_of(&response.uncertainty),
    );
    push(
        engineering_core::GateId::CounterexampleSearch,
        state_of(&response.counterexample_search),
    );
    push(
        engineering_core::GateId::Realization,
        state_of(&response.realization),
    );
    push(
        engineering_core::GateId::Verification,
        state_of(&response.verification),
    );
    states
}

fn state_of<T>(gate: &engineering_core::Gate<T>) -> GateStateKind {
    match &gate.state {
        GateState::Established(_) => GateStateKind::Established,
        GateState::Unavailable(_) => GateStateKind::Unavailable,
        GateState::ResearchRequired(_) => GateStateKind::ResearchRequired,
    }
}

fn control_from_report(
    name: &str,
    report: &engineering_core::KernelReport,
    uncertainty: SutUncertainty,
) -> ControlAnswer {
    let outcome = match report.status {
        KernelStatus::Solved => SutOutcome::Solved {
            observations: Vec::new(),
        },
        KernelStatus::Refused => SutOutcome::Refused {
            code: report
                .refusal
                .as_ref()
                .map(|refusal| refusal.code.clone())
                .unwrap_or_else(|| "Refused".to_string()),
            reason: report
                .refusal
                .as_ref()
                .map(|refusal| refusal.reason.clone())
                .unwrap_or_else(|| "kernel refused".to_string()),
            required: report
                .refusal
                .as_ref()
                .map(|refusal| refusal.required.clone())
                .unwrap_or_default(),
        },
    };
    ControlAnswer {
        name: name.to_string(),
        outcome,
        uncertainty,
    }
}

fn report_uncertainty(report: &engineering_core::KernelReport) -> SutUncertainty {
    if report.status == KernelStatus::Refused {
        return SutUncertainty::NotEstimated {
            reason: "the kernel refused and established no numerical value".to_string(),
        };
    }
    match report.uncertainty.interval() {
        Some(interval) => SutUncertainty::Interval {
            lower: interval.lower(),
            upper: interval.upper(),
        },
        None => SutUncertainty::Exact,
    }
}
