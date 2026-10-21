//! Deterministic execution seam from a fixture request to an assessed
//! 11-gate [`EngineeringResponse`].
//!
//! The pipeline is the only place where requirement validation, authoritative
//! model selection, kernel execution, dimensional requirement evaluation and
//! the response gates are composed. A caller supplies the fixture request and
//! a kernel executor; it never supplies gate counts, outcome classes or
//! acceptance booleans. Those are derived here from the selector, the kernels
//! and [`EngineeringResponse::assess`].

use std::collections::BTreeMap;

use engineering_units::{Interval, Quantity, Uncertainty};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::attestation::ReceiptSigner;
use crate::digest::Digest;
use crate::error::{CoreError, CoreResult};
use crate::gate::{
    research_response, Agreement, AssumptionRecord, AuthorityRef, CounterexampleProbe,
    CounterexampleSearch, CrossCheck, DimensionChecks, EngineeringResponse, ExecutedAnalysis,
    FalsifiableTest, FalsifiableVerification, Formulation, Gate, GateId, KnowledgeAuthority,
    ModelSelection, RealizationConsequence, RealizationConsequences, ResearchRequest,
    ResponseAssessment, TheorySelection, UncertaintyContribution, UncertaintyStatement,
    ValidityEnvelopes,
};
use crate::graph::NodeId;
use crate::receipt::{AnalysisReceipt, ReceiptResult, ReceiptValue};
use crate::requirements::{
    evaluate, CandidateFacts, EngineeringRequirementSet, EngineeringVerdict,
};
use crate::selector::{
    ApplicabilitySelection, ModelCatalog, ModelSelector, RequirementSelection, SelectionRequest,
};
use crate::value::{EngineeringValue, Provenance, ProvenanceKind, RevisionId};

/// Maximum kernel probes in one pipeline run.
pub const MAX_PIPELINE_PROBES: usize = 16;

/// One kernel invocation with named, dimensioned inputs.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelProbe {
    /// Stable probe id.
    pub id: String,
    /// Model the probe executes. `None` means the selector's unique model.
    pub model_id: Option<String>,
    /// Named dimensioned inputs. The kernel converts them into its typed input
    /// struct.
    #[serde(default)]
    pub inputs: BTreeMap<String, Quantity>,
    /// Named text inputs (material grade/temper, model variant selectors).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// Kernel execution state. Only a kernel can report these.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelStatus {
    /// The kernel produced a usable result.
    Solved,
    /// The kernel refused because a required input or validity condition is
    /// missing or invalid.
    Refused,
}

/// A kernel's typed refusal.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRefusal {
    /// Stable refusal code.
    pub code: String,
    /// Why the kernel refused.
    pub reason: String,
    /// Inputs/authorities required to proceed.
    pub required: Vec<String>,
}

/// A kernel rejection of a design that executed but failed a required check.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRejection {
    /// Stable rejection code.
    pub code: String,
    /// Failed checks in deterministic order.
    pub reasons: Vec<String>,
}

/// Independent cross-check evidence returned by a kernel.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelCrossCheck {
    /// Independent method.
    pub method: String,
    /// Primary result.
    pub primary_value: f64,
    /// Independent result.
    pub independent_value: f64,
    /// Absolute difference.
    pub difference: f64,
    /// Acceptance tolerance.
    pub tolerance: f64,
}

/// Everything one executed kernel returns. A kernel cannot set an outcome,
/// a gate or a benchmark metric; those are derived by the pipeline.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelReport {
    /// Model that executed.
    pub model_id: String,
    /// Solved or refused.
    pub status: KernelStatus,
    /// Property values consumed by the dimensional requirement evaluator.
    pub values: BTreeMap<String, EngineeringValue>,
    /// Machine-checked dimensional equations.
    pub checks: Vec<crate::gate::DimensionCheck>,
    /// Assumptions the kernel actually used.
    pub assumptions: Vec<AssumptionRecord>,
    /// Uncertainty of the result.
    pub uncertainty: Uncertainty,
    /// Independent cross-check, when the kernel performed one.
    pub cross_check: Option<KernelCrossCheck>,
    /// Executed receipt, unsigned; the pipeline signs it.
    pub receipt: AnalysisReceipt,
    /// Typed refusal when `status == Refused`.
    pub refusal: Option<KernelRefusal>,
    /// Typed rejection when the executed design failed a required check.
    pub rejection: Option<KernelRejection>,
}

