//! Kernel adapters from the real `engineering-models` crates into the neutral
//! [`KernelReport`] seam. They translate typed inputs and outputs only; they
//! never decide an outcome, gate or benchmark verdict.

use std::collections::BTreeMap;

use engineering_core::{
    AnalysisReceipt, AssumptionRecord, DimensionCheck, EngineeringValue, KernelCrossCheck,
    KernelRefusal, KernelRejection, KernelReport, KernelStatus, Provenance, ProvenanceKind,
    RevisionId,
};
use engineering_models::{
    check_moldability, emc_pcb_check, mosfet_conduction_thermal, torque_preload, CheckStatus,
    EmcPcbInput, EmcRules, Estimate, ModelContext, ModelError, ModelReport, ModelStatus,
    MoldabilityInput, MoldabilityResult, MoldingRules, MosfetThermalInput, NutFactorScatter,
    PcbStackup, RefusalCode, TorquePreloadInput,
};
use engineering_units::{Interval, Quantity, Uncertainty, UnitRegistry};

use crate::BenchError;

/// Fixture provenance: synthetic benchmark inputs, never external evidence.
pub(crate) fn fixture_provenance(citation: &str) -> Provenance {
    Provenance::new(ProvenanceKind::Assumption, citation).expect("static fixture citation")
}

pub(crate) fn model_context() -> ModelContext {
    ModelContext::new(
        RevisionId::new(1),
        fixture_provenance(
            "benchmark fixture: hypothetical design inputs, not a datasheet or test report",
        ),
    )
}

fn quantity(probe: &engineering_core::KernelProbe, name: &str) -> Result<Quantity, BenchError> {
    probe
        .inputs
        .get(name)
        .cloned()
        .ok_or_else(|| BenchError::Case {
            id: probe.id.clone(),
            detail: format!("missing kernel input {name}"),
        })
}

fn optional_quantity(probe: &engineering_core::KernelProbe, name: &str) -> Option<Quantity> {
    probe.inputs.get(name).cloned()
}

fn estimate(
    probe: &engineering_core::KernelProbe,
    name: &str,
    lower: f64,
    upper: f64,
) -> Result<Estimate, BenchError> {
    let nominal = quantity(probe, name)?;
    let interval = Interval::new(lower, upper).map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    Estimate::new(nominal, Uncertainty::Interval(interval)).map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })
}

fn known(
    probe: &engineering_core::KernelProbe,
    _name: &str,
    value: Quantity,
) -> Result<EngineeringValue, BenchError> {
    EngineeringValue::known(value, fixture_provenance(&probe.id), RevisionId::new(1)).map_err(
        |error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        },
    )
}

fn assumptions(values: Vec<engineering_models::AssumptionRecord>) -> Vec<AssumptionRecord> {
    values
        .into_iter()
        .map(|assumption| AssumptionRecord {
            id: assumption.id,
            statement: assumption.statement,
            provenance: assumption.provenance,
        })
        .collect()
}

fn checks(values: Vec<DimensionCheck>) -> Vec<DimensionCheck> {
    values
}

fn cross_check(
    evidence: Option<engineering_models::CrossCheckEvidence>,
) -> Option<KernelCrossCheck> {
    evidence.map(|evidence| KernelCrossCheck {
        method: evidence.method,
        primary_value: evidence.primary_value,
        independent_value: evidence.independent_value,
        difference: evidence.difference,
        tolerance: evidence.tolerance,
    })
}

fn refusal_code(code: RefusalCode) -> String {
    format!("{code:?}")
}

