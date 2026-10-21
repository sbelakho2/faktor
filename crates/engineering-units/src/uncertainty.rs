use serde::{Deserialize, Serialize};

use crate::error::{UnitError, UnitResult};

pub const MAX_STRATIFIED_SAMPLES: usize = 100_000;

/// A closed numeric interval. It carries no implicit unit; callers keep the
/// unit alongside it (see `UncertainQuantity` in the models crate).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "IntervalDef", into = "IntervalDef")]
pub struct Interval {
    lower: f64,
    upper: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct IntervalDef {
    lower: f64,
    upper: f64,
}

impl TryFrom<IntervalDef> for Interval {
    type Error = UnitError;

    fn try_from(def: IntervalDef) -> Result<Self, Self::Error> {
        Self::new(def.lower, def.upper)
    }
}

impl From<Interval> for IntervalDef {
    fn from(interval: Interval) -> Self {
        Self {
            lower: interval.lower,
            upper: interval.upper,
        }
    }
}

impl Interval {
    pub fn new(lower: f64, upper: f64) -> UnitResult<Self> {
        if !lower.is_finite() || !upper.is_finite() {
            return Err(UnitError::NonFiniteValue {
                value: if lower.is_finite() { upper } else { lower },
            });
        }
        if lower > upper {
            return Err(UnitError::InvalidInterval { lower, upper });
        }
        Ok(Self { lower, upper })
    }

    pub fn point(value: f64) -> UnitResult<Self> {
        Self::new(value, value)
    }

    pub fn lower(self) -> f64 {
        self.lower
    }

    pub fn upper(self) -> f64 {
        self.upper
    }

    pub fn width(self) -> f64 {
        self.upper - self.lower
    }

    pub fn midpoint(self) -> f64 {
        (self.lower + self.upper) / 2.0
    }

    pub fn contains(self, value: f64) -> bool {
        value.is_finite() && value >= self.lower && value <= self.upper
    }

