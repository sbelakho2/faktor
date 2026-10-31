// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Committed regression budgets for the fixture measurements (HAR-14).
//!
//! Budgets are DATA, not code: the measured numbers live in
//! `tests/eval/fixtures/budgets.json` (an external fixture — the tests never
//! recompute the expectation from the system under measurement). Each entry
//! records the measured baseline and a generous CI-safe bound; a test fails
//! ONLY on egregious regression (measured > bound, where bound =
//! max(3 x baseline, absolute floor)). Ordinary wall-clock noise never fails
//! a gate.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One budget entry: the recorded baseline and the regression bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub baseline_ms: u64,
    pub bound_ms: u64,
}

/// The committed budget table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budgets {
    pub schema: String,
    pub budgets: BTreeMap<String, Budget>,
}

/// Load and validate the committed budgets file.
pub fn load_budgets(path: &Path) -> Result<Budgets, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let budgets: Budgets =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if budgets.schema != "faktor-eval-budgets/v1" {
        return Err(format!("unexpected schema '{}'", budgets.schema));
    }
    Ok(budgets)
}

impl Budgets {
    /// Check one measured duration against its committed bound. Err names
    /// the budget key and both numbers (egregious regression only).
    pub fn check(&self, key: &str, measured_ms: u64) -> Result<Budget, String> {
        let budget = self
            .budgets
            .get(key)
            .ok_or_else(|| format!("no committed budget for '{key}'"))?;
        if measured_ms > budget.bound_ms {
            return Err(format!(
                "budget '{key}': measured {measured_ms}ms exceeds bound {}ms (baseline {}ms)",
                budget.bound_ms, budget.baseline_ms
            ));
        }
        Ok(*budget)
    }
}

/// Derive a bound from a baseline: max(3 x baseline, floor). Used when
/// RECORDING a new baseline (never as a runtime expectation).
pub fn bound_from_baseline(baseline_ms: u64, floor_ms: u64) -> u64 {
    (baseline_ms.saturating_mul(3)).max(floor_ms)
}