fn base_report(
    probe: &engineering_core::KernelProbe,
    model_id: &str,
    report: &ModelReport<impl Sized>,
    values: BTreeMap<String, EngineeringValue>,
) -> Result<KernelReport, BenchError> {
    let status = match report.status {
        ModelStatus::Solved => KernelStatus::Solved,
        ModelStatus::Refused | ModelStatus::Unavailable => KernelStatus::Refused,
    };
    let refusal = report
        .refusal
        .as_ref()
        .map(|refusal| KernelRefusal {
            code: refusal_code(refusal.code),
            reason: refusal.reason.clone(),
            required: refusal.evidence.clone(),
        })
        .or_else(|| {
            (status == KernelStatus::Refused).then(|| {
                let unavailable = report.value.unavailable_reason();
                KernelRefusal {
                    code: "Unavailable".to_string(),
                    reason: unavailable
                        .map(|value| value.reason.clone())
                        .unwrap_or_else(|| "the model returned no value".to_string()),
                    required: unavailable
                        .map(|value| value.required.clone())
                        .unwrap_or_default(),
                }
            })
        });
    Ok(KernelReport {
        model_id: probe
            .model_id
            .clone()
            .unwrap_or_else(|| model_id.to_string()),
        status,
        values,
        checks: checks(report.checks.clone()),
        assumptions: assumptions(report.assumptions.clone()),
        uncertainty: report.uncertainty,
        cross_check: cross_check(report.cross_check.clone()),
        receipt: report.receipt.clone(),
        refusal,
        rejection: None,
    })
}

/// Execute the MOSFET conduction/thermal kernel.
pub(crate) fn thermal(probe: &engineering_core::KernelProbe) -> Result<KernelReport, BenchError> {
    let input = MosfetThermalInput {
        drain_current_rms: estimate(probe, "drain_current_rms", 19.6, 20.4)?,
        rds_on_reference: estimate(probe, "rds_on_reference", 9.5, 10.5)?,
        temp_coefficient: estimate(probe, "temp_coefficient", 0.0038, 0.0042)?,
        reference_temperature: quantity(probe, "reference_temperature")?,
        ambient_temperature: estimate(probe, "ambient_temperature", 35.0, 45.0)?,
        r_theta_jc: estimate(probe, "r_theta_jc", 1.4, 1.6)?,
        r_theta_cs: optional_quantity(probe, "r_theta_cs")
            .map(|_| estimate(probe, "r_theta_cs", 0.35, 0.45))
            .transpose()?,
        r_theta_sa: estimate(probe, "r_theta_sa", 1.9, 2.1)?,
        max_junction_temperature: engineering_core::ValueState::known(quantity(
            probe,
            "max_junction_temperature",
        )?),
    };
    let report = match mosfet_conduction_thermal(&input, &model_context()) {
        Ok(report) => report,
        Err(ModelError::InvalidInput { field, reason }) if field == "r_theta_cs" => {
            return Ok(KernelReport {
                model_id: probe
                    .model_id
                    .clone()
                    .unwrap_or_else(|| engineering_models::MOSFET_THERMAL_MODEL.to_string()),
                status: KernelStatus::Refused,
                values: BTreeMap::new(),
                checks: Vec::new(),
                assumptions: Vec::new(),
                uncertainty: Uncertainty::Exact,
                cross_check: None,
                receipt: empty_receipt(probe, engineering_models::MOSFET_THERMAL_MODEL, &reason)?,
                refusal: Some(KernelRefusal {
                    code: "NonPositiveGeometry".to_string(),
                    reason: format!("{field}: {reason}"),
                    required: vec!["a strictly positive case-to-sink resistance".to_string()],
                }),
                rejection: None,
            });
        }
        Err(error) => {
            return Err(BenchError::Case {
                id: probe.id.clone(),
                detail: error.to_string(),
            })
        }
    };
    let mut values = BTreeMap::new();
    if let Some(value) = report.value.as_known() {
        values.insert(
            "junction_temperature".to_string(),
            known(
                probe,
                "junction_temperature",
                value.junction_temperature.clone(),
            )?,
        );
        values.insert(
            "conduction_loss".to_string(),
            known(probe, "conduction_loss", value.conduction_loss.clone())?,
        );
    }
    base_report(
        probe,
        engineering_models::MOSFET_THERMAL_MODEL,
        &report,
        values,
    )
}

