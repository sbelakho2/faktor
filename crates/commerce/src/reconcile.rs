//! Multi-extractor reconciliation.
//!
//! Several extractors may observe the same field (official API, first-party
//! JSON, browser network, embedded state, structured markup, DOM, rendered
//! text). [`resolve_field`] reconciles those [`Observation`]s into a
//! [`ResolvedField`]:
//!
//! - all observations agree ⇒
//!   [`ResolvedField::Value`] with the highest-authority origin and the
//!   corroborating origins;
//! - observations disagree ⇒ [`ResolvedField::Conflict`] — conflicting
//!   prices are **never averaged**, the disagreement is recorded;
//! - no observations ⇒ [`ResolvedField::Missing`].
//!
//! The documented authority order (highest first) is
//! `OfficialApi > HttpJson > BrowserNetwork > EmbeddedState >
//! StructuredMarkup > Dom > RenderedText`
//! ([`ObservationOrigin::authority_rank`]). A caller that wants the winner
//! of a recorded conflict asks [`ResolvedField::authoritative`], which
//! applies exactly that ordering (highest rank, then newest observation,
//! then the lower origin rank) and never invents a value.
//!
//! [`ExtractionOutcome`] and [`SchemaFingerprint`] are the per-strategy
//! health/structural-fingerprint vocabulary: a strategy reports
//! success/not-applicable/schema-mismatch/conflict/failed, and a schema
//! digest comparison classifies drift instead of silently misparsing.

use serde::{Deserialize, Serialize};

use crate::offer::ObservationOrigin;
use crate::text::Text;
use faktor_core::authority::{authority_digest_labeled, CanonicalFieldWriter, CanonicalFields};

/// Domain separation for schema fingerprints.
pub const DOMAIN_SCHEMA_FINGERPRINT: &[u8] = b"faktor-commerce.schema-fingerprint";

/// One observation of a field by one extractor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation<T> {
    /// The observed value.
    pub value: T,
    /// The extractor origin.
    pub origin: ObservationOrigin,
    /// When it was observed.
    pub observed_at_ms: u64,
}

/// How disagreements are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationMode {
    /// Any disagreement is a [`ResolvedField::Conflict`] (the safe
    /// default).
    RequireAgreement,
    /// Disagreement is resolved by the documented authority ordering; the
    /// winner is the strictly highest-authority observation, and a tie at
    /// the top is still a conflict.
    AuthorityOrdering,
}

/// The reconciliation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationPolicy {
    /// How disagreements are handled.
    pub mode: ReconciliationMode,
    /// The hard bound on observations considered at once.
    pub max_observations: usize,
}

impl Default for ReconciliationPolicy {
    fn default() -> Self {
        Self {
            mode: ReconciliationMode::RequireAgreement,
            max_observations: 16,
        }
    }
}

/// A reconciled field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ResolvedField<T> {
    /// All observations agreed (or the authority ordering decided).
    Value {
        /// The resolved value.
        value: T,
        /// The origin that supplied it.
        authority: ObservationOrigin,
        /// The origins that corroborated it.
        corroborated_by: Vec<ObservationOrigin>,
    },
    /// Observations disagreed; the disagreement is recorded, never
    /// averaged.
    Conflict {
        /// Every observation, in input order.
        observations: Vec<Observation<T>>,
    },
    /// Nothing was observed.
    Missing,
}

impl<T> ResolvedField<T> {
    /// The resolved value, when there is one.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value { value, .. } => Some(value),
            Self::Conflict { .. } | Self::Missing => None,
        }
    }

    /// True for a recorded conflict.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// True for a missing field.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    /// The winner of a conflict by documented authority ordering.
    ///
    /// Returns the observation with the strictly highest authority rank;
    /// ties are broken by the newest `observed_at_ms`, then by the lower
    /// origin rank. Returns `None` when there is no conflict (use
    /// [`Self::value`]) or when two observations tie on both rank and
    /// timestamp with different values (the conflict is genuinely
    /// undecidable).
    pub fn authoritative(&self) -> Option<&Observation<T>>
    where
        T: PartialEq,
    {
        let Self::Conflict { observations } = self else {
            return None;
        };
        let mut best: Option<&Observation<T>> = None;
        let mut undecidable = false;
        for observation in observations {
            match best {
                None => best = Some(observation),
                Some(current) => {
                    let rank = observation.origin.authority_rank();
                    let current_rank = current.origin.authority_rank();
                    if rank > current_rank {
                        best = Some(observation);
                        undecidable = false;
                    } else if rank == current_rank {
                        if observation.observed_at_ms > current.observed_at_ms {
                            best = Some(observation);
                            undecidable = false;
                        } else if observation.observed_at_ms == current.observed_at_ms
                            && observation.value != current.value
                        {
                            undecidable = true;
                        }
                    }
                }
            }
        }
        if undecidable {
            None
        } else {
            best
        }
    }
}

