//! Bounded empirical quantile distributions.
//!
//! Supplier delivery, quality and lead-time observations arrive as sampled
//! distributions, not point estimates. The representation is intentionally
//! small: four quantiles plus the sample size. Quantiles must be monotonic,
//! non-negative and drawn from at least one observation. There is no
//! interpolation and no distribution fitting here: planning consumes the
//! stated quantile with an explicit policy.

use serde::{Deserialize, Serialize};

use crate::ExactError;

/// Maximum supported sample size (bounds any downstream arithmetic).
pub const MAX_SAMPLE_SIZE: u32 = 100_000_000;

/// The quantile a policy asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantile {
    /// 10th percentile.
    P10,
    /// 50th percentile (median).
    P50,
    /// 90th percentile.
    P90,
    /// 99th percentile.
    P99,
}

/// A monotonic empirical distribution of non-negative integer observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    /// 10th percentile.
    pub p10: i64,
    /// Median.
    pub p50: i64,
    /// 90th percentile.
    pub p90: i64,
    /// 99th percentile.
    pub p99: i64,
    /// Number of observations behind the quantiles.
    pub sample_size: u32,
}

impl Distribution {
    /// Validate monotonicity and sample bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        if self.sample_size == 0 {
            return Err(ExactError::OutOfBounds {
                what: "sample size",
                detail: "zero observations".to_string(),
            });
        }
        if self.sample_size > MAX_SAMPLE_SIZE {
            return Err(ExactError::OutOfBounds {
                what: "sample size",
                detail: format!("{} exceeds {MAX_SAMPLE_SIZE}", self.sample_size),
            });
        }
        if self.p10 < 0 {
            return Err(ExactError::Negative { what: "quantile" });
        }
        if !(self.p10 <= self.p50 && self.p50 <= self.p90 && self.p90 <= self.p99) {
            return Err(ExactError::OutOfBounds {
                what: "quantiles",
                detail: format!(
                    "not monotonic: {} {} {} {}",
                    self.p10, self.p50, self.p90, self.p99
                ),
            });
        }
        Ok(())
    }

    /// The stated quantile.
    pub const fn quantile(&self, quantile: Quantile) -> i64 {
        match quantile {
            Quantile::P10 => self.p10,
            Quantile::P50 => self.p50,
            Quantile::P90 => self.p90,
            Quantile::P99 => self.p99,
        }
    }

    /// The median.
    pub const fn median(&self) -> i64 {
        self.p50
    }

    /// A deterministic dispersion proxy: `p90 - p10`, used where a full
    /// distribution is unnecessary.
    pub fn spread(&self) -> i64 {
        self.p90 - self.p10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonicity_and_sample_bounds_are_enforced() {
        let valid = Distribution {
            p10: 5,
            p50: 7,
            p90: 10,
            p99: 14,
            sample_size: 40,
        };
        assert!(valid.validate().is_ok());
        assert_eq!(valid.quantile(Quantile::P90), 10);
        assert_eq!(valid.spread(), 5);

        let unordered = Distribution {
            p10: 5,
            p50: 9,
            p90: 8,
            p99: 14,
            sample_size: 40,
        };
        assert!(unordered.validate().is_err());

        let empty = Distribution {
            p10: 0,
            p50: 0,
            p90: 0,
            p99: 0,
            sample_size: 0,
        };
        assert!(empty.validate().is_err());

        let negative = Distribution {
            p10: -1,
            p50: 0,
            p90: 1,
            p99: 2,
            sample_size: 1,
        };
        assert!(negative.validate().is_err());
    }
}
