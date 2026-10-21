use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ReceiptResult, ReceiptValue,
    ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_positive, value_in, warning, AssumptionRecord, ModelContext, ModelError, ModelReport,
    ModelResult, ModelStatus, MODEL_VERSION,
};

pub const MOLDING_MODEL: &str = "manufacturing/injection-moldability";
const MODEL_EXPRESSION: &str =
    "wall_variation = (t_max - t_min)/t_nominal; draft_margin = draft_actual - draft_min; flow_ratio = L/t";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoldingRules {
    pub min_draft: Quantity,
    pub max_wall_variation: f64,
    pub max_flow_ratio: Option<f64>,
    pub provenance: Provenance,
}

impl MoldingRules {
    pub fn new(
        min_draft: Quantity,
        max_wall_variation: f64,
        max_flow_ratio: Option<f64>,
        provenance: Provenance,
    ) -> ModelResult<Self> {
        require_positive("max_wall_variation", max_wall_variation)?;
        if let Some(limit) = max_flow_ratio {
            require_positive("max_flow_ratio", limit)?;
        }
        let rules = Self {
            min_draft,
            max_wall_variation,
            max_flow_ratio,
            provenance,
        };
        rules.validate()?;
        Ok(rules)
    }

    fn validate(&self) -> ModelResult<()> {
        let _ = angle_degrees(&self.min_draft)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoldabilityInput {
    pub nominal_wall: Quantity,
    pub min_wall: Quantity,
    pub max_wall: Quantity,
    pub draft_actual: Quantity,
    pub flow_length: Option<Quantity>,
    pub rules: MoldingRules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManufacturabilityCheck {
    pub id: String,
    pub status: CheckStatus,
    pub observed: Option<f64>,
    pub limit: Option<f64>,
    pub unit: String,
    pub required: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoldabilityResult {
    pub wall_variation: f64,
    pub draft_margin: Quantity,
    pub flow_ratio: Option<f64>,
    pub checks: Vec<ManufacturabilityCheck>,
    pub passes_all_required: bool,
}

fn angle_degrees(quantity: &Quantity) -> ModelResult<f64> {
    require_dimension("draft_angle", quantity, Dimension::SCALAR)?;
    let symbol = quantity.unit().symbol();
    if symbol != "deg" && symbol != "rad" {
        return Err(ModelError::InvalidInput {
            field: "draft_angle",
            reason: format!("draft must carry an angle unit (deg or rad), got {symbol}"),
        });
    }
    value_in(quantity, "deg")
}

fn assumption(id: &str, statement: &str, citation: &str) -> AssumptionRecord {
    AssumptionRecord::new(
        id,
        statement,
        Provenance::new(ProvenanceKind::EngineeringPractice, citation)
            .expect("static non-empty provenance"),
    )
}

fn assumptions() -> Vec<AssumptionRecord> {
    vec![
        assumption(
            "asm-rule-inputs",
            "Minimum draft, maximum wall variation and optional flow ratio are explicit DFM review inputs, not universal defaults.",
            "Injection-molding knowledge pack: DFM review inputs.",
        ),
        assumption(
            "asm-thermoplastic",
            "The part is a thermoplastic injection molding; other processes have different rules.",
            "Injection-molding knowledge pack: process scope.",
        ),
        assumption(
            "asm-no-tool-deflection",
            "These geometric checks do not model tool deflection, core shift or packing.",
            "Injection-molding knowledge pack: tooling assumptions.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "wall_variation = (t_max - t_min) / t_nominal",
            Dimension::SCALAR,
            Dimension::LENGTH.checked_div(Dimension::LENGTH)?,
        )?,
        DimensionCheck::new(
            "draft_margin = draft_actual - draft_min",
            Dimension::SCALAR,
            Dimension::SCALAR.checked_sub(Dimension::SCALAR)?,
        )?,
        DimensionCheck::new(
            "flow_ratio = flow_length / t_nominal",
            Dimension::SCALAR,
            Dimension::LENGTH.checked_div(Dimension::LENGTH)?,
        )?,
    ])
}

pub fn check_moldability(
    input: &MoldabilityInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<MoldabilityResult>> {
    for (field, value) in [
        ("nominal_wall", &input.nominal_wall),
        ("min_wall", &input.min_wall),
        ("max_wall", &input.max_wall),
    ] {
        require_dimension(field, value, Dimension::LENGTH)?;
        require_positive(field, value_in(value, "m")?)?;
    }
    let nominal_m = value_in(&input.nominal_wall, "m")?;
    let min_m = value_in(&input.min_wall, "m")?;
    let max_m = value_in(&input.max_wall, "m")?;
    if min_m > nominal_m || nominal_m > max_m {
        return Err(ModelError::InvalidInput {
            field: "wall_thickness",
            reason: format!(
                "expected min <= nominal <= max, got {min_m} <= {nominal_m} <= {max_m}"
            ),
        });
    }
    let draft_actual_deg = angle_degrees(&input.draft_actual)?;
    let draft_min_deg = angle_degrees(&input.rules.min_draft)?;
    let draft_margin_deg = draft_actual_deg - draft_min_deg;
    let wall_variation = (max_m - min_m) / nominal_m;
    let flow_ratio = input
        .flow_length
        .as_ref()
        .map(|length| {
            require_dimension("flow_length", length, Dimension::LENGTH)?;
            let length_m = value_in(length, "m")?;
            require_positive("flow_length", length_m)?;
            Ok::<f64, ModelError>(length_m / nominal_m)
        })
        .transpose()?;

    let draft_pass = draft_margin_deg >= 0.0;
    let wall_pass = wall_variation <= input.rules.max_wall_variation;
    let flow_status = match (flow_ratio, input.rules.max_flow_ratio) {
        (Some(ratio), Some(limit)) => {
            if ratio <= limit {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            }
        }
        _ => CheckStatus::NotApplicable,
    };
    let flow_pass = flow_status != CheckStatus::Fail;
    let passes_all_required = draft_pass && wall_pass && flow_pass;

    let checks_out = vec![
        ManufacturabilityCheck {
            id: "draft".to_string(),
            status: if draft_pass {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            observed: Some(draft_actual_deg),
            limit: Some(draft_min_deg),
            unit: "deg".to_string(),
            required: true,
            message: format!(
                "draft {draft_actual_deg:.4} deg against required {draft_min_deg:.4} deg ({})",
                input.rules.provenance.citation
            ),
        },
        ManufacturabilityCheck {
            id: "wall_uniformity".to_string(),
            status: if wall_pass {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            observed: Some(wall_variation),
            limit: Some(input.rules.max_wall_variation),
            unit: "1".to_string(),
            required: true,
            message: format!(
                "wall variation {:.4} against limit {:.4} ({})",
                wall_variation, input.rules.max_wall_variation, input.rules.provenance.citation
            ),
        },
        ManufacturabilityCheck {
            id: "flow_length_ratio".to_string(),
            status: flow_status,
            observed: flow_ratio,
            limit: input.rules.max_flow_ratio,
            unit: "1".to_string(),
            required: false,
            message: match flow_ratio {
                Some(ratio) => format!(
                    "flow ratio {ratio:.4} against optional limit {:?}",
                    input.rules.max_flow_ratio
                ),
                None => "flow length was not supplied; flow check not applicable".to_string(),
            },
        },
    ];

    let mut warnings = Vec::new();
    if !draft_pass {
        warnings.push(warning(
            "draft.violation",
            format!("draft margin is {draft_margin_deg:.4} deg; ejection damage risk"),
            WarningSeverity::Critical,
        ));
    }
    if !wall_pass {
        warnings.push(warning(
            "wall.violation",
            format!(
                "wall variation {:.4} exceeds {:.4}; sink/warp risk",
                wall_variation, input.rules.max_wall_variation
            ),
            WarningSeverity::Critical,
        ));
    }
    if flow_status == CheckStatus::Fail {
        warnings.push(warning(
            "flow.violation",
            "flow length ratio exceeds the supplied material/tool limit",
            WarningSeverity::Warning,
        ));
    }

    let mut bcs = BTreeMap::new();
    bcs.insert(
        "nominal_wall".to_string(),
        boundary_value(&input.nominal_wall, context)?,
    );
    bcs.insert(
        "min_wall".to_string(),
        boundary_value(&input.min_wall, context)?,
    );
    bcs.insert(
        "max_wall".to_string(),
        boundary_value(&input.max_wall, context)?,
    );
    bcs.insert(
        "draft_actual".to_string(),
        boundary_value(&input.draft_actual, context)?,
    );
    bcs.insert(
        "min_draft_required".to_string(),
        boundary_value(&input.rules.min_draft, context)?,
    );
    if let Some(flow) = &input.flow_length {
        bcs.insert("flow_length".to_string(), boundary_value(flow, context)?);
    }

    let mut results = vec![
        quantity_result(
            "wall_variation",
            Quantity::dimensionless(wall_variation)?,
            Uncertainty::Exact,
        ),
        quantity_result(
            "draft_margin",
            Quantity::new(draft_margin_deg, UnitRegistry::si().require("deg")?)?,
            Uncertainty::Exact,
        ),
    ];
    if let Some(ratio) = flow_ratio {
        results.push(quantity_result(
            "flow_ratio",
            Quantity::dimensionless(ratio)?,
            Uncertainty::Exact,
        ));
    }
    results.push(ReceiptResult {
        name: "passes_all_required".to_string(),
        value: ReceiptValue::Text(passes_all_required.to_string()),
        uncertainty: Uncertainty::Exact,
    });

    let receipt = build_receipt(
        MOLDING_MODEL,
        input,
        MODEL_EXPRESSION,
        &input.rules,
        context,
        bcs,
        Convergence::exact(),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;

    Ok(ModelReport {
        status: ModelStatus::Solved,
        value: ValueState::known(MoldabilityResult {
            wall_variation,
            draft_margin: Quantity::new(draft_margin_deg, UnitRegistry::si().require("deg")?)?,
            flow_ratio,
            checks: checks_out,
            passes_all_required,
        }),
        refusal: None,
        receipt,
        checks: checks()?,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn identity() -> engineering_core::AnalysisIdentity {
    model_identity(MOLDING_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;

#[cfg(test)]
mod tests {
    use engineering_core::{Provenance, ProvenanceKind, RevisionId};
    use engineering_units::UnitRegistry;

    use super::*;

    fn context() -> ModelContext {
        ModelContext::new(
            RevisionId::new(1),
            Provenance::new(ProvenanceKind::Specification, "test").unwrap(),
        )
    }

    fn rules() -> MoldingRules {
        let units = UnitRegistry::si();
        MoldingRules::new(
            Quantity::new(1.0, units.require("deg").unwrap()).unwrap(),
            0.25,
            Some(100.0),
            Provenance::new(ProvenanceKind::EngineeringPractice, "DFM review").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn bad_geometry_is_a_failed_check_not_a_pass() {
        let units = UnitRegistry::si();
        let report = check_moldability(
            &MoldabilityInput {
                nominal_wall: Quantity::new(2.5, units.require("mm").unwrap()).unwrap(),
                min_wall: Quantity::new(2.2, units.require("mm").unwrap()).unwrap(),
                max_wall: Quantity::new(2.9, units.require("mm").unwrap()).unwrap(),
                draft_actual: Quantity::new(0.5, units.require("deg").unwrap()).unwrap(),
                flow_length: Some(Quantity::new(120.0, units.require("mm").unwrap()).unwrap()),
                rules: rules(),
            },
            &context(),
        )
        .unwrap();
        let result = report.value.as_known().unwrap();
        assert!(!result.passes_all_required);
        assert_eq!(result.checks[0].status, CheckStatus::Fail);
        assert_eq!(result.checks[1].status, CheckStatus::Fail);
        assert!((result.wall_variation - 0.28).abs() < 1e-12);
    }

    #[test]
    fn radius_versus_degree_is_not_silently_mixed() {
        let units = UnitRegistry::si();
        let input = MoldabilityInput {
            nominal_wall: Quantity::new(2.5, units.require("mm").unwrap()).unwrap(),
            min_wall: Quantity::new(2.5, units.require("mm").unwrap()).unwrap(),
            max_wall: Quantity::new(2.5, units.require("mm").unwrap()).unwrap(),
            draft_actual: Quantity::new(0.5, units.require("rad").unwrap()).unwrap(),
            flow_length: None,
            rules: rules(),
        };
        let report = check_moldability(&input, &context()).unwrap();
        let result = report.value.as_known().unwrap();
        assert!(result.draft_margin.value() > 27.0);
        assert!(result.passes_all_required);
    }
}
