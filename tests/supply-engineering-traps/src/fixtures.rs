//! Deterministic fixtures and numerical checks for the FEA trap.
//!
//! The `.msh` files are synthetic MSH2 counts, not claims about a real part.
//! The Poisson study is a manufactured-solution check: `-u'' = pi^2 sin(pi x)`
//! with `u(0) = u(1) = 0` and exact solution `u = sin(pi x)`. It exercises
//! second-order finite differences without importing external data.

use engineering_solvers::mesh_stats;

/// Fixed refinement levels. The largest level is small enough to stay bounded
/// and the sequence is linear in the number of levels.
pub const REFINEMENT_LEVELS: [usize; 5] = [8, 16, 32, 64, 128];
const MAX_REFINEMENT_LEVEL: usize = 256;
const MAX_PATHOLOGY_LEVELS: usize = 8;

const MESH_MIN: &str = include_str!("../fixtures/mesh_min.msh");
const MESH_EMPTY: &str = include_str!("../fixtures/mesh_empty.msh");
const MESH_TRUNCATED: &str = include_str!("../fixtures/mesh_truncated.msh");

/// Result of probing the bundled mesh fixtures through the real Gmsh parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshFixtureProbe {
    /// Nodes in the valid fixture.
    pub ok_nodes: usize,
    /// Elements in the valid fixture.
    pub ok_elements: usize,
    /// Typed refusal for the empty fixture.
    pub empty_error: Option<String>,
    /// Typed refusal for the truncated fixture.
    pub truncated_error: Option<String>,
}

impl MeshFixtureProbe {
    /// True only when the valid mesh parses and both adversarial fixtures are
    /// refused.
    pub fn ok(&self) -> bool {
        self.ok_nodes > 0
            && self.ok_elements > 0
            && self.empty_error.as_deref() == Some("mesh is empty")
            && self.truncated_error.is_some()
    }
}

/// Parse the valid, empty and truncated MSH2 fixtures.
pub fn mesh_fixture_probe() -> MeshFixtureProbe {
    let ok = mesh_stats(MESH_MIN.as_bytes());
    let empty = mesh_stats(MESH_EMPTY.as_bytes());
    let truncated = mesh_stats(MESH_TRUNCATED.as_bytes());
    MeshFixtureProbe {
        ok_nodes: ok.as_ref().map_or(0, |stats| stats.nodes),
        ok_elements: ok.as_ref().map_or(0, |stats| stats.elements),
        empty_error: empty.err(),
        truncated_error: truncated.err(),
    }
}

/// Compute the discrete L2 error of a second-order finite-difference solve of
/// the manufactured Poisson problem with `n` interior nodes.
pub fn poisson_l2_error(n: usize) -> Result<f64, String> {
    if n == 0 || n > MAX_REFINEMENT_LEVEL {
        return Err(format!(
            "refinement level {n} is outside 1..={MAX_REFINEMENT_LEVEL}"
        ));
    }
    let h = 1.0 / (n as f64 + 1.0);
    let diagonal = 2.0 / (h * h);
    let off_diagonal = -1.0 / (h * h);
    let rhs = |index: usize| {
        let x = index as f64 * h;
        std::f64::consts::PI * std::f64::consts::PI * (std::f64::consts::PI * x).sin()
    };
    let mut upper = vec![0.0; n];
    let mut solution = vec![0.0; n];
    upper[0] = off_diagonal / diagonal;
    solution[0] = rhs(1) / diagonal;
    for index in 1..n {
        let denominator = diagonal - off_diagonal * upper[index - 1];
        upper[index] = if index + 1 < n {
            off_diagonal / denominator
        } else {
            0.0
        };
        solution[index] = (rhs(index + 1) - off_diagonal * solution[index - 1]) / denominator;
    }
    // Back substitution.
    for index in (0..n - 1).rev() {
        solution[index] -= upper[index] * solution[index + 1];
    }
    let mut sum = 0.0;
    for (index, value) in solution.iter().enumerate() {
        let x = (index + 1) as f64 * h;
        let exact = (std::f64::consts::PI * x).sin();
        let difference = value - exact;
        sum += difference * difference;
    }
    let error = (h * sum).sqrt();
    if error.is_finite() {
        Ok(error)
    } else {
        Err("manufactured-solution error is not finite".to_string())
    }
}