/// Execute the bolted-joint torque/preload kernel.
pub(crate) fn fastener(probe: &engineering_core::KernelProbe) -> Result<KernelReport, BenchError> {
    let validity = Interval::new(
        quantity(probe, "validity_lower")?.value(),
        quantity(probe, "validity_upper")?.value(),
    )
    .map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let scatter = Interval::new(
        quantity(probe, "nut_factor_lower")?.value(),
        quantity(probe, "nut_factor_upper")?.value(),
    )
    .map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let nut_factor = NutFactorScatter::new(
        quantity(probe, "nut_factor")?.value(),
        Uncertainty::Interval(scatter),
        validity,
    )
    .map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let input = TorquePreloadInput {
        torque: quantity(probe, "torque")?,
        nominal_diameter: quantity(probe, "nominal_diameter")?,
        thread_pitch: optional_quantity(probe, "thread_pitch"),
        tensile_area: None,
        nut_factor,
        proof_strength: engineering_core::ValueState::known(quantity(probe, "proof_strength")?),
    };
    let report = torque_preload(&input, &model_context()).map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let mut values = BTreeMap::new();
    if let Some(value) = report.value.as_known() {
        values.insert(
            "preload".to_string(),
            known(probe, "preload", value.preload_nominal.clone())?,
        );
        if let engineering_core::ValueState::Known(utilization) = &value.utilization {
            let quantity =
                Quantity::dimensionless(*utilization).map_err(|error| BenchError::Case {
                    id: probe.id.clone(),
                    detail: error.to_string(),
                })?;
            values.insert(
                "utilization".to_string(),
                known(probe, "utilization", quantity)?,
            );
        }
    }
    base_report(
        probe,
        engineering_models::BOLTED_JOINT_MODEL,
        &report,
        values,
    )
}

/// Execute the injection-molding moldability kernel.
pub(crate) fn molding(probe: &engineering_core::KernelProbe) -> Result<KernelReport, BenchError> {
    let units = UnitRegistry::si();
    let rules = MoldingRules::new(
        Quantity::new(
            1.0,
            units.require("deg").map_err(|error| BenchError::Case {
                id: probe.id.clone(),
                detail: error.to_string(),
            })?,
        )
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?,
        0.12,
        Some(150.0),
        fixture_provenance(
            "benchmark fixture: explicit DFM rule inputs, not a universal draft/wall standard",
        ),
    )
    .map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let input = MoldabilityInput {
        nominal_wall: quantity(probe, "nominal_wall")?,
        min_wall: quantity(probe, "min_wall")?,
        max_wall: quantity(probe, "max_wall")?,
        draft_actual: quantity(probe, "draft_actual")?,
        flow_length: optional_quantity(probe, "flow_length"),
        rules,
    };
    let report = check_moldability(&input, &model_context()).map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let Some(value) = report.value.as_known() else {
        return base_report(
            probe,
            engineering_models::MOLDING_MODEL,
            &report,
            BTreeMap::new(),
        );
    };
    let dimensionless = |value: f64| -> Result<Quantity, BenchError> {
        Quantity::dimensionless(value).map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })
    };
    let mut values = BTreeMap::new();
    values.insert(
        "draft_margin".to_string(),
        known(probe, "draft_margin", value.draft_margin.clone())?,
    );
    values.insert(
        "wall_variation".to_string(),
        known(
            probe,
            "wall_variation",
            dimensionless(value.wall_variation)?,
        )?,
    );
    if let Some(flow_ratio) = value.flow_ratio {
        values.insert(
            "flow_ratio".to_string(),
            known(probe, "flow_ratio", dimensionless(flow_ratio)?)?,
        );
    }
    let rejection = if value.passes_all_required {
        None
    } else {
        Some(KernelRejection {
            code: "MoldabilityViolation".to_string(),
            reasons: value
                .checks
                .iter()
                .filter(|check| check.status == CheckStatus::Fail)
                .map(|check| format!("{}: {}", check.id, check.message))
                .collect(),
        })
    };
    let mut report_out = base_report(probe, engineering_models::MOLDING_MODEL, &report, values)?;
    report_out.rejection = rejection;
    report_out.cross_check = Some(molding_cross_check(probe, value)?);
    Ok(report_out)
}

fn molding_cross_check(
    probe: &engineering_core::KernelProbe,
    value: &MoldabilityResult,
) -> Result<KernelCrossCheck, BenchError> {
    let deg = UnitRegistry::si()
        .require("deg")
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?;
    let metre = UnitRegistry::si()
        .require("m")
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?;
    let nominal = quantity(probe, "nominal_wall")?
        .to_unit(metre.clone())
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?
        .value();
    let min = quantity(probe, "min_wall")?
        .to_unit(metre.clone())
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?
        .value();
    let max = quantity(probe, "max_wall")?
        .to_unit(metre)
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?
        .value();
    let recomputed_wall = (max - min) / nominal;
    let recomputed_draft = quantity(probe, "draft_actual")?
        .to_unit(deg.clone())
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?
        .value()
        - 1.0;
    let observed_draft = value
        .draft_margin
        .to_unit(deg)
        .map_err(|error| BenchError::Case {
            id: probe.id.clone(),
            detail: error.to_string(),
        })?
        .value();
    let primary = value.wall_variation.max(observed_draft);
    let independent = recomputed_wall.max(recomputed_draft);
    Ok(KernelCrossCheck {
        method: "independent arithmetic recomputation of wall variation and draft margin"
            .to_string(),
        primary_value: primary,
        independent_value: independent,
        difference: (primary - independent).abs(),
        tolerance: 1e-9,
    })
}

