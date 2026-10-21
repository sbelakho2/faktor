//! Authoritative model-applicability selection.
//!
//! Model choice is not a caller preference: given a requirement, the
//! established conditions, the material and the environment (temperature),
//! [`ModelSelector`] evaluates every registered model's explicit validity
//! domains and returns exactly one of:
//!
//! * a selected model with domain ids, rationale and provenance;
//! * a typed research request when no registered model covers the regime or
//!   every candidate is outside its domain;
//! * a typed ambiguity when more than one registered model applies.
//!
//! Gates 3 (theory selection) and 4 (validity envelope) recompute the
//! selection from the recorded catalog/request and refuse any theory or
//! envelope that does not match it, so a caller cannot silently substitute a
//! preferred model or widen a registered validity domain.

use std::collections::BTreeSet;

use engineering_units::{Interval, Quantity, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::{require_text, CoreError, CoreResult};
use crate::gate::ValidityEnvelope;
use crate::requirements::Discipline;

pub const MAX_CATALOG_MODELS: usize = 128;
pub const MAX_VALIDITY_DOMAINS: usize = 16;
pub const MAX_SELECTION_REQUESTS: usize = 128;
pub const MAX_SELECTION_TEXT: usize = 2_048;

/// Provenance of a registered model or validity domain. Knowledge packs use
/// free-form provenance kinds ("reference", "standard", ...), preserved here
/// verbatim rather than coerced into an enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionProvenance {
    pub kind: String,
    pub citation: String,
    pub edition: Option<String>,
    pub locator: Option<String>,
    pub note: Option<String>,
    pub digest: Option<Digest>,
}

