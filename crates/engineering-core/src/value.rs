use std::fmt;

use engineering_units::{Dimension, Interval, Quantity, Uncertainty, Unit, UnitRegistry};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{require_text, CoreError, CoreResult};

/// Monotonic revision identity. Zero is not a revision.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
#[schemars(inline)]
pub struct RevisionId(#[schemars(range(min = 1))] u64);

impl RevisionId {
    pub const fn new(raw: u64) -> Self {
        assert!(raw != 0, "revision ids start at 1");
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rev-{}", self.0)
    }
}

#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceKind {
    Specification,
    Measurement,
    Test,
    Simulation,
    Calculation,
    Standard,
    ManufacturerData,
    EngineeringPractice,
    Assumption,
}

impl ProvenanceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Specification => "specification",
            Self::Measurement => "measurement",
            Self::Test => "test",
            Self::Simulation => "simulation",
            Self::Calculation => "calculation",
            Self::Standard => "standard",
            Self::ManufacturerData => "manufacturer_data",
            Self::EngineeringPractice => "engineering_practice",
            Self::Assumption => "assumption",
        }
    }
}

/// The source of one fact. A citation is mandatory; a digest and revision are
/// mandatory only where the fact is revision-bound (checked by the consumers
/// that need one).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub kind: ProvenanceKind,
    pub citation: String,
    pub locator: Option<String>,
    pub digest: Option<crate::digest::Digest>,
    pub revision: Option<RevisionId>,
}

impl Provenance {
    pub fn new(kind: ProvenanceKind, citation: impl Into<String>) -> CoreResult<Self> {
        let value = Self {
            kind,
            citation: citation.into(),
            locator: None,
            digest: None,
            revision: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn with_locator(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }

    pub fn with_revision(mut self, revision: RevisionId) -> Self {
        self.revision = Some(revision);
        self
    }

    pub fn with_digest(mut self, digest: crate::digest::Digest) -> Self {
        self.digest = Some(digest);
        self
    }

    pub fn validate(&self) -> CoreResult<()> {
        require_text("provenance.citation", &self.citation, 2_048)?;
        if let Some(locator) = &self.locator {
            require_text("provenance.locator", locator, 512)?;
        }
        Ok(())
    }
}

/// Explicit conditions under which a value is claimed to hold. An empty
/// temperature bound means "not established by the source", never "any
/// temperature works".
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    /// Inclusive bounds in kelvin.
    pub temperature: Option<Interval>,
    pub conditions: Vec<String>,
    pub exclusions: Vec<String>,
    pub load_regime: Option<String>,
}

impl Applicability {
    pub fn unrestricted() -> Self {
        Self::default()
    }

    pub fn with_temperature(mut self, kelvin: Interval) -> Self {
        self.temperature = Some(kelvin);
        self
    }

    pub fn with_condition(mut self, condition: impl Into<String>) -> Self {
        self.conditions.push(condition.into());
        self
    }

    pub fn with_exclusion(mut self, exclusion: impl Into<String>) -> Self {
        self.exclusions.push(exclusion.into());
        self
    }

    pub fn with_load_regime(mut self, regime: impl Into<String>) -> Self {
        self.load_regime = Some(regime.into());
        self
    }

    pub fn contains_temperature(&self, temperature: &Quantity) -> CoreResult<bool> {
        if temperature.dimension() != Dimension::TEMPERATURE {
            return Err(CoreError::InvalidValue {
                reason: format!(
                    "temperature applicability requires a temperature, got {}",
                    temperature.dimension()
                ),
            });
        }
        let Some(bounds) = self.temperature else {
            return Ok(true);
        };
        let kelvin = temperature
            .unit()
            .convert_to(temperature.value(), &UnitRegistry::si().require("K")?)?;
        Ok(bounds.contains(kelvin))
    }

    pub fn validate(&self) -> CoreResult<()> {
        for condition in self.conditions.iter().chain(self.exclusions.iter()) {
            require_text("applicability.condition", condition, 1_024)?;
        }
        if let Some(regime) = &self.load_regime {
            require_text("applicability.load_regime", regime, 512)?;
        }
        Ok(())
    }

    pub fn is_unrestricted(&self) -> bool {
        self.temperature.is_none()
            && self.conditions.is_empty()
            && self.exclusions.is_empty()
            && self.load_regime.is_none()
    }
}

#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialRef {
    pub name: String,
    pub grade: Option<String>,
    pub temper: Option<String>,
    pub condition: Option<String>,
    pub revision: RevisionId,
    pub provenance: Provenance,
}

