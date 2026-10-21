use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, EngineeringValue, MaterialRef, PropertyRevision, Provenance,
    ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Interval, Quantity, Uncertainty};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    unavailable_result, value_in, warning, AssumptionRecord, ModelContext, ModelError, ModelReport,
    ModelResult, ModelStatus, Refusal, RefusalCode, MODEL_VERSION,
};

pub const MATERIAL_PROPERTY_MODEL: &str = "materials/conditional-property-lookup";
const MODEL_EXPRESSION: &str = "applicable(record, query) = material && grade && temper && property && thickness_range && temperature_range && conditions && orientation";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialPropertyRecord {
    pub id: String,
    pub property: String,
    pub value: EngineeringValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialPropertyQuery {
    pub material: MaterialRef,
    pub property: String,
    pub thickness: Option<Quantity>,
    pub temperature: Option<Quantity>,
    pub orientation: Option<engineering_core::Orientation>,
    pub conditions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialPropertyResult {
    pub value: EngineeringValue,
    pub matched_records: Vec<String>,
    pub applicability_evidence: Vec<String>,
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
            "asm-conditional-property",
            "A material property is conditional on material identity, grade, temper, product form/thickness, temperature, orientation and any established process condition.",
            "Materials knowledge pack: conditional property facts.",
        ),
        assumption(
            "asm-no-extrapolation",
            "A property record is used only when every declared condition is satisfied; there is no interpolation or extrapolation outside the record.",
            "Materials knowledge pack: applicability envelope.",
        ),
        assumption(
            "asm-conflict-surface",
            "Two applicable records with different values are a conflict to surface, not a value to average.",
            "Materials knowledge pack: conflict rule.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "property applicability is dimensionless",
            Dimension::SCALAR,
            Dimension::SCALAR,
        )?,
        DimensionCheck::new(
            "thickness comparison requires two lengths",
            Dimension::SCALAR,
            Dimension::LENGTH.checked_div(Dimension::LENGTH)?,
        )?,
        DimensionCheck::new(
            "temperature comparison requires two temperatures",
            Dimension::SCALAR,
            Dimension::TEMPERATURE.checked_div(Dimension::TEMPERATURE)?,
        )?,
    ])
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn same_material(record: &MaterialRef, query: &MaterialRef) -> bool {
    normalize(&record.name) == normalize(&query.name)
        && record
            .grade
            .as_deref()
            .map(normalize)
            .zip(query.grade.as_deref().map(normalize))
            .map_or(record.grade.is_none() && query.grade.is_none(), |(a, b)| {
                a == b
            })
        && record
            .temper
            .as_deref()
            .map(normalize)
            .zip(query.temper.as_deref().map(normalize))
            .map_or(
                record.temper.is_none() && query.temper.is_none(),
                |(a, b)| a == b,
            )
}

fn thickness_contains(
    range: &engineering_core::ThicknessRange,
    value: &Quantity,
) -> ModelResult<bool> {
    require_dimension("thickness", value, Dimension::LENGTH)?;
    let min_unit = range.min.unit().clone();
    let min = range.min.value();
    let max = range.max.to_unit(min_unit.clone())?.value();
    let actual = value.to_unit(min_unit)?.value();
    Ok(actual >= min && actual <= max)
}

fn temperature_contains(interval: &Interval, value: &Quantity) -> ModelResult<bool> {
    require_dimension("temperature", value, Dimension::TEMPERATURE)?;
    let kelvin = value_in(value, "K")?;
    Ok(interval.contains(kelvin))
}

fn record_applies(
    record: &MaterialPropertyRecord,
    query: &MaterialPropertyQuery,
) -> ModelResult<Result<(), Vec<String>>> {
    let mut failures = Vec::new();
    if normalize(&record.property) != normalize(&query.property) {
        failures.push(format!(
            "record property {:?} does not match query property {:?}",
            record.property, query.property
        ));
        return Ok(Err(failures));
    }
    let Some(material) = &record.value.material else {
        failures.push("record has no material identity".to_string());
        return Ok(Err(failures));
    };
    if !same_material(material, &query.material) {
        failures.push(format!(
            "record material {:?}/{:?}/{:?} does not match query {:?}/{:?}/{:?}",
            material.name,
            material.grade,
            material.temper,
            query.material.name,
            query.material.grade,
            query.material.temper
        ));
    }
    if let Some(range) = &record.value.thickness_range {
        match &query.thickness {
            Some(thickness) => {
                if !thickness_contains(range, thickness)? {
                    failures.push(format!(
                        "thickness {} is outside the record range",
                        thickness
                    ));
                }
            }
            None => failures.push(
                "record is thickness-bound but the query thickness is unavailable".to_string(),
            ),
        }
    }
    if let Some(interval) = record.value.applicability.temperature {
        match &query.temperature {
            Some(temperature) => {
                if !temperature_contains(&interval, temperature)? {
                    failures.push(format!(
                        "temperature {} is outside the record applicability",
                        temperature
                    ));
                }
            }
            None => failures.push(
                "record is temperature-bound but the query temperature is unavailable".to_string(),
            ),
        }
    }
    if let Some(orientation) = record.value.orientation {
        match query.orientation {
            Some(actual) if actual == orientation => {}
            Some(actual) => failures.push(format!(
                "orientation {actual:?} does not match the record {orientation:?}"
            )),
            None => failures.push(
                "record is orientation-bound but the query orientation is unavailable".to_string(),
            ),
        }
    }
    let established: Vec<String> = query
        .conditions
        .iter()
        .map(|item| normalize(item))
        .collect();
    for condition in &record.value.applicability.conditions {
        if !established.contains(&normalize(condition)) {
            failures.push(format!(
                "required condition {condition:?} is not established"
            ));
        }
    }
    if failures.is_empty() {
        Ok(Ok(()))
    } else {
        Ok(Err(failures))
    }
}

