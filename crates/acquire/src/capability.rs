//! Connector capabilities and credential availability.
//!
//! Connectors advertise, per mechanism, how well they can serve each
//! requested field. The planner never hard-codes a site: a mechanism is
//! eligible iff it supports every requested field and its credentials are
//! usable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::mechanism::{AcquisitionMechanism, MECHANISM_PRECEDENCE};
use crate::request::{RequestedField, RequestedFields};

/// How well a mechanism can serve something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityLevel {
    /// Cannot serve it at all.
    None,
    /// Can serve it partially (usable, but the plan is flagged).
    Partial,
    /// Fully supported.
    Full,
}

/// What one connector advertises.
///
/// `fields` maps mechanism -> field -> level. A field with no explicit entry
/// inherits the mechanism's own level, so a `Full` mechanism covers
/// unlisted fields and a `Partial` mechanism covers them partially.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorCapabilities {
    /// Per-mechanism level.
    pub mechanisms: BTreeMap<AcquisitionMechanism, CapabilityLevel>,
    /// Per-mechanism, per-field overrides.
    pub fields: BTreeMap<AcquisitionMechanism, BTreeMap<RequestedField, CapabilityLevel>>,
    /// The largest batch the connector accepts, when it advertises one.
    pub max_batch: Option<u32>,
}

impl ConnectorCapabilities {
    /// No capabilities at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Advertise one mechanism at `level`.
    pub fn with_mechanism(
        mut self,
        mechanism: AcquisitionMechanism,
        level: CapabilityLevel,
    ) -> Self {
        self.mechanisms.insert(mechanism, level);
        self
    }

    /// Override one (mechanism, field) level.
    pub fn with_field(
        mut self,
        mechanism: AcquisitionMechanism,
        field: RequestedField,
        level: CapabilityLevel,
    ) -> Self {
        self.fields
            .entry(mechanism)
            .or_default()
            .insert(field, level);
        self
    }

    /// Advertise a mechanism as `Full` for everything.
    pub fn with_full(mut self, mechanism: AcquisitionMechanism) -> Self {
        self.mechanisms.insert(mechanism, CapabilityLevel::Full);
        self
    }

    /// The mechanism's own level (`None` when absent).
    pub fn mechanism_level(&self, mechanism: AcquisitionMechanism) -> CapabilityLevel {
        self.mechanisms
            .get(&mechanism)
            .copied()
            .unwrap_or(CapabilityLevel::None)
    }

    /// The level for one field, inheriting the mechanism level when there is
    /// no explicit entry.
    pub fn field_level(
        &self,
        mechanism: AcquisitionMechanism,
        field: RequestedField,
    ) -> CapabilityLevel {
        self.fields
            .get(&mechanism)
            .and_then(|fields| fields.get(&field))
            .copied()
            .unwrap_or_else(|| self.mechanism_level(mechanism))
    }

    /// The minimum level over every requested field.
    pub fn coverage(
        &self,
        mechanism: AcquisitionMechanism,
        fields: &RequestedFields,
    ) -> CapabilityLevel {
        let mut level = self.mechanism_level(mechanism);
        for field in fields.iter() {
            level = level.min(self.field_level(mechanism, field));
        }
        level
    }

    /// Whether the mechanism can serve every requested field.
    pub fn eligible(&self, mechanism: AcquisitionMechanism, fields: &RequestedFields) -> bool {
        self.coverage(mechanism, fields) != CapabilityLevel::None
    }

    /// The first requested field this mechanism cannot serve at all.
    pub fn first_unsupported(
        &self,
        mechanism: AcquisitionMechanism,
        fields: &RequestedFields,
    ) -> Option<RequestedField> {
        fields
            .iter()
            .find(|field| self.field_level(mechanism, *field) == CapabilityLevel::None)
    }

    /// Mechanisms that can serve every requested field, in precedence order.
    pub fn eligible_mechanisms(&self, fields: &RequestedFields) -> Vec<AcquisitionMechanism> {
        MECHANISM_PRECEDENCE
            .into_iter()
            .filter(|mechanism| self.eligible(*mechanism, fields))
            .collect()
    }
}