/// Receipt identity helper for a synthetic refusal control.
pub(crate) fn empty_receipt(
    probe: &engineering_core::KernelProbe,
    model_id: &str,
    reason: &str,
) -> Result<AnalysisReceipt, BenchError> {
    let content = engineering_core::ReceiptContent::new(
        engineering_core::Digest::of_bytes(probe.id.as_bytes()),
        engineering_core::Digest::of_bytes(model_id.as_bytes()),
        engineering_core::Digest::of_bytes(b"fixture-settings"),
    )
    .with_result(engineering_core::ReceiptResult {
        name: "refusal".to_string(),
        value: engineering_core::ReceiptValue::Text(reason.to_string()),
        uncertainty: Uncertainty::Exact,
    })
    .with_convergence(engineering_core::Convergence::iterative(1, 0.0, 1.0, false));
    AnalysisReceipt::kernel(
        engineering_core::AnalysisIdentity {
            solver_id: format!("engineering-models/{model_id}"),
            solver_version: engineering_models::MODEL_VERSION.to_string(),
            kind: engineering_core::AnalysisKind::InProcessKernel,
        },
        content,
        0,
    )
    .map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })
}

/// Execute the op-amp loop-stability kernel.
pub(crate) fn analog(probe: &engineering_core::KernelProbe) -> Result<KernelReport, BenchError> {
    let input = engineering_models::OpAmpStabilityInput {
        open_loop_gain: quantity(probe, "open_loop_gain")?.value(),
        first_pole: quantity(probe, "first_pole")?,
        second_pole: quantity(probe, "second_pole")?,
        output_resistance: Some(quantity(probe, "output_resistance")?),
        load_capacitance: Some(quantity(probe, "load_capacitance")?),
        opamp_output_capacitance: Some(quantity(probe, "opamp_output_capacitance")?),
        input_capacitance: Some(quantity(probe, "input_capacitance")?),
        feedback_resistance: Some(quantity(probe, "feedback_resistance")?),
        feedback_capacitor: optional_quantity(probe, "feedback_capacitor"),
        noise_gain: quantity(probe, "noise_gain")?.value(),
        required_phase_margin_deg: quantity(probe, "required_phase_margin_deg")?.value(),
        required_gain_margin_db: quantity(probe, "required_gain_margin_db")?.value(),
    };
    let report =
        engineering_models::opamp_loop_stability(&input, &model_context()).map_err(|error| {
            BenchError::Case {
                id: probe.id.clone(),
                detail: error.to_string(),
            }
        })?;
    let mut values = BTreeMap::new();
    let mut rejection = None;
    if let Some(value) = report.value.as_known() {
        if let engineering_core::ValueState::Known(phase_margin) = &value.phase_margin {
            values.insert(
                "phase_margin".to_string(),
                known(probe, "phase_margin", phase_margin.clone())?,
            );
        }
        if let engineering_core::ValueState::Known(gain_margin) = &value.gain_margin_db {
            values.insert(
                "gain_margin".to_string(),
                known(probe, "gain_margin", gain_margin.clone())?,
            );
        }
        if let Some(gain) = value.required_noise_gain {
            values.insert(
                "required_noise_gain".to_string(),
                known(
                    probe,
                    "required_noise_gain",
                    Quantity::dimensionless(gain).map_err(|error| BenchError::Case {
                        id: probe.id.clone(),
                        detail: error.to_string(),
                    })?,
                )?,
            );
        }
        if !value.stable || value.compensation_required {
            rejection = Some(KernelRejection {
                code: "PhaseMarginViolation".to_string(),
                reasons: vec![value
                    .phase_margin
                    .as_known()
                    .map(|margin| {
                        format!(
                            "phase margin {:.2} deg against required {:.2} deg; gain margin {:?}",
                            margin.value(),
                            input.required_phase_margin_deg,
                            value.gain_margin_db
                        )
                    })
                    .unwrap_or_else(|| "the modeled loop is unstable".to_string())],
            });
        }
    }
    let mut out = base_report(
        probe,
        engineering_models::OPAMP_STABILITY_MODEL,
        &report,
        values,
    )?;
    out.rejection = rejection;
    Ok(out)
}

