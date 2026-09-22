//! Acquisition mechanisms and their documented precedence.
//!
//! The order is `docs/acquire.md` §1 with the local-cache step handled by
//! the planner before any mechanism is considered:
//!
//! 1. fresh local cache (planner step, not a mechanism)
//! 2. [`AcquisitionMechanism::OfficialApi`] — official API
//! 3. [`AcquisitionMechanism::DirectHttp`] — stable first-party JSON/HTTP
//! 4. [`AcquisitionMechanism::BrowserNetwork`] — browser network/XHR/CDP
//! 5. [`AcquisitionMechanism::EmbeddedState`] — embedded page/app state
//! 6. [`AcquisitionMechanism::Dom`] — semantic DOM extraction
//!
//! Rendered-text fallback and human verification are *outcomes* of the
//! planner, not acquisition mechanisms: this crate models the five
//! mechanisms that can actually fetch bytes, and refuses (or demands
//! verification) when none of them is available. No site name is ever
//! consulted: a mechanism is chosen from capabilities, health, credentials
//! and quota alone.

use serde::{Deserialize, Serialize};

/// One way to acquire data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionMechanism {
    /// The official, credentialed API.
    OfficialApi,
    /// A stable first-party JSON/HTTP response fetched directly.
    DirectHttp,
    /// Browser network capture (XHR/fetch/CDP).
    BrowserNetwork,
    /// Embedded page or application state.
    EmbeddedState,
    /// Semantic DOM extraction.
    Dom,
}

/// The documented precedence order, strongest first.
pub const MECHANISM_PRECEDENCE: [AcquisitionMechanism; 5] = [
    AcquisitionMechanism::OfficialApi,
    AcquisitionMechanism::DirectHttp,
    AcquisitionMechanism::BrowserNetwork,
    AcquisitionMechanism::EmbeddedState,
    AcquisitionMechanism::Dom,
];

impl AcquisitionMechanism {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfficialApi => "official_api",
            Self::DirectHttp => "direct_http",
            Self::BrowserNetwork => "browser_network",
            Self::EmbeddedState => "embedded_state",
            Self::Dom => "dom",
        }
    }

    /// The precedence rank (0 = strongest).
    pub const fn precedence_rank(self) -> u8 {
        match self {
            Self::OfficialApi => 0,
            Self::DirectHttp => 1,
            Self::BrowserNetwork => 2,
            Self::EmbeddedState => 3,
            Self::Dom => 4,
        }
    }

    /// Whether this mechanism is the official API path (the only path
    /// governed by the API quota and the no-silent-substitution policy).
    pub const fn is_official_api(self) -> bool {
        matches!(self, Self::OfficialApi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_is_the_spec_order_and_totally_ordered() {
        for (index, mechanism) in MECHANISM_PRECEDENCE.iter().enumerate() {
            assert_eq!(mechanism.precedence_rank() as usize, index);
        }
        let mut sorted = MECHANISM_PRECEDENCE;
        sorted.sort_by_key(|m| m.precedence_rank());
        assert_eq!(sorted, MECHANISM_PRECEDENCE);
        assert!(AcquisitionMechanism::OfficialApi.is_official_api());
        assert!(!AcquisitionMechanism::Dom.is_official_api());
    }
}
