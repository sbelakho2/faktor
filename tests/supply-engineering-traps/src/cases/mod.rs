pub(crate) mod engineering;
pub(crate) mod fea;
pub(crate) mod machine;
pub(crate) mod materials;
pub(crate) mod supply;
pub(crate) mod units;

use engineering_core::{FalsifiableTest, Provenance, ProvenanceKind, RevisionId};
use engineering_knowledge::KnowledgeRegistry;
use engineering_models::common::ModelContext;
use faktor_supply_core::Money;

use crate::{BenchError, CaseReport};

/// Fixed dispatch table. The order is the suite order and is part of the
/// deterministic report.
pub(crate) const CASE_IDS: &[&str] = &[
    engineering::OPAMP_ID,
    engineering::FLYBACK_ID,
    engineering::PCB_ID,
    materials::ALUMINIUM_ID,
    machine::FASTENER_ID,
    machine::FATIGUE_ID,
    engineering::THERMAL_ID,
    engineering::MOLDING_ID,
    fea::FEA_ID,
    supply::SUPPLIER_ID,
    supply::HS_ID,
    units::UNIT_ID,
    supply::UNKNOWN_COST_ID,
];

type CaseFn = fn() -> Result<CaseReport, BenchError>;

const CASES: &[(&str, CaseFn)] = &[
    (engineering::OPAMP_ID, engineering::opamp),
    (engineering::FLYBACK_ID, engineering::flyback),
    (engineering::PCB_ID, engineering::pcb_alternate),
    (materials::ALUMINIUM_ID, materials::aluminium),
    (machine::FASTENER_ID, machine::fastener),
    (machine::FATIGUE_ID, machine::fatigue),
    (engineering::THERMAL_ID, engineering::thermal_contact),
    (engineering::MOLDING_ID, engineering::molding),
    (fea::FEA_ID, fea::mesh_convergence),
    (supply::SUPPLIER_ID, supply::supplier),
    (supply::HS_ID, supply::hs_classification),
    (units::UNIT_ID, units::unit_conversion),
    (supply::UNKNOWN_COST_ID, supply::unknown_cost),
];

pub(crate) fn dispatch(id: &str) -> Result<CaseReport, BenchError> {
    let (_, function) = CASES
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .ok_or_else(|| BenchError::UnknownCase(id.to_string()))?;
    function()
}

pub(crate) fn case_error(id: &str, detail: impl std::fmt::Display) -> BenchError {
    BenchError::Case {
        id: id.to_string(),
        detail: detail.to_string(),
    }
}

/// Fixture provenance is deliberately an assumption: benchmark inputs are
/// synthetic and never claim to be datasheet, supplier, or standards evidence.
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

pub(crate) fn falsifiable(
    claim: impl Into<String>,
    procedure: impl Into<String>,
    pass_criterion: impl Into<String>,
    refutation: impl Into<String>,
    instrument: impl Into<String>,
    evidence: &[&str],
) -> FalsifiableTest {
    FalsifiableTest {
        claim: claim.into(),
        procedure: procedure.into(),
        pass_criterion: pass_criterion.into(),
        refutation: refutation.into(),
        instrument: instrument.into(),
        evidence: evidence.iter().map(|item| (*item).to_string()).collect(),
    }
}

pub(crate) fn registry() -> Result<KnowledgeRegistry, BenchError> {
    KnowledgeRegistry::bundled().map_err(|error| case_error("knowledge", error))
}

pub(crate) fn pack_evidence(
    registry: &KnowledgeRegistry,
    pack_id: &str,
) -> Result<Vec<String>, BenchError> {
    let pack = registry
        .latest(pack_id)
        .ok_or_else(|| case_error(pack_id, "bundled knowledge pack is missing"))?;
    let authority = pack
        .authority()
        .ok_or_else(|| case_error(pack_id, "knowledge pack has no authority digest"))?;
    Ok(vec![format!(
        "knowledge authority {}@{} digest {} scope {:?}",
        authority.pack_id,
        authority.version,
        authority.digest.to_hex(),
        authority.scope
    )])
}

pub(crate) fn pack_contains(
    registry: &KnowledgeRegistry,
    pack_id: &str,
    needle: &str,
) -> Result<bool, BenchError> {
    let pack = registry
        .latest(pack_id)
        .ok_or_else(|| case_error(pack_id, "bundled knowledge pack is missing"))?;
    let haystack = serde_json::to_string(pack)
        .map_err(|error| case_error(pack_id, format!("pack serialization: {error}")))?;
    Ok(haystack.contains(needle))
}

pub(crate) fn pack_has_id(
    registry: &KnowledgeRegistry,
    pack_id: &str,
    item_id: &str,
) -> Result<bool, BenchError> {
    let pack = registry
        .latest(pack_id)
        .ok_or_else(|| case_error(pack_id, "bundled knowledge pack is missing"))?;
    let found = pack.terminology.iter().any(|item| item.id == item_id)
        || pack.models.iter().any(|item| item.id == item_id)
        || pack.equations.iter().any(|item| item.id == item_id)
        || pack.assumptions.iter().any(|item| item.id == item_id)
        || pack.validity_domains.iter().any(|item| item.id == item_id)
        || pack.property_sources.iter().any(|item| item.id == item_id)
        || pack.design_rules.iter().any(|item| item.id == item_id)
        || pack
            .manufacturing_rules
            .iter()
            .any(|item| item.id == item_id)
        || pack.failure_modes.iter().any(|item| item.id == item_id)
        || pack
            .diagnostic_signatures
            .iter()
            .any(|item| item.id == item_id)
        || pack.test_methods.iter().any(|item| item.id == item_id)
        || pack
            .standards_mappings
            .iter()
            .any(|item| item.id == item_id)
        || pack.worked_cases.iter().any(|item| item.id == item_id)
        || pack.counterexamples.iter().any(|item| item.id == item_id)
        || pack.known_traps.iter().any(|item| item.id == item_id);
    Ok(found)
}

pub(crate) fn knowledge_pack_ids(registry: &KnowledgeRegistry) -> Vec<String> {
    registry
        .packs()
        .map(|pack| format!("{}@{}", pack.id, pack.version))
        .collect()
}

pub(crate) fn money_text(amount: Money) -> String {
    let micros = amount.micros();
    let sign = if micros < 0 { "-" } else { "" };
    let magnitude = micros.unsigned_abs();
    format!(
        "{sign}{}.{:06} {}",
        magnitude / 1_000_000,
        magnitude % 1_000_000,
        amount.currency.as_str()
    )
}
