use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{UnitError, UnitResult};

/// SI base dimensions, in fixed order:
/// length, mass, time, electric current, thermodynamic temperature,
/// amount of substance, luminous intensity.
pub const DIMENSION_COUNT: usize = 7;

/// An integer exponent vector over the seven SI base dimensions.
///
/// Exponents are deliberately small (`i8`) and every combination is
/// overflow-checked; a dimension can never wrap around into a false match.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Dimension {
    exponents: [i8; DIMENSION_COUNT],
}

impl Dimension {
    pub const SCALAR: Self = Self::new([0; DIMENSION_COUNT]);
    pub const LENGTH: Self = Self::new([1, 0, 0, 0, 0, 0, 0]);
    pub const MASS: Self = Self::new([0, 1, 0, 0, 0, 0, 0]);
    pub const TIME: Self = Self::new([0, 0, 1, 0, 0, 0, 0]);
    pub const CURRENT: Self = Self::new([0, 0, 0, 1, 0, 0, 0]);
    pub const TEMPERATURE: Self = Self::new([0, 0, 0, 0, 1, 0, 0]);
    pub const AMOUNT: Self = Self::new([0, 0, 0, 0, 0, 1, 0]);
    pub const LUMINOUS_INTENSITY: Self = Self::new([0, 0, 0, 0, 0, 0, 1]);

    pub const VELOCITY: Self = Self::new([1, 0, -1, 0, 0, 0, 0]);
    pub const ACCELERATION: Self = Self::new([1, 0, -2, 0, 0, 0, 0]);
    pub const FORCE: Self = Self::new([1, 1, -2, 0, 0, 0, 0]);
    pub const PRESSURE: Self = Self::new([-1, 1, -2, 0, 0, 0, 0]);
    pub const ENERGY: Self = Self::new([2, 1, -2, 0, 0, 0, 0]);
    pub const POWER: Self = Self::new([2, 1, -3, 0, 0, 0, 0]);
    pub const CHARGE: Self = Self::new([0, 0, 1, 1, 0, 0, 0]);
    pub const VOLTAGE: Self = Self::new([2, 1, -3, -1, 0, 0, 0]);
    pub const CAPACITANCE: Self = Self::new([-2, -1, 4, 2, 0, 0, 0]);
    pub const INDUCTANCE: Self = Self::new([2, 1, -2, -2, 0, 0, 0]);
    pub const RESISTANCE: Self = Self::new([2, 1, -3, -2, 0, 0, 0]);
    pub const CONDUCTANCE: Self = Self::new([-2, -1, 3, 2, 0, 0, 0]);
    pub const FREQUENCY: Self = Self::new([0, 0, -1, 0, 0, 0, 0]);
    pub const ANGLE: Self = Self::SCALAR;
    pub const THERMAL_RESISTANCE: Self = Self::new([-2, -1, 3, 0, 1, 0, 0]);
    pub const THERMAL_CAPACITANCE: Self = Self::new([2, 1, -2, 0, -1, 0, 0]);
    pub const THERMAL_CONDUCTANCE: Self = Self::new([2, 1, -3, 0, -1, 0, 0]);
    pub const AREA: Self = Self::new([2, 0, 0, 0, 0, 0, 0]);
    pub const VOLUME: Self = Self::new([3, 0, 0, 0, 0, 0, 0]);
    pub const DENSITY: Self = Self::new([-3, 1, 0, 0, 0, 0, 0]);
    pub const INVERSE_TEMPERATURE: Self = Self::new([0, 0, 0, 0, -1, 0, 0]);

    pub const fn new(exponents: [i8; DIMENSION_COUNT]) -> Self {
        Self { exponents }
    }

    pub const fn exponents(self) -> [i8; DIMENSION_COUNT] {
        self.exponents
    }

