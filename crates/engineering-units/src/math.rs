use crate::error::{UnitError, UnitResult};

/// Compensated summation: deterministic, order-stable for a fixed input
/// slice, and far less sensitive to magnitude spread than a naive loop.
pub fn kahan_sum(values: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut compensation = 0.0;
    for value in values {
        let adjusted = *value - compensation;
        let next = sum + adjusted;
        compensation = (next - sum) - adjusted;
        sum = next;
    }
    sum
}

pub fn mean(values: &[f64]) -> UnitResult<f64> {
    if values.is_empty() {
        return Err(UnitError::EmptySamples);
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err(UnitError::NonFiniteValue {
            value: values
                .iter()
                .copied()
                .find(|value| !value.is_finite())
                .unwrap_or(f64::NAN),
        });
    }
    Ok(kahan_sum(values) / values.len() as f64)
}

/// Unbiased sample standard deviation (`n - 1` denominator). A single sample
/// has no defined spread and is refused rather than reported as zero.
pub fn sample_std_dev(values: &[f64]) -> UnitResult<f64> {
    if values.len() < 2 {
        return Err(UnitError::InvalidSampleCount {
            min: 2,
            max: usize::MAX,
            got: values.len(),
        });
    }
    let mean = mean(values)?;
    let sum = values
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>();
    Ok((sum / (values.len() - 1) as f64).sqrt())
}

pub fn clamp(value: f64, lower: f64, upper: f64) -> UnitResult<f64> {
    if !value.is_finite() || !lower.is_finite() || !upper.is_finite() {
        return Err(UnitError::InvalidParameter {
            name: "clamp",
            reason: "all inputs must be finite".to_string(),
        });
    }
    if lower > upper {
        return Err(UnitError::InvalidInterval { lower, upper });
    }
    Ok(value.max(lower).min(upper))
}

pub fn lerp(a: f64, b: f64, t: f64) -> UnitResult<f64> {
    if !a.is_finite() || !b.is_finite() || !t.is_finite() {
        return Err(UnitError::InvalidParameter {
            name: "lerp",
            reason: "all inputs must be finite".to_string(),
        });
    }
    Ok((b - a).mul_add(t, a))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RootSolution {
    pub root: f64,
    pub residual: f64,
    pub iterations: u32,
    pub converged: bool,
}

/// Bounded bisection root solve. The bracket and tolerances are validated
/// before any iteration; non-convergence is a typed flag on an honest result,
/// never a fabricated root.
pub fn bisection<F>(
    function: F,
    mut low: f64,
    mut high: f64,
    absolute_tolerance: f64,
    max_iterations: u32,
) -> UnitResult<RootSolution>
where
    F: Fn(f64) -> f64,
{
    if !low.is_finite() || !high.is_finite() {
        return Err(UnitError::InvalidParameter {
            name: "bracket",
            reason: "bounds must be finite".to_string(),
        });
    }
    if low >= high {
        return Err(UnitError::InvalidInterval {
            lower: low,
            upper: high,
        });
    }
    if !absolute_tolerance.is_finite() || absolute_tolerance <= 0.0 {
        return Err(UnitError::InvalidParameter {
            name: "absolute_tolerance",
            reason: "must be finite and > 0".to_string(),
        });
    }
    if max_iterations == 0 || max_iterations > 10_000 {
        return Err(UnitError::InvalidSampleCount {
            min: 1,
            max: 10_000,
            got: max_iterations as usize,
        });
    }

    let mut f_low = function(low);
    let mut f_high = function(high);
    if !f_low.is_finite() || !f_high.is_finite() {
        return Err(UnitError::Indeterminate {
            reason: "function returned a non-finite value at a bracket endpoint".to_string(),
        });
    }
    if f_low == 0.0 {
        return Ok(RootSolution {
            root: low,
            residual: 0.0,
            iterations: 0,
            converged: true,
        });
    }
    if f_high == 0.0 {
        return Ok(RootSolution {
            root: high,
            residual: 0.0,
            iterations: 0,
            converged: true,
        });
    }
    if f_low.signum() == f_high.signum() {
        return Err(UnitError::NoBracket {
            low,
            high,
            f_low,
            f_high,
        });
    }

    let mut root = low;
    let mut residual = f_low;
    for iteration in 1..=max_iterations {
        root = low + (high - low) / 2.0;
        residual = function(root);
        if !residual.is_finite() {
            return Err(UnitError::Indeterminate {
                reason: "function returned a non-finite value during bisection".to_string(),
            });
        }
        if residual == 0.0 || (high - low).abs() / 2.0 <= absolute_tolerance {
            return Ok(RootSolution {
                root,
                residual,
                iterations: iteration,
                converged: true,
            });
        }
        if residual.signum() == f_low.signum() {
            low = root;
            f_low = residual;
        } else {
            high = root;
            f_high = residual;
        }
    }
    let _ = f_high;
    Ok(RootSolution {
        root,
        residual,
        iterations: max_iterations,
        converged: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bisection_finds_a_physical_root() {
        let solved = bisection(|x| x * x - 2.0, 0.0, 2.0, 1e-12, 200).unwrap();
        assert!(solved.converged);
        assert!((solved.root - std::f64::consts::SQRT_2).abs() < 1e-11);
    }

    #[test]
    fn no_bracket_is_refused() {
        assert!(matches!(
            bisection(|x| x * x + 1.0, -1.0, 1.0, 1e-9, 100),
            Err(UnitError::NoBracket { .. })
        ));
    }

    #[test]
    fn non_convergence_is_flagged_not_hidden() {
        let solved = bisection(|x| x - 0.1, 0.0, 1.0, 1e-18, 2).unwrap();
        assert!(!solved.converged);
        assert_eq!(solved.iterations, 2);
    }

    #[test]
    fn sample_statistics_are_deterministic() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(mean(&values).unwrap(), 2.5);
        assert!((sample_std_dev(&values).unwrap() - 1.290_994_448_735_805_6).abs() < 1e-12);
        assert!(sample_std_dev(&[1.0]).is_err());
    }
}
