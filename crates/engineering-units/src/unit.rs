use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dimension::Dimension;
use crate::error::{UnitError, UnitResult};

const MAX_SYMBOL_BYTES: usize = 64;

/// A physical unit with an affine SI mapping:
/// `si_value = value * scale + offset`.
///
/// Linear units have `offset == 0`. Celsius/Fahrenheit are affine; they may
/// be converted but never multiplied, divided, added or subtracted, which
/// would silently turn an absolute temperature into a nonsense quantity.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UnitDef", into = "UnitDef")]
pub struct Unit {
    symbol: String,
    dimension: Dimension,
    scale: f64,
    offset: f64,
}

#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
struct UnitDef {
    symbol: String,
    dimension: Dimension,
    scale: f64,
    offset: f64,
}

impl TryFrom<UnitDef> for Unit {
    type Error = UnitError;

    fn try_from(def: UnitDef) -> Result<Self, Self::Error> {
        Self::new(def.symbol, def.dimension, def.scale, def.offset)
    }
}

impl From<Unit> for UnitDef {
    fn from(unit: Unit) -> Self {
        Self {
            symbol: unit.symbol,
            dimension: unit.dimension,
            scale: unit.scale,
            offset: unit.offset,
        }
    }
}

impl Unit {
    pub fn new(
        symbol: impl Into<String>,
        dimension: Dimension,
        scale: f64,
        offset: f64,
    ) -> UnitResult<Self> {
        let symbol = symbol.into();
        if symbol.is_empty() || symbol.len() > MAX_SYMBOL_BYTES {
            return Err(UnitError::InvalidSymbol {
                max: MAX_SYMBOL_BYTES,
            });
        }
        if !scale.is_finite() || scale == 0.0 {
            return Err(UnitError::InvalidScale { scale });
        }
        if !offset.is_finite() {
            return Err(UnitError::NonFiniteValue { value: offset });
        }
        if offset != 0.0 && dimension != Dimension::TEMPERATURE {
            return Err(UnitError::InvalidParameter {
                name: "offset",
                reason: "affine units are only defined for absolute temperature".to_string(),
            });
        }
        Ok(Self {
            symbol,
            dimension,
            scale,
            offset,
        })
    }

