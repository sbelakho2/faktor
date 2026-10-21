//! Dimension-checked quantities, tolerances, distributions and deterministic
//! numerical kernels for Faktor Engineering.
//!
//! The crate deliberately contains no domain constants and no hidden data:
//! unit conversion is exact-by-definition SI scaling, uncertainty is an
//! explicit interval or distribution, and every refusal is typed. An unknown
//! value is never represented as zero here.

pub mod dimension;
pub mod error;
pub mod math;
pub mod uncertainty;
pub mod unit;

pub use dimension::{Dimension, DIMENSION_COUNT};
pub use error::{UnitError, UnitResult};
pub use math::{bisection, clamp, kahan_sum, lerp, mean, sample_std_dev, RootSolution};
pub use uncertainty::{
    propagate_linear, Distribution, Interval, LinearPropagation, Uncertainty,
    MAX_STRATIFIED_SAMPLES,
};
pub use unit::{Quantity, Unit, UnitRegistry};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quantity_can_cross_unit_systems_without_losing_its_dimension() {
        let units = UnitRegistry::si();
        let inch = units.require("in").unwrap();
        let mm = units.require("mm").unwrap();
        let length = Quantity::new(0.5, inch).unwrap();
        let converted = length.to_unit(mm).unwrap();
        assert!((converted.value() - 12.7).abs() < 1e-12);
        assert_eq!(converted.dimension(), Dimension::LENGTH);
    }

    #[test]
    fn uncertainty_values_survive_serde_without_becoming_zero() {
        let uncertainty = Uncertainty::Distribution(Distribution::normal(0.2, 0.02).unwrap());
        let json = serde_json::to_string(&uncertainty).unwrap();
        let back: Uncertainty = serde_json::from_str(&json).unwrap();
        assert_eq!(back, uncertainty);
    }
}
