//! The benchmark's neutral contract.
//!
//! The SUT produces an opaque [`SutAnswer`]; the oracle consumes it. Neither
//! side can set the other's verdict: there is deliberately no metric boolean,
//! no `with_*` setter and no acceptance flag in this module.

use engineering_core::{FalsifiableTest, GateId};
use faktor_supply_core::Money;
use serde::Serialize;

/// Stable suite schema.
pub const SUITE_SCHEMA: &str = "faktor.supply-engineering-traps/v2";
/// Suite version.
pub const SUITE_VERSION: &str = "2.0.0";

/// The typed result a trap case is expected to produce. The oracle owns the
/// expected value; the SUT cannot choose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedKind {
    /// The pipeline executed and produced a usable result.
    Solved,
    /// The pipeline executed and rejected the design/claim through a required
    /// check.
    Rejected,
    /// The pipeline returned a typed refusal.
    Refused,
    /// The authority/theory/validity needed for a conclusion is missing.
    ResearchRequired,
    /// A value is unknown and remains unavailable.
    Unavailable,
    /// Conflicting authoritative candidates require escalation.
    Ambiguous,
}

impl ExpectedKind {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Solved => "solved",
            Self::Rejected => "rejected",
            Self::Refused => "refused",
            Self::ResearchRequired => "research_required",
            Self::Unavailable => "unavailable",
            Self::Ambiguous => "ambiguous",
        }
    }

    /// True for every typed non-answer.
    pub const fn is_typed_refusal(self) -> bool {
        !matches!(self, Self::Solved)
    }
}

/// One observed value, kept typed so money never becomes a float.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observation {
    /// A physical quantity in its explicit unit.
    Quantity {
        /// Name.
        name: String,
        /// Numeric value.
        value: f64,
        /// Unit symbol.
        unit: String,
    },
    /// Exact integer money.
    Money {
        /// Name.
        name: String,
        /// Micro-units.
        micros: i64,
        /// Currency code.
        currency: String,
    },
    /// A bounded text observation.
    Text {
        /// Name.
        name: String,
        /// Value.
        value: String,
    },
}

impl Observation {
    /// A physical quantity observation.
    pub fn quantity(name: impl Into<String>, value: f64, unit: impl Into<String>) -> Self {
        Self::Quantity {
            name: name.into(),
            value,
            unit: unit.into(),
        }
    }

    /// An exact money observation.
    pub fn money(name: impl Into<String>, amount: Money) -> Self {
        Self::Money {
            name: name.into(),
            micros: amount.micros(),
            currency: amount.currency.as_str().to_string(),
        }
    }

    /// A text observation.
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Text {
            name: name.into(),
            value: value.into(),
        }
    }

    /// Observation name.
    pub fn name(&self) -> &str {
        match self {
            Self::Quantity { name, .. } | Self::Money { name, .. } | Self::Text { name, .. } => {
                name
            }
        }
    }
}

/// The typed outcome of executing one case through the SUT pipeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SutOutcome {
    /// The model executed and produced observations.
    Solved {
        /// Observations.
        observations: Vec<Observation>,
    },
    /// The model executed and the design failed a required check.
    Rejected {
        /// Stable rejection code.
        code: String,
        /// Why the design was rejected.
        reasons: Vec<String>,
        /// Observable values produced before the rejection.
        observations: Vec<Observation>,
    },
    /// The model returned a typed refusal.
    Refused {
        /// Stable refusal code.
        code: String,
        /// Why it refused.
        reason: String,
        /// Missing or invalid inputs named by the refusal.
        required: Vec<String>,
    },
    /// A theory, authority or validity envelope is missing.
    ResearchRequired {
        /// The gate that cannot be established.
        gate: GateId,
        /// Why research is required.
        reason: String,
        /// Sources needed to proceed.
        sources_required: Vec<String>,
    },
    /// A value is unknown and stays unavailable.
    Unavailable {
        /// Why the value is unavailable.
        reason: String,
        /// Inputs needed to establish it.
        required: Vec<String>,
    },
    /// Conflicting authoritative candidates require escalation.
    Ambiguous {
        /// Candidate identifiers from the authority.
        candidates: Vec<String>,
        /// Why the candidates cannot be resolved.
        reason: String,
        /// Authority that can resolve the ambiguity.
        authority: String,
    },
}

impl SutOutcome {
    /// The outcome class.
    pub const fn kind(&self) -> ExpectedKind {
        match self {
            Self::Solved { .. } => ExpectedKind::Solved,
            Self::Rejected { .. } => ExpectedKind::Rejected,
            Self::Refused { .. } => ExpectedKind::Refused,
            Self::ResearchRequired { .. } => ExpectedKind::ResearchRequired,
            Self::Unavailable { .. } => ExpectedKind::Unavailable,
            Self::Ambiguous { .. } => ExpectedKind::Ambiguous,
        }
    }

    /// Stable refusal code, when the outcome is a refusal.
    pub fn refusal_code(&self) -> Option<&str> {
        match self {
            Self::Rejected { code, .. } | Self::Refused { code, .. } => Some(code),
            Self::ResearchRequired { .. }
            | Self::Unavailable { .. }
            | Self::Ambiguous { .. }
            | Self::Solved { .. } => None,
        }
    }