    #[allow(clippy::should_implement_trait)]
    pub fn add(self, rhs: Self) -> Self {
        Self {
            lower: self.lower + rhs.lower,
            upper: self.upper + rhs.upper,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn sub(self, rhs: Self) -> Self {
        Self {
            lower: self.lower - rhs.upper,
            upper: self.upper - rhs.lower,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn mul(self, rhs: Self) -> Self {
        let products = [
            self.lower * rhs.lower,
            self.lower * rhs.upper,
            self.upper * rhs.lower,
            self.upper * rhs.upper,
        ];
        let mut lower = products[0];
        let mut upper = products[0];
        for product in &products[1..] {
            if *product < lower {
                lower = *product;
            }
            if *product > upper {
                upper = *product;
            }
        }
        Self { lower, upper }
    }

    pub fn scale(self, factor: f64) -> UnitResult<Self> {
        if !factor.is_finite() {
            return Err(UnitError::NonFiniteValue { value: factor });
        }
        let a = self.lower * factor;
        let b = self.upper * factor;
        Self::new(a.min(b), a.max(b))
    }

    #[allow(clippy::should_implement_trait)]
    pub fn div(self, rhs: Self) -> UnitResult<Self> {
        if rhs.contains(0.0) {
            return Err(UnitError::Indeterminate {
                reason: "interval division by a divisor spanning zero".to_string(),
            });
        }
        let candidates = [
            self.lower / rhs.lower,
            self.lower / rhs.upper,
            self.upper / rhs.lower,
            self.upper / rhs.upper,
        ];
        let mut lower = candidates[0];
        let mut upper = candidates[0];
        for value in &candidates[1..] {
            if *value < lower {
                lower = *value;
            }
            if *value > upper {
                upper = *value;
            }
        }
        Self::new(lower, upper)
    }

    pub fn exp(self) -> UnitResult<Self> {
        let lower = self.lower.exp();
        let upper = self.upper.exp();
        if !lower.is_finite() || !upper.is_finite() {
            return Err(UnitError::Indeterminate {
                reason: "interval exponential overflowed".to_string(),
            });
        }
        Ok(Self { lower, upper })
    }
}

/// A deterministic probability distribution used for tolerances and scatter.
///
/// `Normal` is unbounded; `envelope()` returns a documented 3-sigma numeric
/// envelope, never a claim of a hard physical bound.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "DistributionDef", into = "DistributionDef")]
pub enum Distribution {
    Point { value: f64 },
    Uniform { low: f64, high: f64 },
    Triangular { low: f64, mode: f64, high: f64 },
    Normal { mean: f64, std_dev: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum DistributionDef {
    Point { value: f64 },
    Uniform { low: f64, high: f64 },
    Triangular { low: f64, mode: f64, high: f64 },
    Normal { mean: f64, std_dev: f64 },
}

impl TryFrom<DistributionDef> for Distribution {
    type Error = UnitError;

    fn try_from(def: DistributionDef) -> Result<Self, Self::Error> {
        let distribution = match def {
            DistributionDef::Point { value } => Distribution::Point { value },
            DistributionDef::Uniform { low, high } => Distribution::Uniform { low, high },
            DistributionDef::Triangular { low, mode, high } => {
                Distribution::Triangular { low, mode, high }
            }
            DistributionDef::Normal { mean, std_dev } => Distribution::Normal { mean, std_dev },
        };
        distribution.validate()?;
        Ok(distribution)
    }
}

impl From<Distribution> for DistributionDef {
    fn from(distribution: Distribution) -> Self {
        match distribution {
            Distribution::Point { value } => DistributionDef::Point { value },
            Distribution::Uniform { low, high } => DistributionDef::Uniform { low, high },
            Distribution::Triangular { low, mode, high } => {
                DistributionDef::Triangular { low, mode, high }
            }
            Distribution::Normal { mean, std_dev } => DistributionDef::Normal { mean, std_dev },
        }
    }
}

impl Distribution {
    pub fn point(value: f64) -> UnitResult<Self> {
        let distribution = Self::Point { value };
        distribution.validate()?;
        Ok(distribution)
    }

    pub fn uniform(low: f64, high: f64) -> UnitResult<Self> {
        let distribution = Self::Uniform { low, high };
        distribution.validate()?;
        Ok(distribution)
    }

    pub fn triangular(low: f64, mode: f64, high: f64) -> UnitResult<Self> {
        let distribution = Self::Triangular { low, mode, high };
        distribution.validate()?;
        Ok(distribution)
    }

    pub fn normal(mean: f64, std_dev: f64) -> UnitResult<Self> {
        let distribution = Self::Normal { mean, std_dev };
        distribution.validate()?;
        Ok(distribution)
    }

    pub fn validate(&self) -> UnitResult<()> {
        match *self {
            Self::Point { value } => {
                if value.is_finite() {
                    Ok(())
                } else {
                    Err(UnitError::NonFiniteValue { value })
                }
            }
            Self::Uniform { low, high } => {
                if !low.is_finite() || !high.is_finite() {
                    return Err(UnitError::InvalidDistribution {
                        kind: "uniform",
                        reason: "bounds must be finite".to_string(),
                    });
                }
                if low >= high {
                    return Err(UnitError::InvalidDistribution {
                        kind: "uniform",
                        reason: format!("requires low < high, got [{low}, {high}]"),
                    });
                }
                Ok(())
            }
            Self::Triangular { low, mode, high } => {
                if !low.is_finite() || !mode.is_finite() || !high.is_finite() {
                    return Err(UnitError::InvalidDistribution {
                        kind: "triangular",
                        reason: "bounds and mode must be finite".to_string(),
                    });
                }
                if !(low < mode && mode < high) {
                    return Err(UnitError::InvalidDistribution {
                        kind: "triangular",
                        reason: format!("requires low < mode < high, got [{low}, {mode}, {high}]"),
                    });
                }
                Ok(())
            }
            Self::Normal { mean, std_dev } => {
                if !mean.is_finite() || !std_dev.is_finite() {
                    return Err(UnitError::InvalidDistribution {
                        kind: "normal",
                        reason: "mean and std_dev must be finite".to_string(),
                    });
                }
                if std_dev <= 0.0 {
                    return Err(UnitError::InvalidDistribution {
                        kind: "normal",
                        reason: format!("std_dev must be > 0, got {std_dev}"),
                    });
                }
                Ok(())
            }
        }
    }

    pub fn mean(self) -> f64 {
        match self {
            Self::Point { value } => value,
            Self::Uniform { low, high } => (low + high) / 2.0,
            Self::Triangular { low, mode, high } => (low + mode + high) / 3.0,
            Self::Normal { mean, .. } => mean,
        }
    }

    pub fn std_dev(self) -> f64 {
        match self {
            Self::Point { .. } => 0.0,
            Self::Uniform { low, high } => (high - low) / (12.0f64).sqrt(),
            Self::Triangular { low, high, .. } => (high - low) / (24.0f64).sqrt(),
            Self::Normal { std_dev, .. } => std_dev,
        }
    }

    pub fn support(self) -> Option<Interval> {
        match self {
            Self::Point { value } => Interval::point(value).ok(),
            Self::Uniform { low, high } => Interval::new(low, high).ok(),
            Self::Triangular { low, high, .. } => Interval::new(low, high).ok(),
            Self::Normal { .. } => None,
        }
    }

    /// Numeric envelope: exact support for bounded distributions, and a
    /// three-standard-deviation envelope for a normal distribution. The
    /// latter is explicitly not a physical bound.
    pub fn envelope(self) -> Option<Interval> {
        match self {
            Self::Normal { mean, std_dev } => {
                Interval::new(mean - 3.0 * std_dev, mean + 3.0 * std_dev).ok()
            }
            _ => self.support(),
        }
    }

    /// Deterministic inverse CDF. Only p strictly inside (0, 1) is accepted,
    /// so infinite normal tails can never become a finite fabricated result.
    pub fn quantile(self, p: f64) -> UnitResult<f64> {
        if !p.is_finite() || p <= 0.0 || p >= 1.0 {
            return Err(UnitError::InvalidQuantile { p });
        }
        let value = match self {
            Self::Point { value } => value,
            Self::Uniform { low, high } => low + p * (high - low),
            Self::Triangular { low, mode, high } => {
                let split = (mode - low) / (high - low);
                if p < split {
                    low + ((p * (high - low) * (mode - low)).sqrt())
                } else {
                    high - (((1.0 - p) * (high - low) * (high - mode)).sqrt())
                }
            }
            Self::Normal { mean, std_dev } => mean + std_dev * inverse_standard_normal(p),
        };
        if value.is_finite() {
            Ok(value)
        } else {
            Err(UnitError::Indeterminate {
                reason: "quantile produced a non-finite value".to_string(),
            })
        }
    }

    /// Stratified (midpoint-of-equal-probability) samples. No RNG, no clock,
    /// no thread scheduling: the same distribution and count always produce
    /// the same samples.
    pub fn stratified_samples(self, count: usize) -> UnitResult<Vec<f64>> {
        if count == 0 || count > MAX_STRATIFIED_SAMPLES {
            return Err(UnitError::InvalidSampleCount {
                min: 1,
                max: MAX_STRATIFIED_SAMPLES,
                got: count,
            });
        }
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            let p = (index as f64 + 0.5) / count as f64;
            out.push(self.quantile(p)?);
        }
        Ok(out)
    }
}

/// Uncertainty attached to one value. `Exact` means the model treats the
/// input as exact; it never means "unknown" and it is never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uncertainty {
    Exact,
    Interval(Interval),
    Distribution(Distribution),
}

impl Uncertainty {
    pub fn interval(self) -> Option<Interval> {
        match self {
            Self::Exact => None,
            Self::Interval(interval) => Some(interval),
            Self::Distribution(distribution) => distribution.envelope(),
        }
    }

    pub fn mean(self) -> Option<f64> {
        match self {
            Self::Exact => None,
            Self::Interval(interval) => Some(interval.midpoint()),
            Self::Distribution(distribution) => Some(distribution.mean()),
        }
    }

    pub fn std_dev(self) -> Option<f64> {
        match self {
            Self::Exact => Some(0.0),
            Self::Interval(_) => None,
            Self::Distribution(distribution) => Some(distribution.std_dev()),
        }
    }

    pub fn is_exact(self) -> bool {
        matches!(self, Self::Exact)
    }

    /// Deterministic samples used for propagation. An interval is sampled at
    /// its endpoints (conservative extreme pairing); an exact value returns
    /// its zero-perturbation sample set.
    pub fn deviation_samples(self, count: usize) -> UnitResult<Vec<f64>> {
        match self {
            Self::Exact => {
                if count == 0 || count > MAX_STRATIFIED_SAMPLES {
                    return Err(UnitError::InvalidSampleCount {
                        min: 1,
                        max: MAX_STRATIFIED_SAMPLES,
                        got: count,
                    });
                }
                Ok(vec![0.0; count])
            }
            Self::Interval(interval) => Ok(vec![
                interval.lower() - interval.midpoint(),
                interval.upper() - interval.midpoint(),
            ]),
            Self::Distribution(distribution) => {
                let mean = distribution.mean();
                Ok(distribution
                    .stratified_samples(count)?
                    .into_iter()
                    .map(|sample| sample - mean)
                    .collect())
            }
        }
    }
}

/// Result of a linear uncertainty propagation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinearPropagation {
    pub interval: Option<Interval>,
    pub mean_deviation: Option<f64>,
    pub std_dev: Option<f64>,
    pub method: String,
}

/// Propagate `sum(coefficient_i * deviation_i)` without assuming statistical
/// independence. If every term is an interval the result is a hard interval.
/// If every non-exact term is a distribution, deterministic stratified
/// samples are combined comonotonically and labelled as such; that is a
/// sensitivity envelope, not an independence proof.
pub fn propagate_linear(terms: &[(f64, Uncertainty)]) -> UnitResult<LinearPropagation> {
    let has_interval = terms
        .iter()
        .any(|(_, uncertainty)| matches!(uncertainty, Uncertainty::Interval(_)));
    let all_exact = terms.iter().all(|(_, u)| u.is_exact());
    if all_exact {
        return Ok(LinearPropagation {
            interval: Interval::point(0.0).ok(),
            mean_deviation: Some(0.0),
            std_dev: Some(0.0),
            method: "exact".to_string(),
        });
    }
    if has_interval {
        let mut sum = Interval::point(0.0)?;
        for (coefficient, uncertainty) in terms {
            if !coefficient.is_finite() {
                return Err(UnitError::NonFiniteValue {
                    value: *coefficient,
                });
            }
            let interval = uncertainty
                .interval()
                .ok_or_else(|| UnitError::Indeterminate {
                    reason: "interval propagation requires every term to have an interval"
                        .to_string(),
                })?;
            sum = sum.add(interval.scale(*coefficient)?);
        }
        return Ok(LinearPropagation {
            interval: Some(sum),
            mean_deviation: Some(sum.midpoint()),
            std_dev: None,
            method: "interval arithmetic (worst-case bounds)".to_string(),
        });
    }

    let sample_count = 512;
    let mut combined = vec![0.0; sample_count];
    for (coefficient, uncertainty) in terms {
        let deviations = uncertainty.deviation_samples(sample_count)?;
        for (slot, deviation) in combined.iter_mut().zip(deviations) {
            *slot += coefficient * deviation;
        }
    }
    let mut lower = combined[0];
    let mut upper = combined[0];
    let mut mean = 0.0;
    for (index, value) in combined.iter().enumerate() {
        if *value < lower {
            lower = *value;
        }
        if *value > upper {
            upper = *value;
        }
        mean += (value - mean) / (index as f64 + 1.0);
    }
    let variance = combined
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / sample_count as f64;
    Ok(LinearPropagation {
        interval: Interval::new(lower, upper).ok(),
        mean_deviation: Some(mean),
        std_dev: Some(variance.sqrt()),
        method: "deterministic stratified samples, comonotonic envelope".to_string(),
    })
}

/// Acklam's rational approximation to the inverse standard normal CDF.
fn inverse_standard_normal(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    const P_LOW: f64 = 0.024_25;
    const P_HIGH: f64 = 1.0 - P_LOW;

    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= P_HIGH {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_multiplication_handles_negative_bounds() {
        let a = Interval::new(-2.0, 3.0).unwrap();
        let b = Interval::new(-4.0, 5.0).unwrap();
        let product = a.mul(b);
        assert_eq!(product.lower(), -12.0);
        assert_eq!(product.upper(), 15.0);
    }

    #[test]
    fn interval_division_by_zero_spanning_divisor_is_refused() {
        let a = Interval::new(1.0, 2.0).unwrap();
        let b = Interval::new(-1.0, 1.0).unwrap();
        assert!(b.contains(0.0));
        assert!(matches!(a.div(b), Err(UnitError::Indeterminate { .. })));
    }

    #[test]
    fn uniform_quantiles_and_samples_are_deterministic() {
        let distribution = Distribution::uniform(10.0, 20.0).unwrap();
        assert!((distribution.quantile(0.5).unwrap() - 15.0).abs() < 1e-12);
        let first = distribution.stratified_samples(8).unwrap();
        let second = distribution.stratified_samples(8).unwrap();
        assert_eq!(first, second);
        assert!(first[0] > 10.0 && first[7] < 20.0);
    }

    #[test]
    fn normal_two_sigma_quantile_matches_reference() {
        let distribution = Distribution::normal(0.0, 1.0).unwrap();
        let value = distribution.quantile(0.977_249_868).unwrap();
        assert!((value - 2.0).abs() < 1e-5);
    }

    #[test]
    fn invalid_distributions_are_typed() {
        assert!(Distribution::uniform(1.0, 1.0).is_err());
        assert!(Distribution::triangular(0.0, 2.0, 1.0).is_err());
        assert!(Distribution::normal(0.0, 0.0).is_err());
        assert!(Distribution::normal(0.0, 1.0)
            .unwrap()
            .quantile(0.0)
            .is_err());
        assert!(Distribution::normal(0.0, 1.0)
            .unwrap()
            .quantile(1.0)
            .is_err());
    }

    #[test]
    fn interval_propagation_is_conservative() {
        let k = Uncertainty::Interval(Interval::new(0.16, 0.24).unwrap());
        let result = propagate_linear(&[(1.0, k), (-2.0, k)]).unwrap();
        let interval = result.interval.unwrap();
        assert!((interval.lower() + 0.32).abs() < 1e-12);
        assert!((interval.upper() + 0.08).abs() < 1e-12);
    }
}
