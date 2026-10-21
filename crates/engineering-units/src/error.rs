use thiserror::Error;

use crate::dimension::Dimension;

/// Every refusal in the units/uncertainty plane is typed. Non-finite input,
/// dimension mismatch, invalid distribution support and arithmetic traps are
/// never coerced to zero or silently ignored.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum UnitError {
    #[error("value is not finite: {value}")]
    NonFiniteValue { value: f64 },
    #[error("unit scale must be finite and non-zero, got {scale}")]
    InvalidScale { scale: f64 },
    #[error("unit symbol is empty or longer than {max} bytes")]
    InvalidSymbol { max: usize },
    #[error("dimension exponent overflow while combining dimensions")]
    ExponentOverflow,
    #[error("dimension mismatch: expected {expected}, found {found}")]
    DimensionMismatch {
        expected: Dimension,
        found: Dimension,
    },
    #[error("affine (offset) units cannot be used in {operation}")]
    AffineArithmetic { operation: &'static str },
    #[error("interval is invalid: lower={lower}, upper={upper}")]
    InvalidInterval { lower: f64, upper: f64 },
    #[error("distribution is invalid for {kind}: {reason}")]
    InvalidDistribution { kind: &'static str, reason: String },
    #[error("quantile must lie strictly inside (0, 1), got {p}")]
    InvalidQuantile { p: f64 },
    #[error("sample count must lie in {min}..={max}, got {got}")]
    InvalidSampleCount { min: usize, max: usize, got: usize },
    #[error("empty sample set")]
    EmptySamples,
    #[error(
        "root solve bracket does not contain a sign change: f({low})={f_low}, f({high})={f_high}"
    )]
    NoBracket {
        low: f64,
        high: f64,
        f_low: f64,
        f_high: f64,
    },
    #[error("invalid parameter {name}: {reason}")]
    InvalidParameter { name: &'static str, reason: String },
    #[error("indeterminate result: {reason}")]
    Indeterminate { reason: String },
}

pub type UnitResult<T> = Result<T, UnitError>;