    pub fn linear(symbol: impl Into<String>, dimension: Dimension, scale: f64) -> UnitResult<Self> {
        Self::new(symbol, dimension, scale, 0.0)
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn dimension(&self) -> Dimension {
        self.dimension
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn offset(&self) -> f64 {
        self.offset
    }

    pub fn is_affine(&self) -> bool {
        self.offset != 0.0
    }

    pub fn is_compatible(&self, other: &Self) -> bool {
        self.dimension == other.dimension
    }

    pub fn to_si(&self, value: f64) -> UnitResult<f64> {
        if !value.is_finite() {
            return Err(UnitError::NonFiniteValue { value });
        }
        let si = value * self.scale + self.offset;
        if !si.is_finite() {
            return Err(UnitError::NonFiniteValue { value: si });
        }
        Ok(si)
    }

    pub fn from_si(&self, si: f64) -> UnitResult<f64> {
        if !si.is_finite() {
            return Err(UnitError::NonFiniteValue { value: si });
        }
        let value = (si - self.offset) / self.scale;
        if !value.is_finite() {
            return Err(UnitError::NonFiniteValue { value });
        }
        Ok(value)
    }

    pub fn convert_to(&self, value: f64, target: &Self) -> UnitResult<f64> {
        if self.dimension != target.dimension {
            return Err(UnitError::DimensionMismatch {
                expected: target.dimension,
                found: self.dimension,
            });
        }
        target.from_si(self.to_si(value)?)
    }

    fn require_linear(&self, operation: &'static str) -> UnitResult<()> {
        if self.is_affine() {
            return Err(UnitError::AffineArithmetic { operation });
        }
        Ok(())
    }

    fn composed_symbol(&self, other: &Self, operator: &str) -> String {
        let candidate = format!("{}{}{}", self.symbol, operator, other.symbol);
        if candidate.len() <= MAX_SYMBOL_BYTES {
            candidate
        } else {
            format!("({})", self.dimension.signature())
        }
    }

    pub fn multiply(&self, other: &Self) -> UnitResult<Self> {
        self.require_linear("multiplication")?;
        other.require_linear("multiplication")?;
        let scale = self.scale * other.scale;
        if !scale.is_finite() {
            return Err(UnitError::NonFiniteValue { value: scale });
        }
        Self::linear(
            self.composed_symbol(other, "*"),
            self.dimension.checked_mul(other.dimension)?,
            scale,
        )
    }

    pub fn divide(&self, other: &Self) -> UnitResult<Self> {
        self.require_linear("division")?;
        other.require_linear("division")?;
        let scale = self.scale / other.scale;
        if !scale.is_finite() {
            return Err(UnitError::NonFiniteValue { value: scale });
        }
        Self::linear(
            self.composed_symbol(other, "/"),
            self.dimension.checked_div(other.dimension)?,
            scale,
        )
    }

    pub fn powi(&self, power: i8) -> UnitResult<Self> {
        self.require_linear("exponentiation")?;
        if power == 0 {
            return Self::linear("1", Dimension::SCALAR, 1.0);
        }
        let scale = self.scale.powi(i32::from(power));
        if !scale.is_finite() || scale == 0.0 {
            return Err(UnitError::NonFiniteValue { value: scale });
        }
        let symbol = if power == 1 {
            self.symbol.clone()
        } else {
            format!("{}^{}", self.symbol, power)
        };
        Self::linear(symbol, self.dimension.checked_powi(power)?, scale)
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.symbol)
    }
}

/// The deterministic built-in unit registry. Symbols are exact lookup keys;
/// aliases are separate keys that convert to the same SI magnitude.
#[derive(Debug, Clone, Default)]
pub struct UnitRegistry {
    units: BTreeMap<String, Unit>,
}

impl UnitRegistry {
    pub fn si() -> Self {
        let mut registry = Self::default();
        let mut add = |unit: Unit| {
            registry.units.insert(unit.symbol().to_string(), unit);
        };

        let scalar = Dimension::SCALAR;
        let length = Dimension::LENGTH;
        let mass = Dimension::MASS;
        let time = Dimension::TIME;
        let current = Dimension::CURRENT;
        let temperature = Dimension::TEMPERATURE;

        add(Unit::linear("1", scalar, 1.0).unwrap());
        add(Unit::linear("%", scalar, 0.01).unwrap());
        add(Unit::linear("m", length, 1.0).unwrap());
        add(Unit::linear("mm", length, 1e-3).unwrap());
        add(Unit::linear("um", length, 1e-6).unwrap());
        add(Unit::linear("nm", length, 1e-9).unwrap());
        add(Unit::linear("km", length, 1e3).unwrap());
        add(Unit::linear("in", length, 0.0254).unwrap());
        add(Unit::linear("mil", length, 2.54e-5).unwrap());
        add(Unit::linear("ft", length, 0.3048).unwrap());
        add(Unit::linear("m^2", Dimension::AREA, 1.0).unwrap());
        add(Unit::linear("mm^2", Dimension::AREA, 1e-6).unwrap());
        add(Unit::linear("m^3", Dimension::VOLUME, 1.0).unwrap());
        add(Unit::linear("kg", mass, 1.0).unwrap());
        add(Unit::linear("g", mass, 1e-3).unwrap());
        add(Unit::linear("mg", mass, 1e-6).unwrap());
        add(Unit::linear("s", time, 1.0).unwrap());
        add(Unit::linear("ms", time, 1e-3).unwrap());
        add(Unit::linear("us", time, 1e-6).unwrap());
        add(Unit::linear("ns", time, 1e-9).unwrap());
        add(Unit::linear("min", time, 60.0).unwrap());
        add(Unit::linear("h", time, 3600.0).unwrap());
        add(Unit::linear("Hz", Dimension::FREQUENCY, 1.0).unwrap());
        add(Unit::linear("kHz", Dimension::FREQUENCY, 1e3).unwrap());
        add(Unit::linear("MHz", Dimension::FREQUENCY, 1e6).unwrap());
        add(Unit::linear("rpm", Dimension::FREQUENCY, 1.0 / 60.0).unwrap());
        add(Unit::linear("A", current, 1.0).unwrap());
        add(Unit::linear("mA", current, 1e-3).unwrap());
        add(Unit::linear("uA", current, 1e-6).unwrap());
        add(Unit::linear("K", temperature, 1.0).unwrap());
        add(Unit::new("degC", temperature, 1.0, 273.15).unwrap());
        add(Unit::new("degF", temperature, 5.0 / 9.0, 273.15 - 32.0 * 5.0 / 9.0).unwrap());
        add(Unit::linear("degC_delta", temperature, 1.0).unwrap());
        add(Unit::linear("degF_delta", temperature, 5.0 / 9.0).unwrap());
        add(Unit::linear("C", Dimension::CHARGE, 1.0).unwrap());
        add(Unit::linear("V", Dimension::VOLTAGE, 1.0).unwrap());
        add(Unit::linear("mV", Dimension::VOLTAGE, 1e-3).unwrap());
        add(Unit::linear("F", Dimension::CAPACITANCE, 1.0).unwrap());
        add(Unit::linear("uF", Dimension::CAPACITANCE, 1e-6).unwrap());
        add(Unit::linear("nF", Dimension::CAPACITANCE, 1e-9).unwrap());
        add(Unit::linear("pF", Dimension::CAPACITANCE, 1e-12).unwrap());
        add(Unit::linear("H", Dimension::INDUCTANCE, 1.0).unwrap());
        add(Unit::linear("mH", Dimension::INDUCTANCE, 1e-3).unwrap());
        add(Unit::linear("uH", Dimension::INDUCTANCE, 1e-6).unwrap());
        add(Unit::linear("nH", Dimension::INDUCTANCE, 1e-9).unwrap());
        add(Unit::linear("ohm", Dimension::RESISTANCE, 1.0).unwrap());
        add(Unit::linear("kohm", Dimension::RESISTANCE, 1e3).unwrap());
        add(Unit::linear("mohm", Dimension::RESISTANCE, 1e-3).unwrap());
        add(Unit::linear("S", Dimension::CONDUCTANCE, 1.0).unwrap());
        add(Unit::linear("N", Dimension::FORCE, 1.0).unwrap());
        add(Unit::linear("kN", Dimension::FORCE, 1e3).unwrap());
        add(Unit::linear("lbf", Dimension::FORCE, 4.448_221_615_260_5).unwrap());
        add(Unit::linear("kip", Dimension::FORCE, 4_448.221_615_260_5).unwrap());
        add(Unit::linear("Pa", Dimension::PRESSURE, 1.0).unwrap());
        add(Unit::linear("kPa", Dimension::PRESSURE, 1e3).unwrap());
        add(Unit::linear("MPa", Dimension::PRESSURE, 1e6).unwrap());
        add(Unit::linear("GPa", Dimension::PRESSURE, 1e9).unwrap());
        add(Unit::linear("psi", Dimension::PRESSURE, 6_894.757_293_168).unwrap());
        add(Unit::linear("ksi", Dimension::PRESSURE, 6_894_757.293_168).unwrap());
        add(Unit::linear("J", Dimension::ENERGY, 1.0).unwrap());
        add(Unit::linear("kJ", Dimension::ENERGY, 1e3).unwrap());
        add(Unit::linear("W", Dimension::POWER, 1.0).unwrap());
        add(Unit::linear("mW", Dimension::POWER, 1e-3).unwrap());
        add(Unit::linear("kW", Dimension::POWER, 1e3).unwrap());
        add(Unit::linear("N*m", Dimension::ENERGY, 1.0).unwrap());
        add(Unit::linear("lbf*ft", Dimension::ENERGY, 1.355_817_948_331_400_4).unwrap());
        add(Unit::linear("ft*lbf", Dimension::ENERGY, 1.355_817_948_331_400_4).unwrap());
        add(Unit::linear("rad", scalar, 1.0).unwrap());
        add(Unit::linear("deg", scalar, std::f64::consts::PI / 180.0).unwrap());
        add(Unit::linear("dB", scalar, 1.0).unwrap());
        add(Unit::linear("1/K", Dimension::INVERSE_TEMPERATURE, 1.0).unwrap());
        add(Unit::linear("K/W", Dimension::THERMAL_RESISTANCE, 1.0).unwrap());
        add(Unit::linear("J/K", Dimension::THERMAL_CAPACITANCE, 1.0).unwrap());
        add(Unit::linear("W/K", Dimension::THERMAL_CONDUCTANCE, 1.0).unwrap());
        add(Unit::linear("W/m/K", Dimension::new([1, 1, -3, 0, -1, 0, 0]), 1.0).unwrap());
        add(Unit::linear("J/kg/K", Dimension::new([2, 0, -2, 0, -1, 0, 0]), 1.0).unwrap());
        add(Unit::linear("kg/m^3", Dimension::DENSITY, 1.0).unwrap());
        add(Unit::linear("mol", Dimension::AMOUNT, 1.0).unwrap());
        add(Unit::linear("cd", Dimension::LUMINOUS_INTENSITY, 1.0).unwrap());

        registry
    }

