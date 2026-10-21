use engineering_core::{
    Applicability, EngineeringValue, GateId, MaterialRef, Provenance, ProvenanceKind, RevisionId,
    ThicknessRange, ValueState,
};
use engineering_units::{Quantity, UnitRegistry};

use super::{case_error, falsifiable, fixture_provenance, registry};
use crate::{CaseBuilder, CaseOutcome, CaseReport, ExpectedKind, Metrics};

pub(crate) const ALUMINIUM_ID: &str = "materials/aluminium-temper-thickness";

fn contains_thickness(range: &ThicknessRange, required: &Quantity) -> bool {
    let Ok(max_in_min) = range.max.to_unit(range.min.unit().clone()) else {
        return false;
    };
    let Ok(required_in_min) = required.to_unit(range.min.unit().clone()) else {
        return false;
    };
    range
        .min
        .compare(&required_in_min)
        .is_ok_and(|ordering| ordering != std::cmp::Ordering::Greater)
        && required_in_min
            .compare(&max_in_min)
            .is_ok_and(|ordering| ordering != std::cmp::Ordering::Greater)
}

pub(crate) fn aluminium() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let units = UnitRegistry::si();
    let required_temper = "T651";
    let required_thickness = Quantity::new(
        80.0,
        units
            .require("mm")
            .map_err(|error| case_error(ALUMINIUM_ID, error))?,
    )
    .map_err(|error| case_error(ALUMINIUM_ID, error))?;
    let candidate = MaterialRef::new(
        "aluminium",
        RevisionId::new(1),
        fixture_provenance("benchmark fixture: hypothetical candidate material reference"),
    )
    .map_err(|error| case_error(ALUMINIUM_ID, error))?
    .with_grade("6061")
    .with_temper("T6");
    let thin_sheet_range = ThicknessRange::new(
        Quantity::new(
            0.0,
            units
                .require("mm")
                .map_err(|error| case_error(ALUMINIUM_ID, error))?,
        )
        .map_err(|error| case_error(ALUMINIUM_ID, error))?,
        Quantity::new(
            12.0,
            units
                .require("mm")
                .map_err(|error| case_error(ALUMINIUM_ID, error))?,
        )
        .map_err(|error| case_error(ALUMINIUM_ID, error))?,
    )
    .map_err(|error| case_error(ALUMINIUM_ID, error))?;
    let required = vec![
        "revision-bound yield/tensile property source for 6061-T651 plate at the required thickness".to_string(),
        "temper designation confirmed on the mill certificate".to_string(),
        "product form, thickness and test direction/orientation".to_string(),
        "specification and acceptance basis (for example ASTM B209/B557 or an equivalent licensed source)".to_string(),
        "property uncertainty and lot-to-lot scatter".to_string(),
    ];
    let value = EngineeringValue::unavailable(
        "no revision-bound strength value applies to 6061-T651 at 80 mm; the available reference is a T6 thin-sheet condition and cannot be substituted",
        required.clone(),
        Provenance::new(
            ProvenanceKind::Assumption,
            "benchmark fixture: no material property value is supplied",
        )
        .map_err(|error| case_error(ALUMINIUM_ID, error))?,
        RevisionId::new(1),
    )
    .map_err(|error| case_error(ALUMINIUM_ID, error))?
    .with_material(candidate.clone())
    .with_thickness_range(thin_sheet_range.clone())
    .with_applicability(Applicability::unrestricted().with_condition("thin sheet, T6 temper"));
    value
        .validate()
        .map_err(|error| case_error(ALUMINIUM_ID, error))?;
    let temper_mismatch = candidate.temper.as_deref() != Some(required_temper);
    let thickness_outside = !contains_thickness(&thin_sheet_range, &required_thickness);
    let unavailable = matches!(value.quantity, ValueState::Unavailable(_));
    let material_authority = registry.packs().any(|pack| {
        serde_json::to_string(pack).is_ok_and(|text| {
            let text = text.to_lowercase();
            text.contains("6061")
                && (text.contains("temper") || text.contains("t651"))
                && text.contains("yield")
        })
    });
    let authority = true;
    let physics = temper_mismatch && thickness_outside && unavailable;
    let missing = required.clone();
    let test = falsifiable(
        "6061-T651 plate at 80 mm has adequate strength for the design load.",
        "Cut specimens from the actual plate at the required location and orientation and perform tensile tests at the service temperature; compare Rp0.2/Rm and elongation with the design allowables.",
        "The measured minimum properties, with the lot scatter, meet the design allowables with the required margin.",
        "The measured properties fall below the design allowables, or the temper/thickness/test direction does not match the qualified condition.",
        "Calibrated universal tensile machine, extensometer, mill certificate, ASTM B557/E8 fixtures.",
        &[
            "the candidate material reference is T6 thin sheet",
            "the design requires T651 plate at 80 mm",
            "no material property pack is bundled",
        ],
    );
    let mut evidence = vec![
        format!(
            "bundled knowledge packs: {}",
            registry
                .packs()
                .map(|pack| format!("{}@{}", pack.id, pack.version))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        if material_authority {
            "a bundled pack mentions 6061 temper/yield, but no revision-bound value for T651 80 mm is supplied".to_string()
        } else {
            "no bundled pack supplies a revision-bound 6061 temper/thickness yield source"
                .to_string()
        },
        format!(
            "candidate material: {} grade {:?} temper {:?}",
            candidate.name, candidate.grade, candidate.temper
        ),
        format!(
            "candidate thickness range: {:?}..{:?} mm",
            thin_sheet_range.min.value(),
            thin_sheet_range.max.value()
        ),
        format!(
            "design requirement: 6061-{required_temper} at {:.1} mm",
            required_thickness.value()
        ),
    ];
    evidence.push(format!(
        "temper mismatch: {temper_mismatch}; thickness outside candidate applicability: {thickness_outside}"
    ));
    CaseBuilder::new(ALUMINIUM_ID, "materials", "Wrong temper and thickness invalidate an aluminium strength claim", ExpectedKind::ResearchRequired)
        .shallow("Aluminium 6061 has a familiar yield strength, so any 6061 part can use it.")
        .outcome(CaseOutcome::ResearchRequired {
            gate: GateId::Authority,
            reason: "no revision-bound property authority covers 6061-T651 at the required plate thickness; a T6 thin-sheet reference is not applicable".to_string(),
            sources_required: required,
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(authority)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(true),
        )
        .evidence(evidence)
        .missing(missing)
        .boundaries(vec![
            "temper designation must match the qualified material condition (T651, not T6)".to_string(),
            "the property must apply to the actual product form and thickness (80 mm plate, not thin sheet)".to_string(),
            "test direction/orientation and service temperature must be inside the property's applicability".to_string(),
        ])
        .uncertainty("No strength value or uncertainty is asserted; the property is unavailable until a revision-bound source covers the actual temper and thickness.")
        .test(test)
        .build()
}