fn records_for_query<'a>(
    records: &'a [MaterialPropertyRecord],
    query: &MaterialPropertyQuery,
) -> Vec<&'a MaterialPropertyRecord> {
    records
        .iter()
        .filter(|record| {
            normalize(&record.property) == normalize(&query.property)
                && record.value.material.as_ref().is_some_and(|material| {
                    normalize(&material.name) == normalize(&query.material.name)
                        && material.grade.as_deref().map(normalize)
                            == query.material.grade.as_deref().map(normalize)
                })
        })
        .collect()
}

fn refusal_report(
    query: &MaterialPropertyQuery,
    records: &[MaterialPropertyRecord],
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<MaterialPropertyResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    if let Some(temperature) = &query.temperature {
        bcs.insert(
            "temperature".to_string(),
            boundary_value(temperature, context)?,
        );
    }
    if let Some(thickness) = &query.thickness {
        bcs.insert("thickness".to_string(), boundary_value(thickness, context)?);
    }
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let results = vec![unavailable_result(
        "property_value",
        reason.clone(),
        refusal.evidence.clone(),
    )?];
    let receipt = build_receipt(
        MATERIAL_PROPERTY_MODEL,
        &(query, records),
        MODEL_EXPRESSION,
        query,
        context,
        bcs,
        Convergence::exact(),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Refused,
        value: ValueState::unavailable(reason, vec!["applicable material property".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

fn unavailable_report(
    query: &MaterialPropertyQuery,
    records: &[MaterialPropertyRecord],
    context: &ModelContext,
    reason: String,
    required: Vec<String>,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<MaterialPropertyResult>> {
    let mut bcs = BTreeMap::new();
    if let Some(temperature) = &query.temperature {
        bcs.insert(
            "temperature".to_string(),
            boundary_value(temperature, context)?,
        );
    }
    let warnings = vec![warning(
        "model.unavailable",
        reason.clone(),
        WarningSeverity::Warning,
    )];
    let results = vec![unavailable_result(
        "property_value",
        reason.clone(),
        required.clone(),
    )?];
    let receipt = build_receipt(
        MATERIAL_PROPERTY_MODEL,
        &(query, records),
        MODEL_EXPRESSION,
        query,
        context,
        bcs,
        Convergence::exact(),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Unavailable,
        value: ValueState::unavailable(reason, required)?,
        refusal: None,
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn lookup_material_property(
    query: &MaterialPropertyQuery,
    records: &[MaterialPropertyRecord],
    context: &ModelContext,
) -> ModelResult<ModelReport<MaterialPropertyResult>> {
    let checks = checks()?;
    query.material.validate()?;
    if query.property.trim().is_empty() {
        return Err(ModelError::InvalidInput {
            field: "property",
            reason: "must not be empty".to_string(),
        });
    }
    if let Some(thickness) = &query.thickness {
        require_dimension("thickness", thickness, Dimension::LENGTH)?;
    }
    if let Some(temperature) = &query.temperature {
        require_dimension("temperature", temperature, Dimension::TEMPERATURE)?;
    }
    for record in records {
        record.value.validate()?;
    }
    let candidates = records_for_query(records, query);
    if candidates.is_empty() {
        let required = vec![
            format!(
                "revision-bound {} property for {} {:?} {:?}",
                query.property, query.material.name, query.material.grade, query.material.temper
            ),
            "the product form, thickness and test orientation".to_string(),
            "the service temperature range".to_string(),
        ];
        return unavailable_report(
            query,
            records,
            context,
            format!(
                "no property record exists for {} {} {:?} {:?}",
                query.property, query.material.name, query.material.grade, query.material.temper
            ),
            required,
            checks,
        );
    }
    let mut matched = Vec::new();
    let mut mismatches = Vec::new();
    for record in &candidates {
        match record_applies(record, query)? {
            Ok(()) => matched.push(*record),
            Err(reasons) => mismatches.push(format!("{}: {}", record.id, reasons.join("; "))),
        }
    }
    if matched.is_empty() {
        return refusal_report(
            query,
            records,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                format!(
                    "{} property records exist for {} but none covers the query conditions",
                    candidates.len(),
                    query.material.name
                ),
                {
                    let mut evidence = mismatches;
                    evidence
                        .push("no interpolation or extrapolation outside the record".to_string());
                    evidence
                },
            ),
            checks,
        );
    }
    let first = matched[0];
    let mut matched_ids = vec![first.id.clone()];
    let mut reference = match &first.value.quantity {
        ValueState::Known(quantity) => Some(quantity.clone()),
        ValueState::Unavailable(_) => None,
    };
    for record in matched.iter().skip(1) {
        match &record.value.quantity {
            ValueState::Known(quantity) => {
                if let Some(existing) = &reference {
                    let same = quantity
                        .to_unit(existing.unit().clone())?
                        .approx_eq(existing, 0.0)?;
                    if !same {
                        return refusal_report(
                            query,
                            records,
                            context,
                            Refusal::new(
                                RefusalCode::AmbiguousProperty,
                                format!(
                                    "conflicting applicable property records for {}: {}",
                                    query.property,
                                    matched
                                        .iter()
                                        .map(|record| record.id.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                                vec!["a single revision-bound property source or a documented resolution".to_string()],
                            ),
                            checks,
                        );
                    }
                } else {
                    reference = Some(quantity.clone());
                }
            }
            ValueState::Unavailable(_) => {
                return unavailable_report(
                    query,
                    records,
                    context,
                    format!(
                        "the applicable property record {} carries a typed unavailable value",
                        record.id
                    ),
                    vec!["a revision-bound property value".to_string()],
                    checks,
                );
            }
        }
        matched_ids.push(record.id.clone());
    }
    let record = first;
    match &record.value.quantity {
        ValueState::Unavailable(reason) => unavailable_report(
            query,
            records,
            context,
            reason.reason.clone(),
            reason.required.clone(),
            checks,
        ),
        ValueState::Known(quantity) => {
            let mut value = record.value.clone();
            value.material = Some(query.material.clone());
            value.temperature = query.temperature.clone();
            value.orientation = query.orientation;
            value.validate()?;
            let mut local = context.clone();
            local.property_revisions.push(PropertyRevision::new(
                query.property.clone(),
                value.revision,
            )?);
            let mut evidence = vec![
                format!(
                    "matched property record {} from revision {}",
                    record.id, value.revision
                ),
                format!("property source: {}", value.source.citation),
            ];
            if let Some(range) = &value.thickness_range {
                evidence.push(format!(
                    "applicable thickness range {}..{} {}",
                    range.min.value(),
                    range.max.value(),
                    range.min.unit()
                ));
            }
            if let Some(interval) = value.applicability.temperature {
                evidence.push(format!(
                    "applicable temperature range {}..{} K",
                    interval.lower(),
                    interval.upper()
                ));
            }
            let mut bcs = BTreeMap::new();
            if let Some(temperature) = &query.temperature {
                bcs.insert(
                    "temperature".to_string(),
                    boundary_value(temperature, context)?,
                );
            }
            if let Some(thickness) = &query.thickness {
                bcs.insert("thickness".to_string(), boundary_value(thickness, context)?);
            }
            let uncertainty = value.uncertainty;
            let results = vec![
                quantity_result("property_value", quantity.clone(), uncertainty),
                quantity_result(
                    "matched_record_count",
                    Quantity::dimensionless(matched_ids.len() as f64)?,
                    Uncertainty::Exact,
                ),
            ];
            let receipt = build_receipt(
                MATERIAL_PROPERTY_MODEL,
                &(query, records),
                MODEL_EXPRESSION,
                query,
                &local,
                bcs,
                Convergence::exact(),
                Vec::new(),
                results,
                uncertainty,
            )?;
            Ok(ModelReport {
                status: ModelStatus::Solved,
                value: ValueState::known(MaterialPropertyResult {
                    value,
                    matched_records: matched_ids,
                    applicability_evidence: evidence,
                }),
                refusal: None,
                receipt,
                checks,
                assumptions: assumptions(),
                warnings: Vec::new(),
                uncertainty,
                cross_check: Some(crate::common::CrossCheckEvidence::new(
                    "query conditions satisfy the record applicability",
                    1.0,
                    1.0,
                    0.0,
                )),
            })
        }
    }
}

pub fn identity() -> engineering_core::AnalysisIdentity {
    model_identity(MATERIAL_PROPERTY_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;