    pub fn get(&self, symbol: &str) -> Option<&Unit> {
        self.units.get(symbol)
    }

    pub fn require(&self, symbol: &str) -> UnitResult<Unit> {
        self.units
            .get(symbol)
            .cloned()
            .ok_or_else(|| UnitError::InvalidParameter {
                name: "unit",
                reason: format!("unknown unit symbol {symbol:?}"),
            })
    }

    pub fn convert(&self, value: f64, from: &str, to: &str) -> UnitResult<f64> {
        let from = self.require(from)?;
        let to = self.require(to)?;
        from.convert_to(value, &to)
    }

    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.units.keys().map(String::as_str)
    }
}

/// One numeric value together with its dimensioned unit.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "QuantityDef", into = "QuantityDef")]
pub struct Quantity {
    value: f64,
    unit: Unit,
}

#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuantityDef {
    value: f64,
    unit: Unit,
}

impl TryFrom<QuantityDef> for Quantity {
    type Error = UnitError;

    fn try_from(def: QuantityDef) -> Result<Self, Self::Error> {
        Self::new(def.value, def.unit)
    }
}

impl From<Quantity> for QuantityDef {
    fn from(quantity: Quantity) -> Self {
        Self {
            value: quantity.value,
            unit: quantity.unit,
        }
    }
}

