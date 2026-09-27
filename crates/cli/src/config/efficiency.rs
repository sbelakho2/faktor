//! `config::efficiency`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[efficiency]` section (audit 86 + the efficiency-variant
/// production flags): five independent boolean feature switches. In
/// PRODUCTION every flag defaults ON — the efficiency system is active —
/// and an explicit `false` is the documented OFF-SWITCH for that one
/// component (an absent section keeps the production defaults; a partial
/// section flips only the keys it names):
///
/// - `failure_learning`: feed the learning crate's failure prior into
///   context selection through `faktor_context`'s `FailurePrior` planner
///   seam (audit 68). OFF installs no prior (neutral omission risk);
/// - `ccr`: compressed-context representation for tool/evidence payloads.
///   OFF keeps raw tool output byte-identical (bounded excerpts only);
/// - `typed_handoff`: re-sent history rendered from durable task rows.
///   OFF falls back to the legacy transcript rendering;
/// - `semantic_context`: information-gain selection of evidence through the
///   ContextCompiler. OFF keeps the producer evidence list byte-identical
///   (neutral sparse fallback; unit contexts stay parity);
/// - `rework_routing`: rework-aware routing over durable verified-outcome
///   stats.
///
/// The section is strictly additive: unknown keys, non-boolean values,
/// duplicate keys and non-object shapes (a JSON array must never enable
/// flags by position) are parse errors on both load paths.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EfficiencyCfg {
    /// Failure-learning prior in context selection (audit 68).
    pub failure_learning: bool,
    /// Compressed Context Representation for tool/evidence payloads.
    pub ccr: bool,
    /// Typed handoff: re-sent history rendered from durable task rows.
    pub typed_handoff: bool,
    /// Semantic context: information-gain selection of evidence through the
    /// ContextCompiler (the audit's `information_gain` flag).
    pub semantic_context: bool,
    /// Rework-aware routing over durable verified-outcome stats.
    pub rework_routing: bool,
}

impl Default for EfficiencyCfg {
    /// The additive type-level default: every flag OFF. Unit/embedded
    /// callers that build `EfficiencyCfg::default()` keep the pre-efficiency
    /// behavior byte-for-byte; the PRODUCTION config paths use
    /// [`EfficiencyCfg::production_defaults`] (see [`Config::default`] and
    /// the serde default), which is what turns the efficiency system on for
    /// a daemon with an absent `[efficiency]` section.
    fn default() -> Self {
        Self {
            failure_learning: false,
            ccr: false,
            typed_handoff: false,
            semantic_context: false,
            rework_routing: false,
        }
    }
}

impl EfficiencyCfg {
    /// The PRODUCTION defaults (audit: the efficiency system is on by
    /// default): every component enabled, with the per-key `false` explicit
    /// off-switch documented on the struct.
    pub const fn production_defaults() -> Self {
        Self {
            failure_learning: true,
            ccr: true,
            typed_handoff: true,
            semantic_context: true,
            rework_routing: true,
        }
    }
}

/// Serde default for an absent `[efficiency]` section: production defaults
/// (all ON). A partial section flips only the keys it names.
pub(crate) fn production_efficiency() -> EfficiencyCfg {
    EfficiencyCfg::production_defaults()
}

/// The `[efficiency]` keys, in stable order (unknown-field errors list them).
pub(crate) const EFFICIENCY_FIELDS: &[&str] = &[
    "failure_learning",
    "ccr",
    "typed_handoff",
    "semantic_context",
    "information_gain",
    "rework_routing",
];

/// Map-only strict parsing for `[efficiency]`: unlike a derived struct with
/// all-default fields, a JSON sequence is REFUSED (serde would otherwise
/// accept `[true]` as positional field values), duplicates are refused, and
/// unknown keys are refused. Absent keys keep the `false` default.
impl<'de> serde::Deserialize<'de> for EfficiencyCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = EfficiencyCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [efficiency] section as a JSON object of booleans")
            }

            fn visit_map<A>(self, mut map: A) -> Result<EfficiencyCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                // A PRESENT partial section starts from the production
                // defaults and flips only the keys it names: an operator who
                // disables one component never silently disables the rest.
                let mut out = EfficiencyCfg::production_defaults();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "failure_learning" => (1u8, "failure_learning"),
                        "ccr" => (2, "ccr"),
                        "typed_handoff" => (4, "typed_handoff"),
                        // The audit calls this switch `information_gain`;
                        // `semantic_context` remains the canonical key. Both
                        // names flip the SAME bit, so naming both is a
                        // duplicate (refused), never a silent last-wins.
                        "semantic_context" | "information_gain" => (8, "semantic_context"),
                        "rework_routing" => (16, "rework_routing"),
                        other => return Err(A::Error::unknown_field(other, EFFICIENCY_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    let value = map.next_value::<bool>()?;
                    match bit {
                        1 => out.failure_learning = value,
                        2 => out.ccr = value,
                        4 => out.typed_handoff = value,
                        8 => out.semantic_context = value,
                        _ => out.rework_routing = value,
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}
