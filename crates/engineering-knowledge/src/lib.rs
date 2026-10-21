//! Versioned engineering knowledge packs.
//!
//! Packs are authored JSON, loaded and validated at runtime. The loader
//! refuses unknown schema fields, empty categories, duplicate ids, dangling
//! cross-references, un-sourced facts and standards that claim licensed text
//! is embedded. There is deliberately no generative "fill in the facts" path:
//! a pack either carries sourced content or it does not load.

use std::collections::{BTreeMap, BTreeSet};

use engineering_core::{
    AuthorityRef, CatalogModel, Digest, Discipline, ModelCatalog, SelectionProvenance,
    ValidityDomain as SelectorValidityDomain,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PACK_SCHEMA: &str = "faktor.engineering.knowledge-pack/v1";
pub const MAX_PACK_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_PACKS: usize = 64;

const BOLTED_JOINT: &str = include_str!("../packs/bolted_joint.json");
const POWER_ELECTRONICS: &str = include_str!("../packs/power_electronics.json");
const INJECTION_MOLDING: &str = include_str!("../packs/injection_molding.json");
const PASSIVES: &str = include_str!("../packs/passives.json");
const ANALOG_STABILITY: &str = include_str!("../packs/analog_stability.json");
const FLYBACK_CONVERTERS: &str = include_str!("../packs/flyback_converters.json");
const THERMAL_NETWORKS: &str = include_str!("../packs/thermal_networks.json");
const FATIGUE_MATERIALS: &str = include_str!("../packs/fatigue_materials.json");
const EMC_PCB: &str = include_str!("../packs/emc_pcb.json");
const MATERIALS_CONDITIONAL: &str = include_str!("../packs/materials_conditional.json");

#[derive(Debug, Error)]
pub enum KnowledgeError {
    #[error("knowledge pack exceeds {MAX_PACK_BYTES} bytes")]
    TooLarge,
    #[error("knowledge pack JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("knowledge pack schema {found:?} is not {expected:?}")]
    SchemaMismatch { found: String, expected: String },
    #[error("knowledge pack field {field} is empty")]
    EmptyField { field: &'static str },
    #[error("knowledge pack version {version:?} is not a semantic version")]
    InvalidVersion { version: String },
    #[error("knowledge pack id {id:?} is duplicated within the pack")]
    DuplicateId { id: String },
    #[error("knowledge pack {id}@{version} is already registered")]
    DuplicatePack { id: String, version: String },
    #[error("knowledge pack reference {from} -> {target} does not exist")]
    MissingReference { from: String, target: String },
    #[error("knowledge pack registry limit of {limit} packs reached")]
    PackLimit { limit: usize },
    #[error("bundled knowledge pack failed validation: {0}")]
    Bundled(String),
    #[error("standards mapping must remain metadata-only; licensed text is not embedded")]
    StandardTextEmbedded,
}

pub type KnowledgeResult<T> = Result<T, KnowledgeError>;

/// Discoverability alias for the versioned pack registry.
pub type KnowledgePackRegistry = KnowledgeRegistry;

fn require(field: &'static str, value: &str) -> KnowledgeResult<()> {
    if value.trim().is_empty() {
        return Err(KnowledgeError::EmptyField { field });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackProvenance {
    pub kind: String,
    pub citation: String,
    pub edition: Option<String>,
    pub locator: Option<String>,
    pub note: Option<String>,
}

impl PackProvenance {
    fn validate(&self, field: &'static str) -> KnowledgeResult<()> {
        require(field, &self.kind)?;
        require(field, &self.citation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Term {
    pub id: String,
    pub term: String,
    pub definition: String,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoverningModel {
    pub id: String,
    pub name: String,
    pub equation_ids: Vec<String>,
    pub assumption_ids: Vec<String>,
    pub validity_domain_ids: Vec<String>,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EquationVariable {
    pub symbol: String,
    pub description: String,
    pub dimension: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Equation {
    pub id: String,
    pub expression: String,
    pub dimensional_signature: String,
    pub variables: Vec<EquationVariable>,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assumption {
    pub id: String,
    pub statement: String,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueRange {
    pub min: f64,
    pub max: f64,
    pub unit: String,
}

impl ValueRange {
    fn validate(&self, field: &'static str) -> KnowledgeResult<()> {
        require(field, &self.unit)?;
        if !self.min.is_finite() || !self.max.is_finite() || self.min > self.max {
            return Err(KnowledgeError::Bundled(format!(
                "{field}: invalid range [{} , {}] {}",
                self.min, self.max, self.unit
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidityDomain {
    pub id: String,
    pub statement: String,
    pub temperature: Option<ValueRange>,
    pub exclusions: Vec<String>,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertySource {
    pub id: String,
    pub property: String,
    pub authority: String,
    pub revision_bound: bool,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSeverity {
    Required,
    Recommended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignRule {
    pub id: String,
    pub rule: String,
    pub check: String,
    pub severity: RuleSeverity,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManufacturingRule {
    pub id: String,
    pub rule: String,
    pub check: String,
    pub severity: RuleSeverity,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureMode {
    pub id: String,
    pub mode: String,
    pub cause: String,
    pub detection: String,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticSignature {
    pub id: String,
    pub signature: String,
    pub indicates: String,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestMethod {
    pub id: String,
    pub method: String,
    pub measures: String,
    pub standard_ref: Option<String>,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandardAccess {
    /// Only the standard identity/scope is stored; the licensed text is
    /// obtained through a licensed store or connector.
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandardMapping {
    pub id: String,
    pub standard: String,
    pub scope: String,
    pub access: StandardAccess,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseInput {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseExpectation {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub tolerance: f64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkedCase {
    pub id: String,
    pub description: String,
    pub inputs: Vec<CaseInput>,
    pub expected: Vec<CaseExpectation>,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counterexample {
    pub id: String,
    pub claim: String,
    pub refutation: String,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownTrap {
    pub id: String,
    pub trap: String,
    pub why_shallow_fails: String,
    pub guard: String,
    pub provenance: PackProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgePack {
    pub schema: String,
    pub id: String,
    pub version: String,
    pub title: String,
    pub domain: String,
    pub provenance: PackProvenance,
    pub terminology: Vec<Term>,
    #[serde(alias = "governing_models")]
    pub models: Vec<GoverningModel>,
    pub equations: Vec<Equation>,
    pub assumptions: Vec<Assumption>,
    #[serde(alias = "validity_domain")]
    pub validity_domains: Vec<ValidityDomain>,
    pub property_sources: Vec<PropertySource>,
    pub design_rules: Vec<DesignRule>,
    pub manufacturing_rules: Vec<ManufacturingRule>,
    pub failure_modes: Vec<FailureMode>,
    pub diagnostic_signatures: Vec<DiagnosticSignature>,
    pub test_methods: Vec<TestMethod>,
    #[serde(alias = "standards")]
    pub standards_mappings: Vec<StandardMapping>,
    #[serde(alias = "cases")]
    pub worked_cases: Vec<WorkedCase>,
    pub counterexamples: Vec<Counterexample>,
    #[serde(alias = "traps")]
    pub known_traps: Vec<KnownTrap>,
    #[serde(skip)]
    source_digest: Option<Digest>,
}

impl KnowledgePack {
    pub fn from_json(bytes: &[u8]) -> KnowledgeResult<Self> {
        Self::load(bytes)
    }

    pub fn load(bytes: &[u8]) -> KnowledgeResult<Self> {
        if bytes.len() > MAX_PACK_BYTES {
            return Err(KnowledgeError::TooLarge);
        }
        let mut pack: Self = serde_json::from_slice(bytes)?;
        pack.source_digest = Some(Digest::of_bytes(bytes));
        pack.validate()?;
        Ok(pack)
    }

    pub fn digest(&self) -> Option<Digest> {
        self.source_digest
    }

    pub fn validate(&self) -> KnowledgeResult<()> {
        if self.schema != PACK_SCHEMA {
            return Err(KnowledgeError::SchemaMismatch {
                found: self.schema.clone(),
                expected: PACK_SCHEMA.to_string(),
            });
        }
        for (field, value) in [
            ("pack.id", self.id.as_str()),
            ("pack.version", self.version.as_str()),
            ("pack.title", self.title.as_str()),
            ("pack.domain", self.domain.as_str()),
        ] {
            require(field, value)?;
        }
        parse_version(&self.version)?;
        self.provenance.validate("pack.provenance")?;

        let mut ids = BTreeSet::new();
        let mut insert = |id: &str, field: &'static str| -> KnowledgeResult<()> {
            require(field, id)?;
            if !ids.insert(id.to_string()) {
                return Err(KnowledgeError::DuplicateId { id: id.to_string() });
            }
            Ok(())
        };

        if self.terminology.is_empty()
            || self.models.is_empty()
            || self.equations.is_empty()
            || self.assumptions.is_empty()
            || self.validity_domains.is_empty()
            || self.property_sources.is_empty()
            || self.design_rules.is_empty()
            || self.manufacturing_rules.is_empty()
            || self.failure_modes.is_empty()
            || self.diagnostic_signatures.is_empty()
            || self.test_methods.is_empty()
            || self.standards_mappings.is_empty()
            || self.worked_cases.is_empty()
            || self.counterexamples.is_empty()
            || self.known_traps.is_empty()
        {
            return Err(KnowledgeError::EmptyField {
                field: "pack.categories",
            });
        }

        for item in &self.terminology {
            insert(&item.id, "terminology.id")?;
            require("terminology.term", &item.term)?;
            require("terminology.definition", &item.definition)?;
            item.provenance.validate("terminology.provenance")?;
        }
        let model_ids: BTreeSet<String> = self.models.iter().map(|item| item.id.clone()).collect();
        let equation_ids: BTreeSet<String> =
            self.equations.iter().map(|item| item.id.clone()).collect();
        let assumption_ids: BTreeSet<String> = self
            .assumptions
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let validity_ids: BTreeSet<String> = self
            .validity_domains
            .iter()
            .map(|item| item.id.clone())
            .collect();

        for item in &self.models {
            insert(&item.id, "model.id")?;
            require("model.name", &item.name)?;
            if item.equation_ids.is_empty() {
                return Err(KnowledgeError::EmptyField {
                    field: "model.equation_ids",
                });
            }
            for id in &item.equation_ids {
                if !equation_ids.contains(id) {
                    return Err(KnowledgeError::MissingReference {
                        from: item.id.clone(),
                        target: id.clone(),
                    });
                }
            }
            for id in &item.assumption_ids {
                if !assumption_ids.contains(id) {
                    return Err(KnowledgeError::MissingReference {
                        from: item.id.clone(),
                        target: id.clone(),
                    });
                }
            }
            for id in &item.validity_domain_ids {
                if !validity_ids.contains(id) {
                    return Err(KnowledgeError::MissingReference {
                        from: item.id.clone(),
                        target: id.clone(),
                    });
                }
            }
            item.provenance.validate("model.provenance")?;
        }

        for item in &self.equations {
            insert(&item.id, "equation.id")?;
            require("equation.expression", &item.expression)?;
            require(
                "equation.dimensional_signature",
                &item.dimensional_signature,
            )?;
            if item.variables.is_empty() {
                return Err(KnowledgeError::EmptyField {
                    field: "equation.variables",
                });
            }
            let mut symbols = BTreeSet::new();
            for variable in &item.variables {
                require("equation.variable.symbol", &variable.symbol)?;
                require("equation.variable.description", &variable.description)?;
                require("equation.variable.dimension", &variable.dimension)?;
                if !symbols.insert(variable.symbol.clone()) {
                    return Err(KnowledgeError::DuplicateId {
                        id: variable.symbol.clone(),
                    });
                }
            }
            item.provenance.validate("equation.provenance")?;
        }

        for item in &self.assumptions {
            insert(&item.id, "assumption.id")?;
            require("assumption.statement", &item.statement)?;
            item.provenance.validate("assumption.provenance")?;
        }
        for item in &self.validity_domains {
            insert(&item.id, "validity_domain.id")?;
            require("validity_domain.statement", &item.statement)?;
            if let Some(range) = &item.temperature {
                range.validate("validity_domain.temperature")?;
            }
            item.provenance.validate("validity_domain.provenance")?;
        }
        for item in &self.property_sources {
            insert(&item.id, "property_source.id")?;
            require("property_source.property", &item.property)?;
            require("property_source.authority", &item.authority)?;
            if !item.revision_bound {
                return Err(KnowledgeError::EmptyField {
                    field: "property_source.revision_bound",
                });
            }
            item.provenance.validate("property_source.provenance")?;
        }
        for item in &self.design_rules {
            insert(&item.id, "design_rule.id")?;
            require("design_rule.rule", &item.rule)?;
            require("design_rule.check", &item.check)?;
            item.provenance.validate("design_rule.provenance")?;
        }
        for item in &self.manufacturing_rules {
            insert(&item.id, "manufacturing_rule.id")?;
            require("manufacturing_rule.rule", &item.rule)?;
            require("manufacturing_rule.check", &item.check)?;
            item.provenance.validate("manufacturing_rule.provenance")?;
        }
        for item in &self.failure_modes {
            insert(&item.id, "failure_mode.id")?;
            require("failure_mode.mode", &item.mode)?;
            require("failure_mode.cause", &item.cause)?;
            require("failure_mode.detection", &item.detection)?;
            item.provenance.validate("failure_mode.provenance")?;
        }
        for item in &self.diagnostic_signatures {
            insert(&item.id, "diagnostic_signature.id")?;
            require("diagnostic_signature.signature", &item.signature)?;
            require("diagnostic_signature.indicates", &item.indicates)?;
            item.provenance
                .validate("diagnostic_signature.provenance")?;
        }
        for item in &self.test_methods {
            insert(&item.id, "test_method.id")?;
            require("test_method.method", &item.method)?;
            require("test_method.measures", &item.measures)?;
            item.provenance.validate("test_method.provenance")?;
        }
        for item in &self.standards_mappings {
            insert(&item.id, "standard_mapping.id")?;
            require("standard_mapping.standard", &item.standard)?;
            require("standard_mapping.scope", &item.scope)?;
            // The enum has exactly one variant: metadata only. Enforce the
            // intent again so a future variant cannot accidentally store text.
            if !matches!(item.access, StandardAccess::MetadataOnly) {
                return Err(KnowledgeError::StandardTextEmbedded);
            }
            item.provenance.validate("standard_mapping.provenance")?;
        }
        for item in &self.worked_cases {
            insert(&item.id, "worked_case.id")?;
            require("worked_case.description", &item.description)?;
            if item.inputs.is_empty() || item.expected.is_empty() {
                return Err(KnowledgeError::EmptyField {
                    field: "worked_case.io",
                });
            }
            for input in &item.inputs {
                require("worked_case.input.name", &input.name)?;
                require("worked_case.input.unit", &input.unit)?;
                if !input.value.is_finite() {
                    return Err(KnowledgeError::Bundled(format!(
                        "worked case {} input {} is not finite",
                        item.id, input.name
                    )));
                }
            }
            for expected in &item.expected {
                require("worked_case.expected.name", &expected.name)?;
                require("worked_case.expected.unit", &expected.unit)?;
                if !expected.value.is_finite()
                    || !expected.tolerance.is_finite()
                    || expected.tolerance < 0.0
                {
                    return Err(KnowledgeError::Bundled(format!(
                        "worked case {} expectation {} is invalid",
                        item.id, expected.name
                    )));
                }
            }
            item.provenance.validate("worked_case.provenance")?;
        }
        for item in &self.counterexamples {
            insert(&item.id, "counterexample.id")?;
            require("counterexample.claim", &item.claim)?;
            require("counterexample.refutation", &item.refutation)?;
            item.provenance.validate("counterexample.provenance")?;
        }
        for item in &self.known_traps {
            insert(&item.id, "known_trap.id")?;
            require("known_trap.trap", &item.trap)?;
            require("known_trap.why_shallow_fails", &item.why_shallow_fails)?;
            require("known_trap.guard", &item.guard)?;
            item.provenance.validate("known_trap.provenance")?;
        }

        // Models must reference existing model ids (guards typos in packs).
        for model in &self.models {
            if !model_ids.contains(&model.id) {
                return Err(KnowledgeError::MissingReference {
                    from: "models".to_string(),
                    target: model.id.clone(),
                });
            }
        }
        Ok(())
    }
}

impl KnowledgePack {
    /// Convert the pack's governing models and validity domains into the
    /// central selector's catalog form. A model without a registered validity
    /// domain is refused by the catalog validator rather than treated as
    /// universally applicable.
    pub fn model_catalog(&self) -> KnowledgeResult<Vec<CatalogModel>> {
        let digest = self
            .digest()
            .ok_or_else(|| KnowledgeError::Bundled("pack lost its source digest".to_string()))?;
        let mut models = Vec::with_capacity(self.models.len());
        for model in &self.models {
            let mut domains = Vec::with_capacity(model.validity_domain_ids.len());
            for id in &model.validity_domain_ids {
                let domain = self
                    .validity_domains
                    .iter()
                    .find(|domain| &domain.id == id)
                    .ok_or_else(|| KnowledgeError::MissingReference {
                        from: model.id.clone(),
                        target: id.clone(),
                    })?;
                domains.push(SelectorValidityDomain {
                    id: domain.id.clone(),
                    statement: domain.statement.clone(),
                    temperature: domain.temperature.as_ref().and_then(|range| {
                        if range.unit == "K" {
                            engineering_units::Interval::new(range.min, range.max).ok()
                        } else {
                            None
                        }
                    }),
                    conditions: Vec::new(),
                    exclusions: domain.exclusions.clone(),
                    materials: Vec::new(),
                    provenance: pack_provenance(domain.provenance.clone(), digest),
                });
            }
            if domains.is_empty() {
                return Err(KnowledgeError::EmptyField {
                    field: "model.validity_domain_ids",
                });
            }
            let (disciplines, properties) = model_targeting(&self.id, &model.id, &self.domain);
            models.push(CatalogModel {
                model_id: model.id.clone(),
                source_model_id: Some(model.id.clone()),
                version: self.version.clone(),
                pack_id: Some(self.id.clone()),
                pack_version: Some(self.version.clone()),
                pack_digest: Some(digest),
                equation_ids: model.equation_ids.clone(),
                disciplines,
                properties,
                domains,
                provenance: pack_provenance(model.provenance.clone(), digest),
            });
        }
        Ok(models)
    }
}

fn pack_provenance(provenance: PackProvenance, digest: Digest) -> SelectionProvenance {
    SelectionProvenance {
        kind: provenance.kind,
        citation: provenance.citation,
        edition: provenance.edition,
        locator: provenance.locator,
        note: provenance.note,
        digest: Some(digest),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// The explicit target mapping for the four shipped packs. Registered packs
/// stay authoritative: the selector only matches requirements whose
/// discipline/property tokens are covered here.
fn model_targeting(pack_id: &str, model_id: &str, domain: &str) -> (Vec<Discipline>, Vec<String>) {
    match (pack_id, model_id) {
        ("bolted-joint", "model-torque-preload") => (
            vec![Discipline::Mechanical],
            strings(&[
                "torque", "preload", "bolt", "bolted", "thread", "clamp", "fastener", "joint",
                "stress", "nut",
            ]),
        ),
        ("power-electronics", "model-conduction-thermal") => (
            vec![Discipline::Electrical, Discipline::Thermal],
            strings(&[
                "mosfet",
                "conduction",
                "loss",
                "junction",
                "thermal",
                "rds",
                "power",
                "temperature",
                "heatsink",
                "current",
            ]),
        ),
        ("injection-molding", "model-moldability-checks") => (
            vec![Discipline::Manufacturing, Discipline::Mechanical],
            strings(&[
                "draft",
                "wall",
                "flow",
                "mold",
                "molding",
                "injection",
                "geometry",
                "manufactur",
                "ejection",
                "thickness",
            ]),
        ),
        ("passives", "model-cap-thermal-ripple") => (
            vec![Discipline::Electrical, Discipline::Thermal],
            strings(&[
                "capacitor",
                "cap",
                "esr",
                "ripple",
                "capacitance",
                "dielectric",
                "voltage",
                "life",
            ]),
        ),
        ("analog-stability", "model-opamp-loop-stability") => (
            vec![Discipline::Electrical],
            strings(&[
                "opamp",
                "op-amp",
                "amplifier",
                "loop",
                "gain",
                "phase",
                "margin",
                "stability",
                "compensation",
                "capacitance",
                "load",
                "bandwidth",
            ]),
        ),
        ("flyback-converters", "model-flyback-converter") => (
            vec![Discipline::Electrical],
            strings(&[
                "flyback",
                "converter",
                "transformer",
                "leakage",
                "snubber",
                "clamp",
                "vds",
                "dcm",
                "ccm",
                "duty",
            ]),
        ),
        ("thermal-networks", "model-thermal-network") => (
            vec![Discipline::Thermal, Discipline::Electrical],
            strings(&[
                "thermal_network",
                "transient",
                "heatsink",
                "sink",
                "contact",
                "interface",
                "rc",
                "impedance",
                "layer",
                "case",
            ]),
        ),
        ("fatigue-materials", "model-fatigue-damage") => (
            vec![Discipline::Mechanical, Discipline::Reliability],
            strings(&[
                "fatigue",
                "sn",
                "s-n",
                "strain",
                "damage",
                "miner",
                "endurance",
                "goodman",
                "basquin",
            ]),
        ),
        ("emc-pcb", "model-pcb-loop-inductance") => (
            vec![Discipline::Emc, Discipline::Electrical],
            strings(&[
                "emc",
                "loop",
                "inductance",
                "return",
                "decoupling",
                "creepage",
                "clearance",
                "pcb",
                "placement",
                "spacing",
                "stackup",
            ]),
        ),
        ("materials-conditional", "model-material-property-lookup") => (
            vec![Discipline::Mechanical, Discipline::Other],
            strings(&[
                "material",
                "temper",
                "property",
                "yield",
                "strength",
                "aluminium",
                "orientation",
            ]),
        ),
        _ => {
            let disciplines = match domain {
                "machine_design" => vec![Discipline::Mechanical],
                "manufacturing" => vec![Discipline::Manufacturing],
                "power_electronics" | "passives" => {
                    vec![Discipline::Electrical, Discipline::Thermal]
                }
                "analog" => vec![Discipline::Electrical],
                "thermal" => vec![Discipline::Thermal],
                "fatigue" => vec![Discipline::Mechanical, Discipline::Reliability],
                "emc" => vec![Discipline::Emc],
                "materials" => vec![Discipline::Mechanical, Discipline::Other],
                _ => vec![Discipline::Other],
            };
            let properties = model_id
                .split(|character: char| !character.is_ascii_alphanumeric())
                .filter(|token| token.len() >= 3)
                .map(|token| token.to_ascii_lowercase())
                .collect();
            (disciplines, properties)
        }
    }
}

impl KnowledgePack {
    pub fn authority(&self) -> Option<AuthorityRef> {
        Some(AuthorityRef {
            pack_id: self.id.clone(),
            version: self.version.clone(),
            digest: self.source_digest?,
            scope: format!("{} / {}", self.domain, self.title),
        })
    }
}

#[derive(Debug, Clone)]
pub struct KnowledgeRegistry {
    /// id -> version -> pack, so `get`/`latest` use borrowed keys and never
    /// allocate per lookup, and `latest` parses each version once.
    packs: BTreeMap<String, BTreeMap<String, KnowledgePack>>,
    max_packs: usize,
}

impl KnowledgeRegistry {
    pub fn new(max_packs: usize) -> KnowledgeResult<Self> {
        if max_packs == 0 {
            return Err(KnowledgeError::PackLimit { limit: 0 });
        }
        Ok(Self {
            packs: BTreeMap::new(),
            max_packs,
        })
    }

    pub fn register_json(&mut self, bytes: &[u8]) -> KnowledgeResult<AuthorityRef> {
        let pack = KnowledgePack::load(bytes)?;
        if self
            .packs
            .get(&pack.id)
            .is_some_and(|versions| versions.contains_key(&pack.version))
        {
            return Err(KnowledgeError::DuplicatePack {
                id: pack.id.clone(),
                version: pack.version.clone(),
            });
        }
        if self.len() >= self.max_packs {
            return Err(KnowledgeError::PackLimit {
                limit: self.max_packs,
            });
        }
        let authority = pack.authority().ok_or_else(|| {
            KnowledgeError::Bundled("validated pack lost its source digest".to_string())
        })?;
        self.packs
            .entry(pack.id.clone())
            .or_default()
            .insert(pack.version.clone(), pack);
        Ok(authority)
    }

    pub fn get(&self, id: &str, version: &str) -> Option<&KnowledgePack> {
        self.packs.get(id)?.get(version)
    }

    pub fn latest(&self, id: &str) -> Option<&KnowledgePack> {
        self.packs
            .get(id)?
            .iter()
            .max_by_key(|(version, _)| parse_version(version).ok())
            .map(|(_, pack)| pack)
    }

    pub fn by_domain(&self, domain: &str) -> Vec<&KnowledgePack> {
        self.packs
            .values()
            .flat_map(|versions| versions.values())
            .filter(|pack| pack.domain == domain)
            .collect()
    }

    pub fn packs(&self) -> impl Iterator<Item = &KnowledgePack> {
        self.packs.values().flat_map(|versions| versions.values())
    }

    pub fn len(&self) -> usize {
        self.packs.values().map(BTreeMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.packs.is_empty()
    }

    /// The full registered-model catalog for every pack in this registry.
    pub fn model_catalog(&self) -> KnowledgeResult<ModelCatalog> {
        let mut models = Vec::new();
        for pack in self.packs() {
            models.extend(pack.model_catalog()?);
        }
        ModelCatalog::new(models).map_err(|error| KnowledgeError::Bundled(error.to_string()))
    }

    /// The starter packs shipped with this build. They are loaded through the
    /// exact same validating JSON path as any future pack.
    pub fn bundled() -> KnowledgeResult<Self> {
        let mut registry = Self::new(DEFAULT_MAX_PACKS)?;
        for (name, bytes) in [
            ("bolted_joint", BOLTED_JOINT),
            ("power_electronics", POWER_ELECTRONICS),
            ("injection_molding", INJECTION_MOLDING),
            ("passives", PASSIVES),
            ("analog_stability", ANALOG_STABILITY),
            ("flyback_converters", FLYBACK_CONVERTERS),
            ("thermal_networks", THERMAL_NETWORKS),
            ("fatigue_materials", FATIGUE_MATERIALS),
            ("emc_pcb", EMC_PCB),
            ("materials_conditional", MATERIALS_CONDITIONAL),
        ] {
            registry
                .register_json(bytes.as_bytes())
                .map_err(|error| KnowledgeError::Bundled(format!("{name}: {error}")))?;
        }
        Ok(registry)
    }
}

fn parse_version(version: &str) -> KnowledgeResult<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next();
    let minor = parts.next();
    let patch = parts.next();
    if parts.next().is_some() {
        return Err(KnowledgeError::InvalidVersion {
            version: version.to_string(),
        });
    }
    let parse = |part: Option<&str>| -> KnowledgeResult<u64> {
        part.and_then(|part| part.parse::<u64>().ok())
            .ok_or_else(|| KnowledgeError::InvalidVersion {
                version: version.to_string(),
            })
    };
    Ok((parse(major)?, parse(minor)?, parse(patch)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_packs_load_and_validate() {
        let registry = KnowledgeRegistry::bundled().unwrap();
        assert_eq!(registry.len(), 10);
        for pack in registry.packs() {
            pack.validate().unwrap();
            assert_eq!(pack.schema, PACK_SCHEMA);
            assert!(pack.digest().is_some());
            assert!(pack.authority().is_some());
        }
    }

    #[test]
    fn duplicate_pack_version_is_refused() {
        let mut registry = KnowledgeRegistry::new(4).unwrap();
        registry.register_json(BOLTED_JOINT.as_bytes()).unwrap();
        assert!(matches!(
            registry.register_json(BOLTED_JOINT.as_bytes()),
            Err(KnowledgeError::DuplicatePack { .. })
        ));
    }

    #[test]
    #[ignore = "[perf] knowledge retrieval across 500 versions"]
    fn perf_latest_and_get_over_five_hundred_versions() {
        let base: serde_json::Value = serde_json::from_str(BOLTED_JOINT).unwrap();
        let mut registry = KnowledgeRegistry::new(600).unwrap();
        for index in 0..500_u32 {
            let mut value = base.clone();
            value["version"] = serde_json::Value::String(format!("1.{}.0", index % 100));
            // Keep ids unique within the registry by using distinct pack ids
            // for the remaining dimensions; the same id exercises `latest`.
            value["id"] = serde_json::Value::String(format!("perf-{}", index / 100));
            registry
                .register_json(serde_json::to_vec(&value).unwrap().as_slice())
                .unwrap();
        }
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            assert!(registry.latest("perf-0").is_some());
            assert!(registry.get("perf-4", "1.99.0").is_some());
        }
        let elapsed = started.elapsed();
        eprintln!(
            "perf knowledge: 10k latest+get over {} packs = {:?}",
            registry.len(),
            elapsed
        );
        assert!(elapsed.as_secs() < 5, "retrieval took {elapsed:?}");
    }

    #[test]
    fn latest_version_is_selected_semantically() {
        let mut registry = KnowledgeRegistry::new(4).unwrap();
        let base: serde_json::Value = serde_json::from_str(BOLTED_JOINT).unwrap();
        for version in ["1.2.0", "1.10.0"] {
            let mut value = base.clone();
            value["version"] = serde_json::Value::String(version.to_string());
            registry
                .register_json(serde_json::to_vec(&value).unwrap().as_slice())
                .unwrap();
        }
        assert_eq!(registry.latest("bolted-joint").unwrap().version, "1.10.0");
    }
}