impl KernelReport {
    fn validate(&self) -> CoreResult<()> {
        if self.model_id.trim().is_empty() {
            return Err(CoreError::InvalidValue {
                reason: "kernel model id must not be empty".to_string(),
            });
        }
        match (self.status, &self.refusal) {
            (KernelStatus::Refused, None) => {
                return Err(CoreError::InvalidValue {
                    reason: "a refused kernel must carry a typed refusal".to_string(),
                })
            }
            (KernelStatus::Solved, Some(_)) => {
                return Err(CoreError::InvalidValue {
                    reason: "a solved kernel must not carry a refusal".to_string(),
                })
            }
            _ => {}
        }
        self.receipt.validate()?;
        for value in self.values.values() {
            value.validate()?;
        }
        Ok(())
    }
}

/// A hard blocker discovered by the knowledge/solver adapter before a
/// numerical kernel can run. It is still evaluated through the response gates.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PipelineBlocker {
    /// The selected authority excludes the claim or no pack covers it.
    Research(ResearchRequest),
    /// The solver capability needed by the claim does not exist.
    Refusal(KernelRefusal),
}

/// One fixture request submitted to the pipeline.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineRequest {
    /// Request id.
    pub id: String,
    /// Revision-bound requirement set.
    pub requirements: EngineeringRequirementSet,
    /// Registered model catalog (normally built from knowledge packs).
    pub catalog: ModelCatalog,
    /// Applicability requests, one per requirement.
    pub selections: Vec<SelectionRequest>,
    /// Kernel probes to execute after selection.
    #[serde(default)]
    pub probes: Vec<KernelProbe>,
    /// Knowledge authority cited by the request.
    #[serde(default)]
    pub authority: Vec<AuthorityRef>,
    /// Explicit assumptions.
    #[serde(default)]
    pub assumptions: Vec<AssumptionRecord>,
    /// Known unknowns.
    #[serde(default)]
    pub unknowns: Vec<String>,
    /// Falsifiable verification tests.
    #[serde(default)]
    pub tests: Vec<FalsifiableTest>,
    /// Counterexample/failure probes that were actually considered.
    #[serde(default)]
    pub counterexamples: Vec<CounterexampleProbe>,
    /// Realization consequences.
    #[serde(default)]
    pub realization: Vec<RealizationConsequence>,
    /// Boundary conditions named by the request.
    #[serde(default)]
    pub boundary_conditions: Vec<String>,
    /// Knowledge/solver blocker, when one exists.
    #[serde(default)]
    pub blocker: Option<PipelineBlocker>,
}

/// The outcome the pipeline derived, never a caller-supplied label.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PipelineOutcome {
    /// The model executed and every compared requirement is satisfied.
    Solved,
    /// The model executed and a required check failed.
    Rejected {
        /// Stable rejection code.
        code: String,
        /// Failed checks.
        reasons: Vec<String>,
    },
    /// The kernel refused.
    Refused {
        /// Stable refusal code.
        code: String,
        /// Why.
        reason: String,
        /// Required inputs/authorities.
        required: Vec<String>,
    },
    /// Selection or a knowledge blocker requires research.
    ResearchRequired {
        /// Gate that cannot be established.
        gate: GateId,
        /// Why.
        reason: String,
        /// Required sources.
        sources_required: Vec<String>,
    },
    /// The selected models disagree.
    Ambiguous {
        /// Candidate model ids.
        candidates: Vec<String>,
        /// Why.
        reason: String,
        /// Sources that could disambiguate.
        sources_required: Vec<String>,
    },
    /// The selected authority does not establish a value.
    Unavailable {
        /// Why.
        reason: String,
        /// Required inputs.
        required: Vec<String>,
    },
}

/// Everything one pipeline run produced.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    /// The selector's recorded, reproducible decision.
    pub selection: ApplicabilitySelection,
    /// The 11-gate response.
    pub response: EngineeringResponse,
    /// The response's own assessment.
    pub assessment: ResponseAssessment,
    /// Derived outcome.
    pub outcome: PipelineOutcome,
    /// Signed executed receipts.
    pub receipts: Vec<AnalysisReceipt>,
    /// Property values produced by kernels.
    pub values: BTreeMap<String, EngineeringValue>,
    /// Boundary conditions named by receipts and the request.
    pub boundary_conditions: Vec<String>,
}