impl SelectionProvenance {
    pub fn new(kind: impl Into<String>, citation: impl Into<String>) -> CoreResult<Self> {
        let value = Self {
            kind: kind.into(),
            citation: citation.into(),
            edition: None,
            locator: None,
            note: None,
            digest: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> CoreResult<()> {
        require_text("selection.provenance.kind", &self.kind, 128)?;
        require_text("selection.provenance.citation", &self.citation, 2_048)?;
        if let Some(edition) = &self.edition {
            require_text("selection.provenance.edition", edition, 128)?;
        }
        if let Some(locator) = &self.locator {
            require_text("selection.provenance.locator", locator, 512)?;
        }
        if let Some(note) = &self.note {
            require_text("selection.provenance.note", note, 2_048)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidityDomain {
    pub id: String,
    pub statement: String,
    /// Inclusive temperature envelope in kelvin, when the domain binds one.
    pub temperature: Option<Interval>,
    /// Conditions that must be established for the domain to apply.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Conditions that invalidate the domain.
    #[serde(default)]
    pub exclusions: Vec<String>,
    /// Material names/grades the domain is restricted to; empty means the
    /// registered domain does not restrict material (not "any material").
    #[serde(default)]
    pub materials: Vec<String>,
    pub provenance: SelectionProvenance,
}

impl ValidityDomain {
    fn validate(&self) -> CoreResult<()> {
        require_text("validity_domain.id", &self.id, 256)?;
        require_text(
            "validity_domain.statement",
            &self.statement,
            MAX_SELECTION_TEXT,
        )?;
        if self.conditions.len() > 64 || self.exclusions.len() > 64 || self.materials.len() > 64 {
            return Err(CoreError::InvalidSelection {
                reason: format!("validity domain {} exceeds the condition bound", self.id),
            });
        }
        for item in self
            .conditions
            .iter()
            .chain(self.exclusions.iter())
            .chain(self.materials.iter())
        {
            require_text("validity_domain.condition", item, 1_024)?;
        }
        self.provenance.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    /// Model identity used in a theory selection (an alias of the registered
    /// pack model is allowed; `source_model_id` preserves the pack id).
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model_id: Option<String>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_digest: Option<Digest>,
    pub equation_ids: Vec<String>,
    /// Disciplines the model governs. Empty means the registration did not
    /// establish a discipline and the model is not selectable.
    pub disciplines: Vec<Discipline>,
    /// Property tokens the model governs (compared with the requirement's
    /// property name, e.g. `junction_temperature` -> `junction`).
    pub properties: Vec<String>,
    pub domains: Vec<ValidityDomain>,
    pub provenance: SelectionProvenance,
}

impl CatalogModel {
    fn validate(&self) -> CoreResult<()> {
        require_text("catalog.model_id", &self.model_id, 256)?;
        require_text("catalog.model_version", &self.version, 128)?;
        if let Some(source) = &self.source_model_id {
            require_text("catalog.source_model_id", source, 256)?;
        }
        if self.pack_id.is_some() != self.pack_version.is_some()
            || self.pack_id.is_some() != self.pack_digest.is_some()
        {
            return Err(CoreError::InvalidSelection {
                reason: "pack id, version and digest must be recorded together".to_string(),
            });
        }
        if self.equation_ids.is_empty() || self.equation_ids.len() > 128 {
            return Err(CoreError::InvalidSelection {
                reason: format!("model {} must cite 1..=128 equations", self.model_id),
            });
        }
        for equation in &self.equation_ids {
            require_text("catalog.equation_id", equation, 256)?;
        }
        if self.disciplines.is_empty() {
            return Err(CoreError::InvalidSelection {
                reason: format!("model {} has no governing discipline", self.model_id),
            });
        }
        if self.properties.len() > 64 {
            return Err(CoreError::InvalidSelection {
                reason: format!("model {} exceeds the property-token bound", self.model_id),
            });
        }
        for property in &self.properties {
            require_text("catalog.property", property, 128)?;
        }
        if self.domains.is_empty() || self.domains.len() > MAX_VALIDITY_DOMAINS {
            return Err(CoreError::InvalidSelection {
                reason: format!(
                    "model {} must record 1..={MAX_VALIDITY_DOMAINS} validity domains",
                    self.model_id
                ),
            });
        }
        let mut ids = BTreeSet::new();
        for domain in &self.domains {
            domain.validate()?;
            if !ids.insert(domain.id.clone()) {
                return Err(CoreError::InvalidSelection {
                    reason: format!("model {} has duplicate domain {}", self.model_id, domain.id),
                });
            }
        }
        self.provenance.validate()
    }

    fn governs(&self, request: &SelectionRequest) -> bool {
        if !self.disciplines.contains(&request.discipline) {
            return false;
        }
        let Some(property) = &request.property else {
            return true;
        };
        let wanted = tokens(property);
        self.properties
            .iter()
            .map(|property| property.to_ascii_lowercase())
            .any(|property| wanted.contains(&property) || wanted.iter().any(|t| t == &property))
    }

    fn evaluate(&self, request: &SelectionRequest) -> Result<(), Vec<String>> {
        let mut failures = Vec::new();
        for domain in &self.domains {
            if let Some(bounds) = domain.temperature {
                match &request.temperature {
                    None => failures.push(format!(
                        "domain {} is temperature-bound but the environment temperature is unavailable",
                        domain.id
                    )),
                    Some(temperature) => match kelvin(temperature) {
                        Err(reason) => failures.push(format!("domain {}: {reason}", domain.id)),
                        Ok(kelvin) if !bounds.contains(kelvin) => failures.push(format!(
                            "domain {} admits {}..{} K but the environment is {kelvin} K",
                            domain.id,
                            bounds.lower(),
                            bounds.upper()
                        )),
                        Ok(_) => {}
                    },
                }
            }
            let established: BTreeSet<String> = request
                .conditions
                .iter()
                .map(|condition| normalize(condition))
                .collect();
            for condition in &domain.conditions {
                if !established.contains(&normalize(condition)) {
                    failures.push(format!(
                        "domain {} requires the established condition {condition:?}",
                        domain.id
                    ));
                }
            }
            for exclusion in &domain.exclusions {
                let exclusion = normalize(exclusion);
                let present = request.conditions.iter().any(|condition| {
                    let condition = normalize(condition);
                    condition == exclusion
                        || condition.contains(&exclusion)
                        || exclusion.contains(&condition)
                }) || request
                    .material
                    .as_ref()
                    .is_some_and(|material| normalize(material).contains(&exclusion));
                if present {
                    failures.push(format!(
                        "domain {} is excluded by the present condition {:?}",
                        domain.id, exclusion
                    ));
                }
            }
            if !domain.materials.is_empty() {
                let Some(material) = &request.material else {
                    failures.push(format!(
                        "domain {} is material-bound but no material is recorded",
                        domain.id
                    ));
                    continue;
                };
                let material = normalize(material);
                if !domain
                    .materials
                    .iter()
                    .any(|allowed| normalize(allowed) == material)
                {
                    failures.push(format!(
                        "domain {} does not cover material {material:?}",
                        domain.id
                    ));
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }

    fn rationale(&self, request: &SelectionRequest) -> String {
        let domains = self
            .domains
            .iter()
            .map(|domain| domain.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let temperature = request
            .temperature
            .as_ref()
            .and_then(|temperature| kelvin(temperature).ok())
            .map(|value| format!("{value} K"))
            .unwrap_or_else(|| "not bound".to_string());
        format!(
            "{}@{} selected for requirement {} by discipline {} and property {}; domains [{}] satisfied with temperature {} and {} established condition(s)",
            self.model_id,
            self.version,
            request.requirement_id,
            request.discipline.as_str(),
            request.property.as_deref().unwrap_or("<discipline only>"),
            domains,
            temperature,
            request.conditions.len()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub models: Vec<CatalogModel>,
}

impl ModelCatalog {
    pub fn new(models: Vec<CatalogModel>) -> CoreResult<Self> {
        let catalog = Self { models };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> CoreResult<()> {
        if self.models.is_empty() || self.models.len() > MAX_CATALOG_MODELS {
            return Err(CoreError::InvalidSelection {
                reason: format!("catalog must contain 1..={MAX_CATALOG_MODELS} models"),
            });
        }
        let mut ids = BTreeSet::new();
        for model in &self.models {
            model.validate()?;
            if !ids.insert(model.model_id.clone()) {
                return Err(CoreError::InvalidSelection {
                    reason: format!("duplicate catalog model {}", model.model_id),
                });
            }
        }
        Ok(())
    }

    pub fn get(&self, model_id: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|model| model.model_id == model_id)
    }

    pub fn digest(&self) -> CoreResult<Digest> {
        Digest::of_json(self)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    pub requirement_id: String,
    pub discipline: Discipline,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    /// Conditions established by the candidate/requirement at decision time.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Material identity, when the requirement is material-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    /// Environment temperature, when established.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Quantity>,
}

impl SelectionRequest {
    pub fn validate(&self) -> CoreResult<()> {
        require_text("selection.requirement_id", &self.requirement_id, 256)?;
        if let Some(property) = &self.property {
            require_text("selection.property", property, 256)?;
        }
        if self.conditions.len() > 128 {
            return Err(CoreError::InvalidSelection {
                reason: format!(
                    "requirement {} records more than 128 conditions",
                    self.requirement_id
                ),
            });
        }
        for condition in &self.conditions {
            require_text("selection.condition", condition, 1_024)?;
        }
        if let Some(material) = &self.material {
            require_text("selection.material", material, 512)?;
        }
        if let Some(temperature) = &self.temperature {
            if temperature.dimension() != engineering_units::Dimension::TEMPERATURE {
                return Err(CoreError::InvalidSelection {
                    reason: "selection temperature must have the temperature dimension".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedModel {
    pub requirement_id: String,
    pub model_id: String,
    pub source_model_id: Option<String>,
    pub version: String,
    pub pack_id: Option<String>,
    pub pack_version: Option<String>,
    pub pack_digest: Option<Digest>,
    pub equation_ids: Vec<String>,
    pub validity_domain_ids: Vec<String>,
    /// The exact registered domains that admitted the model. Gates 3/4
    /// recompute from these; they are not caller-editable inputs.
    pub validity: Vec<ValidityDomain>,
    pub rationale: String,
    pub provenance: SelectionProvenance,
}

impl SelectedModel {
    fn validate(&self) -> CoreResult<()> {
        require_text("selected.requirement_id", &self.requirement_id, 256)?;
        require_text("selected.model_id", &self.model_id, 256)?;
        require_text("selected.rationale", &self.rationale, 4_096)?;
        if self.equation_ids.is_empty() || self.validity.is_empty() {
            return Err(CoreError::InvalidSelection {
                reason: format!(
                    "selected model {} must record equations and validity domains",
                    self.model_id
                ),
            });
        }
        for domain in &self.validity {
            domain.validate()?;
        }
        self.provenance.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequirementSelection {
    Selected {
        requirement_id: String,
        models: Vec<SelectedModel>,
        rationale: String,
        alternatives_considered: Vec<String>,
    },
    ResearchRequired {
        requirement_id: String,
        reason: String,
        sources_required: Vec<String>,
    },
    Ambiguous {
        requirement_id: String,
        candidates: Vec<String>,
        reason: String,
    },
}

impl RequirementSelection {
    pub fn is_selected(&self) -> bool {
        matches!(self, Self::Selected { .. })
    }

    pub fn requirement_id(&self) -> &str {
        match self {
            Self::Selected { requirement_id, .. }
            | Self::ResearchRequired { requirement_id, .. }
            | Self::Ambiguous { requirement_id, .. } => requirement_id,
        }
    }
}

/// The recorded applicability decision. `validate` recomputes the selection
/// from the recorded catalog and requests; the response gates call it so a
/// theory or envelope that disagrees with the selector is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilitySelection {
    pub catalog: ModelCatalog,
    pub requests: Vec<SelectionRequest>,
    pub selections: Vec<RequirementSelection>,
}

impl ApplicabilitySelection {
    pub fn validate(&self) -> CoreResult<()> {
        self.catalog.validate()?;
        if self.requests.is_empty() || self.requests.len() > MAX_SELECTION_REQUESTS {
            return Err(CoreError::InvalidSelection {
                reason: format!("selection records 1..={MAX_SELECTION_REQUESTS} requests"),
            });
        }
        for request in &self.requests {
            request.validate()?;
        }
        if self.selections.len() != self.requests.len() {
            return Err(CoreError::InvalidSelection {
                reason: "each selection request needs exactly one recorded outcome".to_string(),
            });
        }
        for selection in &self.selections {
            if let RequirementSelection::Selected { models, .. } = selection {
                for model in models {
                    model.validate()?;
                }
            }
        }
        let recomputed = ModelSelector::select(&self.catalog, &self.requests)?;
        if recomputed.selections != self.selections {
            return Err(CoreError::InvalidSelection {
                reason: "recorded applicability outcomes do not match the selector's result"
                    .to_string(),
            });
        }
        Ok(())
    }

    pub fn is_all_selected(&self) -> bool {
        self.selections
            .iter()
            .all(RequirementSelection::is_selected)
    }

    pub fn selected_models(&self) -> impl Iterator<Item = &SelectedModel> {
        self.selections
            .iter()
            .flat_map(|selection| match selection {
                RequirementSelection::Selected { models, .. } => models.iter(),
                _ => [].iter(),
            })
    }

    /// Unique selected models in first-selection order (one model can govern
    /// more than one requirement).
    pub fn unique_models(&self) -> Vec<&SelectedModel> {
        let mut seen = BTreeSet::new();
        self.selected_models()
            .filter(|model| seen.insert(model.model_id.clone()))
            .collect()
    }

    /// Validity envelopes for the selected models. Gate 4 must equal this
    /// list exactly; a caller cannot widen or replace a registered domain.
    pub fn envelopes(&self) -> CoreResult<Vec<ValidityEnvelope>> {
        let mut envelopes = Vec::new();
        for model in self.unique_models() {
            let mut temperature: Option<Interval> = None;
            let mut conditions = BTreeSet::new();
            let mut exclusions = BTreeSet::new();
            for domain in &model.validity {
                if let Some(bounds) = domain.temperature {
                    temperature = Some(match temperature {
                        None => bounds,
                        Some(existing) if existing == bounds => existing,
                        Some(_) => {
                            return Err(CoreError::InvalidSelection {
                                reason: format!(
                                    "model {} has multiple distinct temperature domains that cannot be represented by one envelope",
                                    model.model_id
                                ),
                            })
                        }
                    });
                }
                conditions.extend(domain.conditions.iter().cloned());
                exclusions.extend(domain.exclusions.iter().cloned());
            }
            envelopes.push(ValidityEnvelope {
                model_id: model.model_id.clone(),
                temperature,
                conditions: conditions.into_iter().collect(),
                exclusions: exclusions.into_iter().collect(),
            });
        }
        Ok(envelopes)
    }

    pub fn digest(&self) -> CoreResult<Digest> {
        Digest::of_json(self)
    }
}

/// The central selector. Stateless and deterministic: the same catalog and
/// requests always produce byte-identical outcomes.
pub struct ModelSelector;

impl ModelSelector {
    pub fn select(
        catalog: &ModelCatalog,
        requests: &[SelectionRequest],
    ) -> CoreResult<ApplicabilitySelection> {
        catalog.validate()?;
        if requests.is_empty() || requests.len() > MAX_SELECTION_REQUESTS {
            return Err(CoreError::InvalidSelection {
                reason: format!("selection needs 1..={MAX_SELECTION_REQUESTS} requests"),
            });
        }
        let mut selections = Vec::with_capacity(requests.len());
        for request in requests {
            request.validate()?;
            selections.push(Self::select_one(catalog, request));
        }
        Ok(ApplicabilitySelection {
            catalog: catalog.clone(),
            requests: requests.to_vec(),
            selections,
        })
    }

    fn select_one(catalog: &ModelCatalog, request: &SelectionRequest) -> RequirementSelection {
        let candidates: Vec<&CatalogModel> = catalog
            .models
            .iter()
            .filter(|model| model.governs(request))
            .collect();
        if candidates.is_empty() {
            return RequirementSelection::ResearchRequired {
                requirement_id: request.requirement_id.clone(),
                reason: format!(
                    "no registered model governs discipline {} and property {}",
                    request.discipline.as_str(),
                    request.property.as_deref().unwrap_or("<none>")
                ),
                sources_required: vec![format!(
                    "a validated {} model covering {}",
                    request.discipline.as_str(),
                    request.property.as_deref().unwrap_or("this regime")
                )],
            };
        }
        let mut applicable: Vec<(&CatalogModel, String)> = Vec::new();
        let mut rejected = Vec::new();
        for candidate in &candidates {
            match candidate.evaluate(request) {
                Ok(()) => applicable.push((candidate, candidate.rationale(request))),
                Err(failures) => {
                    rejected.push(format!("{}: {}", candidate.model_id, failures.join("; ")));
                }
            }
        }
        let mut alternatives = rejected
            .iter()
            .map(|reason| format!("rejected candidate {reason}"))
            .collect::<Vec<_>>();
        for model in &catalog.models {
            if !candidates
                .iter()
                .any(|candidate| candidate.model_id == model.model_id)
            {
                alternatives.push(format!(
                    "{} does not govern discipline {} / property {}",
                    model.model_id,
                    request.discipline.as_str(),
                    request.property.as_deref().unwrap_or("<none>")
                ));
            }
        }
        match applicable.len() {
            0 => RequirementSelection::ResearchRequired {
                requirement_id: request.requirement_id.clone(),
                reason: format!(
                    "every registered model for discipline {} and property {} is outside its validity domain: {}",
                    request.discipline.as_str(),
                    request.property.as_deref().unwrap_or("<none>"),
                    rejected.join("; ")
                ),
                sources_required: candidates
                    .iter()
                    .flat_map(|model| model.domains.iter())
                    .map(|domain| domain.statement.clone())
                    .collect(),
            },
            1 => {
                let (model, rationale) = &applicable[0];
                RequirementSelection::Selected {
                    requirement_id: request.requirement_id.clone(),
                    models: vec![SelectedModel {
                        requirement_id: request.requirement_id.clone(),
                        model_id: model.model_id.clone(),
                        source_model_id: model.source_model_id.clone(),
                        version: model.version.clone(),
                        pack_id: model.pack_id.clone(),
                        pack_version: model.pack_version.clone(),
                        pack_digest: model.pack_digest,
                        equation_ids: model.equation_ids.clone(),
                        validity_domain_ids: model
                            .domains
                            .iter()
                            .map(|domain| domain.id.clone())
                            .collect(),
                        validity: model.domains.clone(),
                        rationale: rationale.clone(),
                        provenance: model.provenance.clone(),
                    }],
                    rationale: rationale.clone(),
                    alternatives_considered: if alternatives.is_empty() {
                        vec!["no other registered model covers this discipline/property".to_string()]
                    } else {
                        alternatives
                    },
                }
            }
            _ => RequirementSelection::Ambiguous {
                requirement_id: request.requirement_id.clone(),
                candidates: applicable
                    .iter()
                    .map(|(model, _)| format!("{}@{}", model.model_id, model.version))
                    .collect(),
                reason: format!(
                    "{} registered models satisfy the same validity domains for requirement {}; refusing to choose by preference",
                    applicable.len(),
                    request.requirement_id
                ),
            },
        }
    }
}

fn kelvin(quantity: &Quantity) -> Result<f64, String> {
    let unit = UnitRegistry::si()
        .require("K")
        .map_err(|error| error.to_string())?;
    quantity
        .unit()
        .convert_to(quantity.value(), &unit)
        .map_err(|error| error.to_string())
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}
