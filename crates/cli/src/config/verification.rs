//! `config::verification`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[verification]` section (daemon verification policy).
/// Strictly additive with `serde(default)`: an absent section (or absent
/// keys inside it) keep the crate defaults (quick ≤ 60 s, unit ≤ 600 s
/// inline, full in background). `quick_max_s: 0` disables the verification
/// service entirely (fail closed — mutating turns classify Unverified).
/// Unknown keys inside the section are parse errors (strict both on the
/// lenient and the strict load path).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCfg {
    #[serde(default = "default_quick_max_s")]
    pub quick_max_s: u64,
    #[serde(default = "default_unit_max_s")]
    pub unit_max_s: u64,
    #[serde(default = "default_full_as_background")]
    pub full_as_background: bool,
}

pub(crate) fn default_quick_max_s() -> u64 {
    60
}

pub(crate) fn default_unit_max_s() -> u64 {
    600
}

pub(crate) fn default_full_as_background() -> bool {
    true
}

impl Default for VerificationCfg {
    fn default() -> Self {
        Self {
            quick_max_s: default_quick_max_s(),
            unit_max_s: default_unit_max_s(),
            full_as_background: default_full_as_background(),
        }
    }
}

impl VerificationCfg {
    /// The verification policy this section resolves to. `None` when
    /// `quick_max_s` is 0: the verification service is DISABLED (fail
    /// closed — mutating turns classify Unverified, never silently
    /// complete). Sane values map onto the crate policy whose per-category
    /// budgets gate every check (`budget_for`).
    pub fn policy(&self) -> Option<faktor_verify::exec::VerificationPolicy> {
        if self.quick_max_s == 0 {
            return None;
        }
        Some(faktor_verify::exec::VerificationPolicy {
            quick_max: std::time::Duration::from_secs(self.quick_max_s),
            unit_max: std::time::Duration::from_secs(self.unit_max_s),
            full_as_background: self.full_as_background,
            min_inline: std::time::Duration::from_secs(5),
        })
    }
}
