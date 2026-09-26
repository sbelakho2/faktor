//! The browser execution seam: the runtime plans, the adapter executes.
//!
//! Spec §10: "Browser fallback is for legitimate field availability/
//! resilience, never quota evasion; API and browser fallback are
//! independently circuit-broken. API-first: do not scrape around official
//! interfaces."
//!
//! Mechanism *selection* is the `faktor-acquire` `AcquisitionPlanner`'s job:
//! the service plans (capabilities, credentials, quota, health, cache) and
//! hands the chosen [`Mechanism`](crate::contract::Mechanism) to the adapter
//! through [`AcquireCtx::mechanism`](crate::context::AcquireCtx::mechanism).
//! This module contains no policy, no cause taxonomy and no decision
//! function: a connector that is told to execute a browser mechanism calls
//! the injected [`BrowserFallback`], and a connector that is told to execute
//! an API mechanism never touches it. There is no code path here that can
//! switch one into the other.
//!
//! `faktor-browser`'s egress broker applies the per-connector destination
//! policy to whatever the provider does; this crate never widens it.

use async_trait::async_trait;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{SourceError, SourceId};

use crate::context::AcquireCtx;

/// Maximum fields in one browser observation.
pub const MAX_FALLBACK_FIELDS: usize = 16;

/// One browser observation: bounded named fields, all untrusted data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackObservation {
    values: Vec<(Text<64>, Text<512>)>,
}

impl FallbackObservation {
    /// Validate and bound a field list.
    pub fn new(values: Vec<(Text<64>, Text<512>)>) -> Result<Self, SourceError> {
        if values.len() > MAX_FALLBACK_FIELDS {
            return Err(SourceError::ExtractionIncomplete);
        }
        Ok(Self { values })
    }

    /// One field value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(field, _)| field.as_str() == name)
            .map(|(_, value)| value.as_str())
    }

    /// The field count.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True when no fields were observed.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The injected browser authority seam (implemented by the acquisition
/// runtime over `faktor-browser`; never by a connector).
#[async_trait]
pub trait BrowserFallback: Send + Sync {
    /// Observe the wanted fields on one page. The provider is responsible
    /// for the destination policy, bounded capture, verification handling
    /// and returning typed errors.
    async fn observe(
        &self,
        ctx: &AcquireCtx,
        source: &SourceId,
        url: &CanonicalUrl,
        wanted: &[&'static str],
    ) -> Result<FallbackObservation, SourceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_are_bounded() {
        let mut values = Vec::new();
        for index in 0..MAX_FALLBACK_FIELDS {
            values.push((
                Text::<64>::new(&format!("field{index}")).unwrap(),
                Text::<512>::new("value").unwrap(),
            ));
        }
        assert!(FallbackObservation::new(values.clone()).is_ok());
        values.push((
            Text::<64>::new("overflow").unwrap(),
            Text::<512>::new("value").unwrap(),
        ));
        assert!(FallbackObservation::new(values).is_err());
    }
}
