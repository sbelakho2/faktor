use std::collections::BTreeMap;

use engineering_core::{
    AnalysisIdentity, AnalysisKind, AnalysisReceipt, AnalysisWarning, Convergence, Digest,
    DimensionCheck, EngineeringValue, PropertyRevision, Provenance, ReceiptContent, ReceiptResult,
    ReceiptValue, Repeatability, RevisionId, UnavailableValue, ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitError, UnitRegistry};
use serde::Serialize;
use thiserror::Error;

pub const MODEL_VERSION: &str = "1.0.0";

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("units error: {0}")]
    Units(#[from] UnitError),
    #[error("core error: {0}")]
    Core(#[from] engineering_core::CoreError),
    #[error("field {field} has dimension {found}, expected {expected}")]
    DimensionMismatch {
        field: &'static str,
        expected: Dimension,
        found: Dimension,
    },
    #[error("invalid input {field}: {reason}")]
    InvalidInput { field: &'static str, reason: String },
    #[error("serialization failed: {reason}")]
    Serialization { reason: String },
}

pub type ModelResult<T> = Result<T, ModelError>;

#[derive(Debug, Clone)]
pub struct ModelContext {
    pub revision: RevisionId,
    pub source: Provenance,
    pub property_revisions: Vec<PropertyRevision>,
}

impl ModelContext {
    pub fn new(revision: RevisionId, source: Provenance) -> Self {
        Self {
            revision,
            source,
            property_revisions: Vec::new(),
        }
    }

    pub fn with_property_revision(mut self, revision: PropertyRevision) -> Self {
        self.property_revisions.push(revision);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    OutsideValidity,
    MissingInterfaceResistance,
    MissingStrength,
    ThermalRunaway,
    NonPositiveGeometry,
    UnsupportedCondition,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Refusal {
    pub code: RefusalCode,
    pub reason: String,
    pub evidence: Vec<String>,
}

impl Refusal {
    pub fn new(code: RefusalCode, reason: impl Into<String>, evidence: Vec<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            evidence,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    Solved,
    Refused,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuantityRange {
    pub min: Quantity,
    pub max: Quantity,
}

impl QuantityRange {
    pub fn new(min: Quantity, max: Quantity) -> ModelResult<Self> {
        if min.dimension() != max.dimension() {
            return Err(ModelError::DimensionMismatch {
                field: "range",
                expected: min.dimension(),
                found: max.dimension(),
            });
        }
        let max_in_min = max.to_unit(min.unit().clone())?;
        if min.compare(&max_in_min)? == std::cmp::Ordering::Greater {
            return Err(ModelError::InvalidInput {
                field: "range",
                reason: "minimum exceeds maximum".to_string(),
            });
        }
        Ok(Self { min, max })
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CrossCheckEvidence {
    pub method: String,
    pub primary_value: f64,
    pub independent_value: f64,
    pub difference: f64,
    pub tolerance: f64,
    pub passed: bool,
}

impl CrossCheckEvidence {
    pub fn new(
        method: impl Into<String>,
        primary_value: f64,
        independent_value: f64,
        tolerance: f64,
    ) -> Self {
        let difference = (primary_value - independent_value).abs();
        Self {
            method: method.into(),
            primary_value,
            independent_value,
            difference,
            tolerance,
            passed: difference <= tolerance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelReport<T> {
    pub status: ModelStatus,
    pub value: ValueState<T>,
    pub refusal: Option<Refusal>,
    pub receipt: AnalysisReceipt,
    pub checks: Vec<DimensionCheck>,
    pub assumptions: Vec<AssumptionRecord>,
    pub warnings: Vec<AnalysisWarning>,
    pub uncertainty: Uncertainty,
    pub cross_check: Option<CrossCheckEvidence>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssumptionRecord {
    pub id: String,
    pub statement: String,
    pub provenance: Provenance,
}

impl AssumptionRecord {
    pub fn new(
        id: impl Into<String>,
        statement: impl Into<String>,
        provenance: Provenance,
    ) -> Self {
        Self {
            id: id.into(),
            statement: statement.into(),
            provenance,
        }
    }
}

pub(crate) fn model_identity(model_id: &str) -> AnalysisIdentity {
    AnalysisIdentity {
        solver_id: format!("engineering-models/{model_id}"),
        solver_version: MODEL_VERSION.to_string(),
        kind: AnalysisKind::InProcessKernel,
    }
}

pub(crate) fn require_dimension(
    field: &'static str,
    quantity: &Quantity,
    expected: Dimension,
) -> ModelResult<()> {
    if quantity.dimension() == expected {
        Ok(())
    } else {
        Err(ModelError::DimensionMismatch {
            field,
            expected,
            found: quantity.dimension(),
        })
    }
}

pub(crate) fn value_in(quantity: &Quantity, unit: &str) -> ModelResult<f64> {
    let target = UnitRegistry::si().require(unit)?;
    Ok(quantity.to_unit(target)?.value())
}

pub(crate) fn boundary_value(
    quantity: &Quantity,
    context: &ModelContext,
) -> ModelResult<EngineeringValue> {
    Ok(EngineeringValue::known(
        quantity.clone(),
        context.source.clone(),
        context.revision,
    )?)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_receipt(
    model_id: &str,
    input: &impl Serialize,
    model_expression: &str,
    settings: &impl Serialize,
    context: &ModelContext,
    boundary_conditions: BTreeMap<String, EngineeringValue>,
    convergence: Convergence,
    warnings: Vec<AnalysisWarning>,
    results: Vec<ReceiptResult>,
    uncertainty: Uncertainty,
) -> ModelResult<AnalysisReceipt> {
    let input_digest = Digest::of_json(input).map_err(|error| ModelError::Serialization {
        reason: error.to_string(),
    })?;
    let settings_digest = Digest::of_json(settings).map_err(|error| ModelError::Serialization {
        reason: error.to_string(),
    })?;
    let mut content = ReceiptContent::new(
        input_digest,
        Digest::of_text(model_expression),
        settings_digest,
    );
    content.property_revisions = context.property_revisions.clone();
    content.boundary_conditions = boundary_conditions;
    content.convergence = convergence;
    content.warnings = warnings;
    content.results = results;
    content.uncertainty = uncertainty;
    content.repeatability = Repeatability::deterministic();
    Ok(AnalysisReceipt::kernel(
        model_identity(model_id),
        content,
        0,
    )?)
}

pub(crate) fn warning(
    code: &str,
    message: impl Into<String>,
    severity: WarningSeverity,
) -> AnalysisWarning {
    AnalysisWarning {
        code: code.to_string(),
        message: message.into(),
        severity,
    }
}

pub(crate) fn quantity_result(
    name: &str,
    quantity: Quantity,
    uncertainty: Uncertainty,
) -> ReceiptResult {
    ReceiptResult {
        name: name.to_string(),
        value: ReceiptValue::Quantity(quantity),
        uncertainty,
    }
}

pub(crate) fn unavailable_result(
    name: &str,
    reason: impl Into<String>,
    required: Vec<String>,
) -> ModelResult<ReceiptResult> {
    Ok(ReceiptResult {
        name: name.to_string(),
        value: ReceiptValue::Unavailable(UnavailableValue::new(reason, required)?),
        uncertainty: Uncertainty::Exact,
    })
}

pub(crate) fn require_finite(field: &'static str, value: f64) -> ModelResult<()> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ModelError::InvalidInput {
            field,
            reason: "value is not finite".to_string(),
        })
    }
}

pub(crate) fn require_positive(field: &'static str, value: f64) -> ModelResult<()> {
    require_finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(ModelError::InvalidInput {
            field,
            reason: format!("must be > 0, got {value}"),
        })
    }
}