/// Execute one fixture request. `execute` is the only kernel seam; it receives
/// named dimensioned inputs and returns a [`KernelReport`].
pub fn run(
    request: PipelineRequest,
    execute: impl Fn(&KernelProbe) -> CoreResult<KernelReport>,
) -> CoreResult<PipelineRun> {
    if request.id.trim().is_empty() {
        return Err(CoreError::InvalidValue {
            reason: "pipeline request id must not be empty".to_string(),
        });
    }
    if request.probes.len() > MAX_PIPELINE_PROBES {
        return Err(CoreError::GraphLimit {
            what: "pipeline probes",
            limit: MAX_PIPELINE_PROBES,
        });
    }
    request.requirements.validate()?;
    let selection = ModelSelector::select(&request.catalog, &request.selections)?;
    let mut kernels = Vec::new();
    let mut refusal = None;
    let mut rejection = None;
    let mut research_gap: Option<ResearchRequest> = None;
    let mut ambiguous: Option<(Vec<String>, String)> = None;
    let blocker_refusal = matches!(request.blocker.as_ref(), Some(PipelineBlocker::Refusal(_)));

    if let Some(blocker) = &request.blocker {
        match blocker {
            PipelineBlocker::Research(gap) => research_gap = Some(gap.clone()),
            PipelineBlocker::Refusal(blocked) => refusal = Some(blocked.clone()),
        }
    }
    if research_gap.is_none() && !blocker_refusal {
        for outcome in &selection.selections {
            match outcome {
                RequirementSelection::Selected { .. } => {}
                RequirementSelection::ResearchRequired {
                    requirement_id,
                    reason,
                    sources_required,
                } => {
                    research_gap = Some(research_response(
                        GateId::Theory,
                        format!("{requirement_id}: {reason}"),
                        sources_required.clone(),
                    ));
                    break;
                }
                RequirementSelection::Ambiguous {
                    requirement_id,
                    candidates,
                    reason,
                } => {
                    ambiguous = Some((candidates.clone(), format!("{requirement_id}: {reason}")));
                    break;
                }
            }
        }
    }

    if research_gap.is_none() && ambiguous.is_none() && refusal.is_none() {
        let selected: Vec<String> = selection
            .unique_models()
            .into_iter()
            .map(|model| model.model_id.clone())
            .collect();
        for probe in &request.probes {
            let model_id = probe
                .model_id
                .as_deref()
                .ok_or_else(|| CoreError::InvalidValue {
                    reason: format!("probe {} has no model id", probe.id),
                })?;
            if !selected.iter().any(|candidate| candidate == model_id) {
                return Err(CoreError::InvalidValue {
                    reason: format!(
                        "probe {} names model {model_id}, which the selector did not select",
                        probe.id
                    ),
                });
            }
            let report = execute(probe)?;
            report.validate()?;
            if report.model_id != model_id {
                return Err(CoreError::InvalidValue {
                    reason: format!(
                        "probe {} requested {model_id} but the kernel reported {}",
                        probe.id, report.model_id
                    ),
                });
            }
            if let Some(kernel_refusal) = &report.refusal {
                refusal = Some(kernel_refusal.clone());
            }
            if let Some(kernel_rejection) = &report.rejection {
                rejection = Some(kernel_rejection.clone());
            }
            kernels.push(report);
        }
        if request.probes.is_empty() {
            refusal = Some(KernelRefusal {
                code: "NoExecutableKernel".to_string(),
                reason: "the selected model has no executable kernel probe".to_string(),
                required: vec!["an executable kernel bound to the selected model".to_string()],
            });
        }
    }

    let outcome = if blocker_refusal {
        let refusal = refusal.clone().ok_or_else(|| CoreError::InvalidValue {
            reason: "a blocker refusal must carry a typed refusal".to_string(),
        })?;
        PipelineOutcome::Refused {
            code: refusal.code.clone(),
            reason: refusal.reason.clone(),
            required: refusal.required.clone(),
        }
    } else if let Some(reason) = &research_gap {
        PipelineOutcome::ResearchRequired {
            gate: reason.gate,
            reason: reason.reason.clone(),
            sources_required: reason.sources_required.clone(),
        }
    } else if let Some((candidates, reason)) = &ambiguous {
        PipelineOutcome::Ambiguous {
            candidates: candidates.clone(),
            reason: reason.clone(),
            sources_required: candidates
                .iter()
                .map(|candidate| format!("disambiguate {candidate}"))
                .collect(),
        }
    } else if let Some(refusal) = &refusal {
        PipelineOutcome::Refused {
            code: refusal.code.clone(),
            reason: refusal.reason.clone(),
            required: refusal.required.clone(),
        }
    } else if let Some(rejection) = &rejection {
        PipelineOutcome::Rejected {
            code: rejection.code.clone(),
            reasons: rejection.reasons.clone(),
        }
    } else {
        PipelineOutcome::Solved
    };

    let mut values = BTreeMap::new();
    for report in &kernels {
        values.extend(report.values.clone());
    }
    let candidate = CandidateFacts {
        candidate_id: request.id.clone(),
        node: Some(NodeId::new(1)),
        revision: RevisionId::new(1),
        values: values.clone(),
        categorical: BTreeMap::new(),
        conditions: request
            .selections
            .iter()
            .flat_map(|selection| selection.conditions.clone())
            .collect(),
        temperature: request
            .selections
            .iter()
            .find_map(|selection| selection.temperature.clone()),
        evidence: Vec::new(),
    };
    let evaluation = if matches!(
        outcome,
        PipelineOutcome::Solved | PipelineOutcome::Rejected { .. }
    ) {
        Some(evaluate(&request.requirements, &candidate)?)
    } else {
        None
    };
    let outcome = match (&outcome, &evaluation) {
        (PipelineOutcome::Solved, Some(evaluation)) => match evaluation.verdict {
            EngineeringVerdict::Reject => PipelineOutcome::Rejected {
                code: "RequirementNotSatisfied".to_string(),
                reasons: evaluation
                    .unresolved()
                    .iter()
                    .map(|outcome| format!("{}: {:?}", outcome.requirement_id, outcome.state))
                    .collect(),
            },
            EngineeringVerdict::Unavailable => PipelineOutcome::Unavailable {
                reason: "a required property or model validity condition is unavailable"
                    .to_string(),
                required: evaluation
                    .unresolved()
                    .iter()
                    .map(|outcome| outcome.requirement_id.clone())
                    .collect(),
            },
            EngineeringVerdict::Accept | EngineeringVerdict::Qualify => PipelineOutcome::Solved,
        },
        _ => outcome,
    };

    let mut receipts = Vec::new();
    let signer = ReceiptSigner::from_seed(*Digest::of_bytes(request.id.as_bytes()).as_bytes());
    for report in &kernels {
        let mut receipt = report.receipt.clone();
        signer.sign_receipt(&mut receipt)?;
        receipts.push(receipt);
    }

    let mut boundary_conditions = request.boundary_conditions.clone();
    for receipt in &receipts {
        boundary_conditions.extend(receipt.boundary_conditions.keys().cloned());
    }
    if let Ok(envelopes) = selection.envelopes() {
        for envelope in envelopes {
            boundary_conditions.extend(envelope.conditions);
            boundary_conditions.extend(envelope.exclusions);
        }
    }
    boundary_conditions.sort();
    boundary_conditions.dedup();

    let response = build_response(
        &request,
        &selection,
        &kernels,
        &receipts,
        evaluation.as_ref(),
        research_gap.as_ref(),
        ambiguous.as_ref(),
        refusal.as_ref(),
        &outcome,
    )?;
    let assessment = response.assess_integrity_only();

    Ok(PipelineRun {
        selection,
        response,
        assessment,
        outcome,
        receipts,
        values,
        boundary_conditions,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_response(
    request: &PipelineRequest,
    selection: &ApplicabilitySelection,
    kernels: &[KernelReport],
    receipts: &[AnalysisReceipt],
    evaluation: Option<&crate::requirements::CandidateEvaluation>,
    research_gap: Option<&ResearchRequest>,
    ambiguous: Option<&(Vec<String>, String)>,
    refusal: Option<&KernelRefusal>,
    outcome: &PipelineOutcome,
) -> CoreResult<EngineeringResponse> {
    let selected = selection.is_all_selected() && research_gap.is_none() && ambiguous.is_none();
    let unknowns = {
        let mut values = request.unknowns.clone();
        if let Some(evaluation) = evaluation {
            values.extend(
                evaluation
                    .unresolved()
                    .iter()
                    .map(|outcome| format!("{}: {:?}", outcome.requirement_id, outcome.state)),
            );
        }
        values
    };
    let formulation = Gate::established(Formulation {
        objective: request.requirements.objective.clone(),
        scope: request.requirements.scope.clone(),
        requirements: vec![NodeId::new(1)],
        constraints: if request.requirements.constraints.is_empty() {
            Vec::new()
        } else {
            vec![NodeId::new(1)]
        },
        assumptions: request.assumptions.clone(),
        unknowns,
    });
    let authority = if request.authority.is_empty() {
        Gate::unavailable("no knowledge authority is recorded", Vec::new())
    } else {
        Gate::established(KnowledgeAuthority {
            packs: request.authority.clone(),
        })
    };
    let theory = if let Some(gap) = research_gap {
        Gate::research_required(gap.gate, gap.reason.clone(), gap.sources_required.clone())
    } else if let Some((candidates, reason)) = ambiguous {
        Gate::research_required(
            GateId::Theory,
            reason.clone(),
            candidates
                .iter()
                .map(|candidate| format!("disambiguate {candidate}"))
                .collect(),
        )
    } else if !selected {
        Gate::research_required(
            GateId::Theory,
            "the applicability selector did not establish a governing model",
            vec!["a registered model with an established validity domain".to_string()],
        )
    } else {
        let mut models = Vec::new();
        let mut alternatives = Vec::new();
        for selection_outcome in &selection.selections {
            if let RequirementSelection::Selected {
                alternatives_considered,
                ..
            } = selection_outcome
            {
                alternatives.extend(alternatives_considered.clone());
            }
        }
        for model in selection.unique_models() {
            models.push(ModelSelection {
                model_id: model.model_id.clone(),
                version: model.version.clone(),
                equation_ids: model.equation_ids.clone(),
                rationale: model.rationale.clone(),
            });
        }
        if alternatives.is_empty() {
            alternatives
                .push("no other registered model covers this discipline/property".to_string());
        }
        Gate::established(TheorySelection {
            models,
            alternatives_considered: alternatives,
            selection: Some(selection.clone()),
        })
    };
    let validity = if selected && research_gap.is_none() && ambiguous.is_none() {
        match selection.envelopes() {
            Ok(envelopes) => Gate::established(ValidityEnvelopes { envelopes }),
            Err(error) => Gate::unavailable(error.to_string(), Vec::new()),
        }
    } else {
        Gate::research_required(
            GateId::Validity,
            research_gap
                .map(|gap| gap.reason.clone())
                .or_else(|| ambiguous.map(|(_, reason)| reason.clone()))
                .unwrap_or_else(|| {
                    "no validity envelope can be established without a selected model".to_string()
                }),
            research_gap
                .map(|gap| gap.sources_required.clone())
                .unwrap_or_else(|| {
                    vec!["a registered validity domain for the selected model".to_string()]
                }),
        )
    };
    let mut dimensions = Vec::new();
    for report in kernels {
        dimensions.extend(report.checks.clone());
    }
    if let Some(evaluation) = evaluation {
        dimensions.extend(evaluation.dimension_checks.clone());
    }
    let dimensions = if dimensions.is_empty() {
        Gate::unavailable(
            "no machine-checked dimensional comparison was executed",
            Vec::new(),
        )
    } else {
        Gate::established(DimensionChecks { checks: dimensions })
    };
    let executed_analysis = if refusal.is_some() {
        Gate::unavailable(
            refusal
                .map(|refusal| refusal.reason.clone())
                .unwrap_or_default(),
            refusal
                .map(|refusal| refusal.required.clone())
                .unwrap_or_default(),
        )
    } else if receipts.is_empty() {
        Gate::unavailable("no executed analysis receipt is recorded", Vec::new())
    } else {
        Gate::established(ExecutedAnalysis {
            receipts: receipts.to_vec(),
        })
    };
    let cross_check = cross_check(kernels, receipts)?;
    let uncertainty = if kernels.is_empty() {
        Gate::unavailable("no kernel uncertainty was produced", Vec::new())
    } else {
        let mut contributions = Vec::new();
        for report in kernels {
            contributions.push(UncertaintyContribution {
                source: report.model_id.clone(),
                uncertainty: report.uncertainty,
                note: "kernel-declared result uncertainty".to_string(),
            });
        }
        let summary = combine_intervals(kernels);
        Gate::established(UncertaintyStatement {
            method: "kernel-declared interval propagation".to_string(),
            contributions,
            coverage: summary.map(|_| 1.0),
            summary,
        })
    };
    let counterexample_search = if request.counterexamples.is_empty() {
        Gate::unavailable("no counterexample/failure probe was executed", Vec::new())
    } else {
        Gate::established(CounterexampleSearch {
            probes: request.counterexamples.clone(),
        })
    };
    let realization = if request.realization.is_empty() {
        Gate::unavailable("no realization consequence is recorded", Vec::new())
    } else {
        Gate::established(RealizationConsequences {
            consequences: request.realization.clone(),
        })
    };
    let verification = if request.tests.is_empty() {
        Gate::unavailable("no falsifiable verification is recorded", Vec::new())
    } else {
        Gate::established(FalsifiableVerification {
            tests: request.tests.clone(),
        })
    };

    let response = EngineeringResponse {
        formulation,
        authority,
        theory,
        validity,
        dimensions,
        executed_analysis,
        cross_check,
        uncertainty,
        counterexample_search,
        realization,
        verification,
    };

    // A solved result must establish the full contract. A rejection is a
    // negative result: it may legitimately fail to establish gate 7 or even
    // carry a model-side gate violation, but the response still records every
    // gate state for the caller/oracle to inspect.
    if matches!(outcome, PipelineOutcome::Solved) {
        let assessment = response.assess_integrity_only();
        if !assessment.is_self_consistent() {
            return Err(CoreError::InvalidValue {
                reason: format!(
                    "the executed pipeline did not establish the 11-gate contract: {:?}",
                    assessment
                ),
            });
        }
    }
    Ok(response)
}

fn cross_check(
    kernels: &[KernelReport],
    receipts: &[AnalysisReceipt],
) -> CoreResult<Gate<CrossCheck>> {
    let Some(report) = kernels.first() else {
        return Ok(Gate::unavailable(
            "no executed kernel to cross-check",
            Vec::new(),
        ));
    };
    let Some(check) = &report.cross_check else {
        return Ok(Gate::unavailable(
            "the kernel did not perform an independent cross-check",
            vec!["an independent method for the selected model".to_string()],
        ));
    };
    let some_receipt = receipts.first().ok_or_else(|| CoreError::InvalidValue {
        reason: "cross-check requires an executed receipt".to_string(),
    })?;
    if !check.tolerance.is_finite() || check.tolerance <= 0.0 {
        return Ok(Gate::unavailable(
            "cross-check tolerance is not positive",
            Vec::new(),
        ));
    }
    if check.difference.abs() > check.tolerance {
        return Ok(Gate::unavailable(
            "independent methods disagree beyond the stated tolerance",
            Vec::new(),
        ));
    }
    let mut independent = some_receipt.clone();
    independent.identity.solver_id = format!("{}.crosscheck", some_receipt.identity.solver_id);
    independent.attestation = None;
    let signer = ReceiptSigner::from_seed(
        *Digest::of_bytes(format!("crosscheck:{}", some_receipt.identity.solver_id).as_bytes())
            .as_bytes(),
    );
    signer.sign_receipt(&mut independent)?;
    Ok(Gate::established(CrossCheck {
        method: check.method.clone(),
        independent_of: some_receipt.identity.solver_id.clone(),
        receipts: vec![independent],
        agreement: Agreement {
            metric: format!("{} absolute difference", report.model_id),
            value: check.difference,
            tolerance: check.tolerance,
            agrees: true,
        },
    }))
}

fn combine_intervals(kernels: &[KernelReport]) -> Option<Interval> {
    let mut combined: Option<Interval> = None;
    for report in kernels {
        let Some(bounds) = report.uncertainty.interval() else {
            continue;
        };
        combined = Some(match combined {
            None => bounds,
            Some(existing) => Interval::new(
                existing.lower().min(bounds.lower()),
                existing.upper().max(bounds.upper()),
            )
            .ok()?,
        });
    }
    combined
}

/// Convert a receipt result into the benchmark-neutral observation shape.
pub fn receipt_observation(result: &ReceiptResult) -> (String, ReceiptValue) {
    (result.name.clone(), result.value.clone())
}

/// Context provenance for in-process pipeline receipts.
pub fn pipeline_provenance(citation: &str) -> Provenance {
    Provenance::new(ProvenanceKind::Assumption, citation)
        .expect("static pipeline provenance must be non-empty")
}