    pub const fn is_dimensionless(self) -> bool {
        let mut i = 0;
        while i < DIMENSION_COUNT {
            if self.exponents[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    pub fn checked_mul(self, rhs: Self) -> UnitResult<Self> {
        self.combine(rhs, 1)
    }

    pub fn checked_div(self, rhs: Self) -> UnitResult<Self> {
        self.combine(rhs, -1)
    }

    pub fn checked_add(self, rhs: Self) -> UnitResult<Self> {
        if self == rhs {
            Ok(self)
        } else {
            Err(UnitError::DimensionMismatch {
                expected: self,
                found: rhs,
            })
        }
    }

    pub fn checked_sub(self, rhs: Self) -> UnitResult<Self> {
        self.checked_add(rhs)
    }

    pub fn checked_powi(self, power: i8) -> UnitResult<Self> {
        let mut out = [0i8; DIMENSION_COUNT];
        for (slot, exponent) in out.iter_mut().zip(self.exponents) {
            *slot = exponent
                .checked_mul(power)
                .ok_or(UnitError::ExponentOverflow)?;
        }
        Ok(Self::new(out))
    }

    fn combine(self, rhs: Self, sign: i8) -> UnitResult<Self> {
        let mut out = [0i8; DIMENSION_COUNT];
        for (slot, (left, right)) in out
            .iter_mut()
            .zip(self.exponents.into_iter().zip(rhs.exponents))
        {
            let adjusted = if sign == 1 {
                right
            } else {
                right.checked_neg().ok_or(UnitError::ExponentOverflow)?
            };
            *slot = left
                .checked_add(adjusted)
                .ok_or(UnitError::ExponentOverflow)?;
        }
        Ok(Self::new(out))
    }

    /// Human-readable dimensional signature, e.g. `L^1 M^1 T^-2`.
    pub fn signature(self) -> String {
        if self.is_dimensionless() {
            return "1".to_string();
        }
        const NAMES: [&str; DIMENSION_COUNT] = ["L", "M", "T", "I", "Theta", "N", "J"];
        let mut out = String::new();
        for (name, exponent) in NAMES.iter().zip(self.exponents) {
            if exponent == 0 {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(name);
            out.push('^');
            out.push_str(&exponent.to_string());
        }
        out
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.signature())
    }
}

impl std::ops::Mul for Dimension {
    type Output = UnitResult<Self>;

    fn mul(self, rhs: Self) -> Self::Output {
        self.checked_mul(rhs)
    }
}

impl std::ops::Div for Dimension {
    type Output = UnitResult<Self>;

    fn div(self, rhs: Self) -> Self::Output {
        self.checked_div(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_is_mass_times_acceleration() {
        let accel = Dimension::ACCELERATION;
        assert_eq!(
            Dimension::MASS.checked_mul(accel).unwrap(),
            Dimension::FORCE
        );
    }

    #[test]
    fn torque_and_energy_share_a_dimension_but_not_a_meaning() {
        let torque = Dimension::FORCE.checked_mul(Dimension::LENGTH).unwrap();
        assert_eq!(torque, Dimension::ENERGY);
    }

    #[test]
    fn addition_preserves_the_common_dimension_and_refuses_mismatch() {
        assert_eq!(
            Dimension::VOLTAGE.checked_add(Dimension::VOLTAGE).unwrap(),
            Dimension::VOLTAGE
        );
        assert_eq!(
            Dimension::TEMPERATURE
                .checked_add(
                    Dimension::POWER
                        .checked_mul(Dimension::THERMAL_RESISTANCE)
                        .unwrap()
                )
                .unwrap(),
            Dimension::TEMPERATURE
        );
        assert!(Dimension::VOLTAGE.checked_add(Dimension::CURRENT).is_err());
        assert!(Dimension::VOLTAGE.checked_sub(Dimension::CURRENT).is_err());
    }

    #[test]
    fn exponent_overflow_is_refused() {
        let huge = Dimension::new([127, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            huge.checked_mul(Dimension::LENGTH),
            Err(UnitError::ExponentOverflow)
        );
    }
}