/// Reconcile observations of one field. Never averages.
pub fn resolve_field<T: PartialEq + Clone>(
    observations: &[Observation<T>],
    policy: &ReconciliationPolicy,
) -> ResolvedField<T> {
    if observations.is_empty() {
        return ResolvedField::Missing;
    }
    if observations.len() > policy.max_observations {
        return ResolvedField::Conflict {
            observations: observations.to_vec(),
        };
    }
    let first = &observations[0];
    let all_equal = observations
        .iter()
        .all(|observation| observation.value == first.value);
    if all_equal {
        let mut authority = first.origin;
        let mut authority_at = first.observed_at_ms;
        for observation in &observations[1..] {
            if observation.origin.authority_rank() > authority.authority_rank()
                || (observation.origin.authority_rank() == authority.authority_rank()
                    && observation.observed_at_ms > authority_at)
            {
                authority = observation.origin;
                authority_at = observation.observed_at_ms;
            }
        }
        let corroborated_by = observations
            .iter()
            .filter(|observation| observation.origin != authority)
            .map(|observation| observation.origin)
            .collect();
        return ResolvedField::Value {
            value: first.value.clone(),
            authority,
            corroborated_by,
        };
    }
    if policy.mode == ReconciliationMode::AuthorityOrdering {
        let conflict = ResolvedField::Conflict {
            observations: observations.to_vec(),
        };
        if let Some(winner) = conflict.authoritative() {
            let corroborated_by = observations
                .iter()
                .filter(|observation| {
                    observation.value == winner.value && observation.origin != winner.origin
                })
                .map(|observation| observation.origin)
                .collect();
            return ResolvedField::Value {
                value: winner.value.clone(),
                authority: winner.origin,
                corroborated_by,
            };
        }
        return conflict;
    }
    ResolvedField::Conflict {
        observations: observations.to_vec(),
    }
}

/// What an extraction strategy reported for one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExtractionOutcome {
    /// The strategy extracted the field.
    Success,
    /// The strategy does not apply to this source/page.
    NotApplicable,
    /// The structure did not match the expected fingerprint.
    SchemaMismatch {
        /// The expected fingerprint, when one is known.
        expected: Option<SchemaFingerprint>,
        /// The observed fingerprint.
        observed: SchemaFingerprint,
    },
    /// The strategy observed conflicting values.
    Conflict {
        /// The conflicting field paths.
        fields: Vec<Text<128>>,
    },
    /// The strategy failed.
    Failed {
        /// Why it failed.
        reason: Text<256>,
    },
}

impl ExtractionOutcome {
    /// True only for [`ExtractionOutcome::Success`].
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// The wire label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotApplicable => "not_applicable",
            Self::SchemaMismatch { .. } => "schema_mismatch",
            Self::Conflict { .. } => "conflict",
            Self::Failed { .. } => "failed",
        }
    }
}

/// A structural fingerprint of a payload shape (JSON key paths, DOM
/// landmarks, script-state schema paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaFingerprint {
    /// `blake3:<64 hex>` over the sorted, deduplicated key paths.
    pub digest: String,
    /// The number of distinct key paths.
    pub keys: u32,
}

impl SchemaFingerprint {
    /// Fingerprint a set of key paths. Order and duplicates do not matter.
    pub fn from_key_paths(paths: &[&str]) -> Self {
        let mut sorted: Vec<&str> = paths
            .iter()
            .copied()
            .filter(|path| !path.is_empty())
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        let keys = sorted.len() as u32;
        let digest =
            authority_digest_labeled(DOMAIN_SCHEMA_FINGERPRINT, 1, FingerprintFields(&sorted));
        Self { digest, keys }
    }
}

struct FingerprintFields<'a>(&'a [&'a str]);

impl CanonicalFields for FingerprintFields<'_> {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        out.uint(self.0.len() as u64);
        for path in self.0 {
            out.text(path);
        }
    }
}

/// The classification of a fingerprint comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaDrift {
    /// The fingerprints are identical.
    Match,
    /// The fingerprints differ: the schema drifted.
    Drifted,
    /// At least one fingerprint is unknown.
    NotComparable,
}

/// Classify a schema comparison.
pub fn classify_schema_drift(
    expected: Option<&SchemaFingerprint>,
    observed: Option<&SchemaFingerprint>,
) -> SchemaDrift {
    match (expected, observed) {
        (Some(expected), Some(observed)) => {
            if expected.digest == observed.digest {
                SchemaDrift::Match
            } else {
                SchemaDrift::Drifted
            }
        }
        _ => SchemaDrift::NotComparable,
    }
}