    /// Observations produced by a solved outcome.
    pub fn observations(&self) -> &[Observation] {
        match self {
            Self::Solved { observations } | Self::Rejected { observations, .. } => observations,
            _ => &[],
        }
    }
}

/// One gate's assessed state. This is an output of the 11-gate response, not a
/// caller assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStateKind {
    /// The gate was established.
    Established,
    /// The gate has a typed gap.
    Unavailable,
    /// The gate needs research.
    ResearchRequired,
}

/// One gate state record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GateRecord {
    /// Gate identity.
    pub gate: GateId,
    /// Assessed state.
    pub state: GateStateKind,
}

/// How the SUT represents uncertainty. A refusal cannot be exact.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SutUncertainty {
    /// The result is exact by definition.
    Exact,
    /// The result carries an explicit interval.
    Interval {
        /// Lower bound.
        lower: f64,
        /// Upper bound.
        upper: f64,
    },
    /// The SUT did not establish a value, so it cannot state one.
    NotEstimated {
        /// Why no estimate exists.
        reason: String,
    },
}

impl SutUncertainty {
    /// True for an explicit interval.
    pub const fn is_interval(&self) -> bool {
        matches!(self, Self::Interval { .. })
    }

    /// True for a typed absence of an estimate.
    pub const fn is_not_estimated(&self) -> bool {
        matches!(self, Self::NotEstimated { .. })
    }
}

/// One control probe executed by the SUT alongside the main fixture.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlAnswer {
    /// Stable control name.
    pub name: String,
    /// What the control produced.
    pub outcome: SutOutcome,
    /// Uncertainty shape of the control.
    pub uncertainty: SutUncertainty,
}

/// The opaque SUT answer. The oracle receives exactly this and nothing else
/// from the SUT.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SutAnswer {
    /// Case id.
    pub id: String,
    /// Typed outcome.
    pub outcome: SutOutcome,
    /// Assessed 11-gate states, in gate order.
    pub gate_states: Vec<GateRecord>,
    /// Missing information named by the SUT.
    pub missing_info: Vec<String>,
    /// Boundary/validity conditions named by the SUT.
    pub boundary_conditions: Vec<String>,
    /// Explicit uncertainty statement.
    pub uncertainty: SutUncertainty,
    /// Evidence/assumption authorities actually used.
    pub assumption_coverage: Vec<String>,
    /// Evidence/citation/digest strings.
    pub evidence: Vec<String>,
    /// Falsifiable experiments the SUT constructed.
    pub experiments: Vec<FalsifiableTest>,
    /// Control probes.
    pub controls: Vec<ControlAnswer>,
}

impl SutAnswer {
    /// The observation with `name`, when present.
    pub fn observation(&self, name: &str) -> Option<&Observation> {
        self.outcome
            .observations()
            .iter()
            .find(|observation| observation.name() == name)
    }

    /// A money observation in micro-units.
    pub fn money_micros(&self, name: &str) -> Option<i64> {
        match self.observation(name) {
            Some(Observation::Money { micros, .. }) => Some(*micros),
            _ => None,
        }
    }

    /// A numeric observation value.
    pub fn quantity(&self, name: &str) -> Option<f64> {
        match self.observation(name) {
            Some(Observation::Quantity { value, .. }) => Some(*value),
            _ => None,
        }
    }

    /// True when any missing-information or refusal-required line contains a
    /// token (ASCII case-insensitive).
    pub fn mentions_missing(&self, token: &str) -> bool {
        let token = token.to_ascii_lowercase();
        let in_missing = self
            .missing_info
            .iter()
            .any(|line| line.to_ascii_lowercase().contains(&token));
        let in_required = match &self.outcome {
            SutOutcome::Refused { required, .. } | SutOutcome::Unavailable { required, .. } => {
                required
                    .iter()
                    .any(|line| line.to_ascii_lowercase().contains(&token))
            }
            SutOutcome::ResearchRequired {
                sources_required, ..
            } => sources_required
                .iter()
                .any(|line| line.to_ascii_lowercase().contains(&token)),
            SutOutcome::Ambiguous { .. }
            | SutOutcome::Rejected { .. }
            | SutOutcome::Solved { .. } => false,
        };
        in_missing || in_required
    }

    /// True when any boundary-condition line contains a token.
    pub fn mentions_boundary(&self, token: &str) -> bool {
        let token = token.to_ascii_lowercase();
        self.boundary_conditions
            .iter()
            .any(|line| line.to_ascii_lowercase().contains(&token))
    }

    /// True when any coverage line contains a token.
    pub fn mentions_coverage(&self, token: &str) -> bool {
        let token = token.to_ascii_lowercase();
        self.assumption_coverage
            .iter()
            .chain(self.evidence.iter())
            .any(|line| line.to_ascii_lowercase().contains(&token))
    }

    /// The control with `name`.
    pub fn control(&self, name: &str) -> Option<&ControlAnswer> {
        self.controls.iter().find(|control| control.name == name)
    }

    /// Gate state for `gate`.
    pub fn gate(&self, gate: GateId) -> Option<GateStateKind> {
        self.gate_states
            .iter()
            .find(|record| record.gate == gate)
            .map(|record| record.state)
    }
}