/// Execute the PCB return-path and spacing kernel.
pub(crate) fn emc(probe: &engineering_core::KernelProbe) -> Result<KernelReport, BenchError> {
    let stackup = optional_quantity(probe, "stackup_height")
        .map(|height| {
            Ok::<_, BenchError>(PcbStackup {
                dielectric_height: height,
                copper_thickness: optional_quantity(probe, "copper_thickness"),
            })
        })
        .transpose()?;
    let input = EmcPcbInput {
        stackup,
        trace_width: optional_quantity(probe, "trace_width"),
        loop_length: optional_quantity(probe, "loop_length"),
        capacitor_distance: optional_quantity(probe, "capacitor_distance"),
        via_length: optional_quantity(probe, "via_length"),
        frequency: optional_quantity(probe, "frequency"),
        max_loop_inductance: optional_quantity(probe, "max_loop_inductance"),
        actual_clearance: optional_quantity(probe, "actual_clearance"),
        actual_creepage: optional_quantity(probe, "actual_creepage"),
        rules: match (
            optional_quantity(probe, "min_clearance"),
            optional_quantity(probe, "min_creepage"),
        ) {
            (Some(min_clearance), Some(min_creepage)) => Some(EmcRules {
                min_clearance,
                min_creepage,
                provenance: fixture_provenance("benchmark fixture: project spacing rule"),
            }),
            _ => None,
        },
    };
    let report = emc_pcb_check(&input, &model_context()).map_err(|error| BenchError::Case {
        id: probe.id.clone(),
        detail: error.to_string(),
    })?;
    let mut values = BTreeMap::new();
    let mut rejection = None;
    if let Some(value) = report.value.as_known() {
        if let engineering_core::ValueState::Known(inductance) = &value.loop_inductance {
            values.insert(
                "loop_inductance".to_string(),
                known(probe, "loop_inductance", inductance.clone())?,
            );
            if let Some(target) = &input.max_loop_inductance {
                let target = target.to_unit(inductance.unit().clone()).map_err(|error| {
                    BenchError::Case {
                        id: probe.id.clone(),
                        detail: error.to_string(),
                    }
                })?;
                if inductance.value() > target.value() {
                    rejection = Some(KernelRejection {
                        code: "ReturnPathViolation".to_string(),
                        reasons: vec![format!(
                            "loop inductance {:.6e} H exceeds the target {:.6e} H",
                            inductance.value(),
                            target.value()
                        )],
                    });
                }
            }
        }
        if let engineering_core::ValueState::Known(factor) = &value.placement_inductance_factor {
            values.insert(
                "placement_factor".to_string(),
                known(
                    probe,
                    "placement_factor",
                    Quantity::dimensionless(*factor).map_err(|error| BenchError::Case {
                        id: probe.id.clone(),
                        detail: error.to_string(),
                    })?,
                )?,
            );
        }
        for (name, check) in [
            ("clearance_margin", &value.clearance_check),
            ("creepage_margin", &value.creepage_check),
        ] {
            if let engineering_core::ValueState::Known(check) = check {
                let margin =
                    check
                        .actual
                        .sub(&check.required)
                        .map_err(|error| BenchError::Case {
                            id: probe.id.clone(),
                            detail: error.to_string(),
                        })?;
                values.insert(name.to_string(), known(probe, name, margin)?);
                if !check.pass {
                    let rejection = rejection.get_or_insert_with(|| KernelRejection {
                        code: "ReturnPathViolation".to_string(),
                        reasons: Vec::new(),
                    });
                    rejection.reasons.push(check.message.clone());
                }
            }
        }
    }
    let mut out = base_report(probe, engineering_models::EMC_PCB_MODEL, &report, values)?;
    out.rejection = rejection;
    Ok(out)
}