/// Whether credentials exist for a mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    /// The mechanism needs no credentials.
    NotRequired,
    /// Credentials exist and are usable.
    Available,
    /// Credentials are missing.
    Missing,
    /// Credentials exist but are expired/revoked.
    Expired,
}

impl CredentialStatus {
    /// Whether a mechanism with this status may be used.
    pub const fn usable(self) -> bool {
        matches!(self, Self::NotRequired | Self::Available)
    }
}

/// Credential availability per mechanism (never the credential values).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialAvailability {
    /// Per-mechanism status.
    pub per_mechanism: BTreeMap<AcquisitionMechanism, CredentialStatus>,
}

impl CredentialAvailability {
    /// Everything is `NotRequired`.
    pub fn none() -> Self {
        Self::default()
    }

    /// Set one mechanism's status.
    pub fn with_status(
        mut self,
        mechanism: AcquisitionMechanism,
        status: CredentialStatus,
    ) -> Self {
        self.per_mechanism.insert(mechanism, status);
        self
    }

    /// The status of one mechanism (`NotRequired` by default).
    pub fn status(&self, mechanism: AcquisitionMechanism) -> CredentialStatus {
        self.per_mechanism
            .get(&mechanism)
            .copied()
            .unwrap_or(CredentialStatus::NotRequired)
    }

    /// Whether the mechanism's credentials are usable.
    pub fn usable(&self, mechanism: AcquisitionMechanism) -> bool {
        self.status(mechanism).usable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_levels_inherit_and_override() {
        let caps = ConnectorCapabilities::none()
            .with_full(AcquisitionMechanism::OfficialApi)
            .with_field(
                AcquisitionMechanism::OfficialApi,
                RequestedField::BulkSet,
                CapabilityLevel::None,
            );
        assert_eq!(
            caps.field_level(AcquisitionMechanism::OfficialApi, RequestedField::Identity),
            CapabilityLevel::Full
        );
        assert_eq!(
            caps.field_level(AcquisitionMechanism::OfficialApi, RequestedField::BulkSet),
            CapabilityLevel::None
        );
        let fields = RequestedFields::of([RequestedField::Identity, RequestedField::BulkSet]);
        assert!(!caps.eligible(AcquisitionMechanism::OfficialApi, &fields));
        assert_eq!(
            caps.first_unsupported(AcquisitionMechanism::OfficialApi, &fields),
            Some(RequestedField::BulkSet)
        );
    }

    #[test]
    fn eligibility_requires_full_coverage_of_every_requested_field() {
        let caps = ConnectorCapabilities::none()
            .with_mechanism(AcquisitionMechanism::DirectHttp, CapabilityLevel::Partial)
            .with_field(
                AcquisitionMechanism::DirectHttp,
                RequestedField::Availability,
                CapabilityLevel::None,
            );
        assert!(caps.eligible(
            AcquisitionMechanism::DirectHttp,
            &RequestedFields::of([RequestedField::Descriptive])
        ));
        assert!(!caps.eligible(
            AcquisitionMechanism::DirectHttp,
            &RequestedFields::of([RequestedField::Availability])
        ));
        assert_eq!(
            caps.coverage(
                AcquisitionMechanism::DirectHttp,
                &RequestedFields::of([RequestedField::Descriptive])
            ),
            CapabilityLevel::Partial
        );
    }

    #[test]
    fn credential_defaults_are_not_required_and_expired_is_unusable() {
        let creds = CredentialAvailability::none();
        assert!(creds.usable(AcquisitionMechanism::Dom));
        let creds = creds
            .with_status(AcquisitionMechanism::OfficialApi, CredentialStatus::Expired)
            .with_status(
                AcquisitionMechanism::BrowserNetwork,
                CredentialStatus::Available,
            );
        assert!(!creds.usable(AcquisitionMechanism::OfficialApi));
        assert!(creds.usable(AcquisitionMechanism::BrowserNetwork));
    }
}