impl MaterialRef {
    pub fn new(
        name: impl Into<String>,
        revision: RevisionId,
        provenance: Provenance,
    ) -> CoreResult<Self> {
        let value = Self {
            name: name.into(),
            grade: None,
            temper: None,
            condition: None,
            revision,
            provenance,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn with_grade(mut self, grade: impl Into<String>) -> Self {
        self.grade = Some(grade.into());
        self
    }

    pub fn with_temper(mut self, temper: impl Into<String>) -> Self {
        self.temper = Some(temper.into());
        self
    }

    pub fn validate(&self) -> CoreResult<()> {
        require_text("material.name", &self.name, 256)?;
        self.provenance.validate()?;
        if let Some(grade) = &self.grade {
            require_text("material.grade", grade, 128)?;
        }
        if let Some(temper) = &self.temper {
            require_text("material.temper", temper, 128)?;
        }
        Ok(())
    }
}

#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Orientation {
    Longitudinal,
    Transverse,
    ThroughThickness,
    AngleDeg { degrees: f64 },
}

impl Orientation {
    pub fn validate(self) -> CoreResult<()> {
        if let Self::AngleDeg { degrees } = self {
            if !degrees.is_finite() {
                return Err(CoreError::InvalidValue {
                    reason: "orientation angle must be finite".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThicknessRange {
    pub min: Quantity,
    pub max: Quantity,
}

impl ThicknessRange {
    pub fn new(min: Quantity, max: Quantity) -> CoreResult<Self> {
        if min.dimension() != Dimension::LENGTH || max.dimension() != Dimension::LENGTH {
            return Err(CoreError::InvalidValue {
                reason: "thickness range requires two lengths".to_string(),
            });
        }
        let max_in_min = max.to_unit(min.unit().clone())?;
        if min.compare(&max_in_min)? == std::cmp::Ordering::Greater {
            return Err(CoreError::InvalidValue {
                reason: "thickness range minimum exceeds maximum".to_string(),
            });
        }
        Ok(Self { min, max })
    }
}

/// A typed unknown. The reason and the missing inputs travel with the value;
/// there is no zero-default anywhere in this type.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnavailableValue {
    pub reason: String,
    pub required: Vec<String>,
}

impl UnavailableValue {
    pub fn new(reason: impl Into<String>, required: Vec<String>) -> CoreResult<Self> {
        let value = Self {
            reason: reason.into(),
            required,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> CoreResult<()> {
        require_text("unavailable.reason", &self.reason, 2_048)?;
        for item in &self.required {
            require_text("unavailable.required", item, 512)?;
        }
        Ok(())
    }
}

#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueState<T> {
    Known(T),
    Unavailable(UnavailableValue),
}

impl<T> ValueState<T> {
    pub fn known(value: T) -> Self {
        Self::Known(value)
    }

    pub fn unavailable(reason: impl Into<String>, required: Vec<String>) -> CoreResult<Self> {
        Ok(Self::Unavailable(UnavailableValue::new(reason, required)?))
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known(_))
    }

    pub fn as_known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&UnavailableValue> {
        match self {
            Self::Known(_) => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }

    pub fn require_known(&self, field: &'static str) -> CoreResult<&T> {
        match self {
            Self::Known(value) => Ok(value),
            Self::Unavailable(reason) => Err(CoreError::InvalidValue {
                reason: format!("{field} is unavailable: {}", reason.reason),
            }),
        }
    }

    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> ValueState<U> {
        match self {
            Self::Known(value) => ValueState::Known(map(value)),
            Self::Unavailable(reason) => ValueState::Unavailable(reason),
        }
    }
}

/// A conditional engineering fact. Every field is part of the claim:
/// material, condition, temperature, orientation, thickness range,
/// tolerance/distribution, source, revision, applicability and uncertainty.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringValue {
    pub quantity: ValueState<Quantity>,
    pub material: Option<MaterialRef>,
    pub condition: Option<String>,
    pub temperature: Option<Quantity>,
    pub orientation: Option<Orientation>,
    pub thickness_range: Option<ThicknessRange>,
    pub tolerance: Option<Uncertainty>,
    pub source: Provenance,
    pub revision: RevisionId,
    pub applicability: Applicability,
    pub uncertainty: Uncertainty,
}

impl EngineeringValue {
    pub fn known(quantity: Quantity, source: Provenance, revision: RevisionId) -> CoreResult<Self> {
        let value = Self {
            quantity: ValueState::Known(quantity),
            material: None,
            condition: None,
            temperature: None,
            orientation: None,
            thickness_range: None,
            tolerance: None,
            source,
            revision,
            applicability: Applicability::unrestricted(),
            uncertainty: Uncertainty::Exact,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn unavailable(
        reason: impl Into<String>,
        required: Vec<String>,
        source: Provenance,
        revision: RevisionId,
    ) -> CoreResult<Self> {
        let value = Self {
            quantity: ValueState::unavailable(reason, required)?,
            material: None,
            condition: None,
            temperature: None,
            orientation: None,
            thickness_range: None,
            tolerance: None,
            source,
            revision,
            applicability: Applicability::unrestricted(),
            uncertainty: Uncertainty::Exact,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn with_material(mut self, material: MaterialRef) -> Self {
        self.material = Some(material);
        self
    }

    pub fn with_condition(mut self, condition: impl Into<String>) -> Self {
        self.condition = Some(condition.into());
        self
    }

    pub fn with_temperature(mut self, temperature: Quantity) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_orientation(mut self, orientation: Orientation) -> Self {
        self.orientation = Some(orientation);
        self
    }

    pub fn with_thickness_range(mut self, range: ThicknessRange) -> Self {
        self.thickness_range = Some(range);
        self
    }

    pub fn with_tolerance(mut self, tolerance: Uncertainty) -> Self {
        self.tolerance = Some(tolerance);
        self
    }

    pub fn with_uncertainty(mut self, uncertainty: Uncertainty) -> Self {
        self.uncertainty = uncertainty;
        self
    }

    pub fn with_applicability(mut self, applicability: Applicability) -> Self {
        self.applicability = applicability;
        self
    }

    pub fn validate(&self) -> CoreResult<()> {
        self.source.validate()?;
        self.applicability.validate()?;
        if let ValueState::Known(quantity) = &self.quantity {
            if !quantity.value().is_finite() {
                return Err(CoreError::InvalidValue {
                    reason: "quantity value is not finite".to_string(),
                });
            }
        } else if let ValueState::Unavailable(reason) = &self.quantity {
            reason.validate()?;
        }
        if let Some(material) = &self.material {
            material.validate()?;
        }
        if let Some(condition) = &self.condition {
            require_text("engineering_value.condition", condition, 1_024)?;
        }
        if let Some(temperature) = &self.temperature {
            if temperature.dimension() != Dimension::TEMPERATURE {
                return Err(CoreError::InvalidValue {
                    reason: "temperature field has a non-temperature dimension".to_string(),
                });
            }
        }
        if let Some(orientation) = self.orientation {
            orientation.validate()?;
        }
        if let Some(range) = &self.thickness_range {
            ThicknessRange::new(range.min.clone(), range.max.clone())?;
        }
        Ok(())
    }
}

/// Helper for callers that need a quantity in a named unit from the built-in
/// registry. Unknown symbols are typed errors, never implicit defaults.
pub fn quantity_in(value: f64, unit_symbol: &str) -> CoreResult<Quantity> {
    let unit: Unit = UnitRegistry::si().require(unit_symbol)?;
    Ok(Quantity::new(value, unit)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Provenance {
        Provenance::new(ProvenanceKind::Calculation, "unit test").unwrap()
    }

    #[test]
    fn unavailable_is_not_zero() {
        let value = EngineeringValue::unavailable(
            "proof strength not supplied",
            vec!["material.proof_strength".to_string()],
            source(),
            RevisionId::new(1),
        )
        .unwrap();
        assert!(!value.quantity.is_known());
        assert_eq!(
            value.quantity.unavailable_reason().unwrap().required,
            vec!["material.proof_strength"]
        );
    }

    #[test]
    fn thickness_minimum_above_maximum_is_refused() {
        let units = UnitRegistry::si();
        let min = Quantity::new(2.0, units.require("mm").unwrap()).unwrap();
        let max = Quantity::new(1.0, units.require("mm").unwrap()).unwrap();
        assert!(ThicknessRange::new(min, max).is_err());
    }

    #[test]
    fn temperature_applicability_converts_units() {
        let applicability =
            Applicability::unrestricted().with_temperature(Interval::new(273.15, 373.15).unwrap());
        let units = UnitRegistry::si();
        let at_100c = Quantity::new(100.0, units.require("degC").unwrap()).unwrap();
        assert!(applicability.contains_temperature(&at_100c).unwrap());
        let at_0c = Quantity::new(0.0, units.require("degC").unwrap()).unwrap();
        assert!(applicability.contains_temperature(&at_0c).unwrap());
        let at_minus_20c = Quantity::new(-20.0, units.require("degC").unwrap()).unwrap();
        assert!(!applicability.contains_temperature(&at_minus_20c).unwrap());
    }
}