impl Quantity {
    pub fn new(value: f64, unit: Unit) -> UnitResult<Self> {
        if !value.is_finite() {
            return Err(UnitError::NonFiniteValue { value });
        }
        Ok(Self { value, unit })
    }

    pub fn dimensionless(value: f64) -> UnitResult<Self> {
        Self::new(value, Unit::linear("1", Dimension::SCALAR, 1.0)?)
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn unit(&self) -> &Unit {
        &self.unit
    }

    pub fn dimension(&self) -> Dimension {
        self.unit.dimension()
    }

    pub fn si_value(&self) -> UnitResult<f64> {
        self.unit.to_si(self.value)
    }

    pub fn value_in(&self, target: &Unit) -> UnitResult<f64> {
        self.unit.convert_to(self.value, target)
    }

    pub fn to_unit(&self, target: Unit) -> UnitResult<Self> {
        let value = self.unit.convert_to(self.value, &target)?;
        Self::new(value, target)
    }

    pub fn scale(&self, factor: f64) -> UnitResult<Self> {
        self.unit.require_linear("scaling")?;
        let value = self.value * factor;
        if !value.is_finite() {
            return Err(UnitError::NonFiniteValue { value });
        }
        Self::new(value, self.unit.clone())
    }

    pub fn add(&self, other: &Self) -> UnitResult<Self> {
        self.unit.require_linear("addition")?;
        other.unit.require_linear("addition")?;
        let rhs = other.unit.convert_to(other.value, &self.unit)?;
        Self::new(self.value + rhs, self.unit.clone())
    }

    pub fn sub(&self, other: &Self) -> UnitResult<Self> {
        self.unit.require_linear("subtraction")?;
        other.unit.require_linear("subtraction")?;
        let rhs = other.unit.convert_to(other.value, &self.unit)?;
        Self::new(self.value - rhs, self.unit.clone())
    }

    pub fn mul(&self, other: &Self) -> UnitResult<Self> {
        let unit = self.unit.multiply(&other.unit)?;
        Self::new(self.value * other.value, unit)
    }

    pub fn div(&self, other: &Self) -> UnitResult<Self> {
        if other.value == 0.0 {
            return Err(UnitError::Indeterminate {
                reason: "division by a zero quantity".to_string(),
            });
        }
        let unit = self.unit.divide(&other.unit)?;
        Self::new(self.value / other.value, unit)
    }

    pub fn powi(&self, power: i8) -> UnitResult<Self> {
        self.unit.require_linear("exponentiation")?;
        let value = self.value.powi(i32::from(power));
        Self::new(value, self.unit.powi(power)?)
    }

    pub fn compare(&self, other: &Self) -> UnitResult<std::cmp::Ordering> {
        let rhs = other.unit.convert_to(other.value, &self.unit)?;
        self.value
            .partial_cmp(&rhs)
            .ok_or_else(|| UnitError::Indeterminate {
                reason: "comparison produced no ordering".to_string(),
            })
    }

    pub fn approx_eq(&self, other: &Self, absolute_tolerance: f64) -> UnitResult<bool> {
        if !absolute_tolerance.is_finite() || absolute_tolerance < 0.0 {
            return Err(UnitError::InvalidParameter {
                name: "absolute_tolerance",
                reason: "must be finite and >= 0".to_string(),
            });
        }
        let rhs = other.unit.convert_to(other.value, &self.unit)?;
        Ok((self.value - rhs).abs() <= absolute_tolerance)
    }

    pub fn is_zero(&self) -> bool {
        self.value == 0.0
    }

    pub fn abs(&self) -> UnitResult<Self> {
        Self::new(self.value.abs(), self.unit.clone())
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.value, self.unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> UnitRegistry {
        UnitRegistry::si()
    }

    #[test]
    fn affine_temperature_conversion_round_trips() {
        let units = registry();
        let celsius = units.convert(100.0, "degC", "degF").unwrap();
        assert!((celsius - 212.0).abs() < 1e-9);
        let back = units.convert(celsius, "degF", "degC").unwrap();
        assert!((back - 100.0).abs() < 1e-9);
    }

    #[test]
    fn temperature_delta_is_not_absolute_temperature() {
        let units = registry();
        let delta = units.convert(10.0, "degC_delta", "degF_delta").unwrap();
        assert!((delta - 18.0).abs() < 1e-9);
        let fahrenheit = units.convert(10.0, "degC", "degF").unwrap();
        assert!((fahrenheit - 50.0).abs() < 1e-9);
    }

    #[test]
    fn affine_addition_is_refused() {
        let units = registry();
        let a = Quantity::new(20.0, units.require("degC").unwrap()).unwrap();
        let b = Quantity::new(10.0, units.require("degC").unwrap()).unwrap();
        assert!(matches!(a.add(&b), Err(UnitError::AffineArithmetic { .. })));
    }

    #[test]
    fn multiplicative_dimension_check() {
        let units = registry();
        let torque = Quantity::new(100.0, units.require("N*m").unwrap()).unwrap();
        let k = Quantity::new(0.2, units.require("1").unwrap()).unwrap();
        let diameter = Quantity::new(12.0, units.require("mm").unwrap()).unwrap();
        let force = torque.div(&k).unwrap().div(&diameter).unwrap();
        let newton = units.require("N").unwrap();
        let value = force.value_in(&newton).unwrap();
        assert!((value - 41_666.666_666_666_664).abs() < 1e-6);
        assert_eq!(force.dimension(), Dimension::FORCE);
    }

    #[test]
    fn adding_newton_to_millimetre_is_refused() {
        let units = registry();
        let a = Quantity::new(1.0, units.require("N").unwrap()).unwrap();
        let b = Quantity::new(1.0, units.require("mm").unwrap()).unwrap();
        assert!(matches!(
            a.add(&b),
            Err(UnitError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn division_by_zero_quantity_is_typed() {
        let units = registry();
        let a = Quantity::new(1.0, units.require("N").unwrap()).unwrap();
        let zero = Quantity::new(0.0, units.require("m").unwrap()).unwrap();
        assert!(matches!(a.div(&zero), Err(UnitError::Indeterminate { .. })));
    }

    #[test]
    fn imperial_torque_conversion_uses_the_exact_definition() {
        let units = registry();
        let value = units.convert(100.0, "lbf*ft", "N*m").unwrap();
        assert!((value - 135.581_794_833_140_04).abs() < 1e-9);
    }
}