/// Run the fixed manufactured-solution refinement sequence.
pub fn manufactured_solution_study() -> Result<Vec<(usize, f64)>, String> {
    REFINEMENT_LEVELS
        .iter()
        .map(|level| poisson_l2_error(*level).map(|error| (*level, error)))
        .collect()
}

/// Observed order from two errors on meshes refined by a factor of two.
pub fn observed_order(coarse: f64, fine: f64) -> Option<f64> {
    if !coarse.is_finite() || !fine.is_finite() || coarse <= 0.0 || fine <= 0.0 || fine >= coarse {
        return None;
    }
    Some((coarse / fine).ln() / 2.0_f64.ln())
}

/// Detect a pathological refinement study. Returns a typed reason when the
/// sequence is non-monotone, stalled, or has an implausibly low observed order.
/// This is an acceptance check, not a physical model.
pub fn convergence_pathology(errors: &[f64]) -> Option<String> {
    if errors.len() < 3 || errors.len() > MAX_PATHOLOGY_LEVELS {
        return Some(format!(
            "expected 3..={MAX_PATHOLOGY_LEVELS} refinement levels, got {}",
            errors.len()
        ));
    }
    if errors
        .iter()
        .any(|error| !error.is_finite() || *error <= 0.0)
    {
        return Some("refinement errors must be finite and positive".to_string());
    }
    for pair in errors.windows(2) {
        if pair[1] >= pair[0] {
            return Some("non-monotone refinement: the error did not decrease".to_string());
        }
    }
    let last = errors.len() - 1;
    let reduction = errors[last - 1] / errors[last];
    if reduction < 1.5 {
        return Some(
            "stalled refinement: the final level reduced the error by less than 1.5x".to_string(),
        );
    }
    let Some(order) = observed_order(errors[last - 1], errors[last]) else {
        return Some("could not compute an observed order from the final refinement".to_string());
    };
    if order < 0.5 {
        return Some(format!(
            "observed order {order:.3} is below the first-order threshold"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_mesh_fixture_parses_and_adversarial_fixtures_are_refused() {
        let probe = mesh_fixture_probe();
        assert!(probe.ok(), "mesh probe failed: {probe:?}");
        assert_eq!(probe.ok_nodes, 8);
        assert_eq!(probe.ok_elements, 6);
    }

    #[test]
    fn manufactured_solution_is_second_order_and_bounded() {
        let study = manufactured_solution_study().unwrap();
        assert_eq!(study.len(), REFINEMENT_LEVELS.len());
        for pair in study.windows(2) {
            let order = observed_order(pair[0].1, pair[1].1).unwrap();
            assert!(order > 1.8, "order {order} at n={}", pair[1].0);
        }
        assert!(
            convergence_pathology(&study.iter().map(|(_, e)| *e).collect::<Vec<_>>()).is_none()
        );
    }

    #[test]
    fn pathological_refinement_is_detected() {
        assert!(convergence_pathology(&[0.40, 0.35, 0.36, 0.22]).is_some());
        assert!(convergence_pathology(&[0.40, 0.20, 0.10, 0.095]).is_some());
        assert!(convergence_pathology(&[0.4, f64::NAN, 0.1]).is_some());
        assert!(convergence_pathology(&[0.4]).is_some());
    }

    #[test]
    fn refinement_level_is_bounded_before_allocation() {
        assert!(poisson_l2_error(0).is_err());
        assert!(poisson_l2_error(MAX_REFINEMENT_LEVEL + 1).is_err());
        assert!(poisson_l2_error(usize::MAX).is_err());
    }
}
