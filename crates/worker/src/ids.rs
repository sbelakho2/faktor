//! Typed worker-plane identities.
//!
//! Every id is a distinct type with a strict shape (bounded printable ASCII
//! without whitespace, quotes or backslashes); deserialization re-validates,
//! so a hostile DTO can never smuggle an invalid identity into the domain.
//! Registration tokens reuse the control-plane [`faktor_cloud::SecretToken`]
//! / [`faktor_cloud::TokenHash`] discipline: revealed once at mint, stored
//! only as a SHA-256 hash.

use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::WorkerError;

/// Bound on any worker-plane id.
pub const MAX_ID_BYTES: usize = 128;
/// Bound on one job key (the caller's idempotency identity of a job).
pub const MAX_JOB_KEY_BYTES: usize = 256;

fn validate_id(kind: &str, value: &str) -> Result<(), WorkerError> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(WorkerError::Malformed(format!(
            "{kind} id must be 1..={MAX_ID_BYTES} bytes"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
    {
        return Err(WorkerError::Malformed(format!(
            "{kind} id must be printable ASCII without whitespace, quotes or backslashes"
        )));
    }
    Ok(())
}

macro_rules! string_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(raw: impl Into<String>) -> Result<Self, WorkerError> {
                let raw = raw.into();
                validate_id($kind, &raw)?;
                Ok(Self(raw))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::try_new(raw).map_err(D::Error::custom)
            }
        }
    };
}

string_id!(WorkerId, "worker");
string_id!(WorkerLeaseId, "worker lease");
string_id!(ExecutionJobId, "execution job");
string_id!(WorkerTokenLabel, "worker token label");

/// One immutable job generation number, 1-based. Generation 0 never exists:
/// a generation is minted by the scheduler and never renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobGeneration(pub u64);

impl JobGeneration {
    /// The first generation of a job.
    pub const FIRST: JobGeneration = JobGeneration(1);

    /// The next generation (checked; a saturated generation refuses).
    pub fn next(self) -> Result<Self, WorkerError> {
        self.0
            .checked_add(1)
            .map(JobGeneration)
            .ok_or_else(|| WorkerError::Malformed("job generation overflow".into()))
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for JobGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One job key: the caller-supplied idempotency identity of a job inside one
/// organization. A re-schedule with the same key returns the SAME job root
/// (its existing immutable generation) — a replay never mints a new
/// generation behind the caller's back.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct JobKey(String);

impl JobKey {
    pub fn try_new(raw: impl Into<String>) -> Result<Self, WorkerError> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > MAX_JOB_KEY_BYTES {
            return Err(WorkerError::Malformed(format!(
                "job key must be 1..={MAX_JOB_KEY_BYTES} bytes"
            )));
        }
        if !raw.as_bytes().iter().all(|b| b.is_ascii_graphic()) {
            return Err(WorkerError::Malformed(
                "job key must be printable ASCII without whitespace".into(),
            ));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for JobKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_new(raw).map_err(D::Error::custom)
    }
}

/// Mint one opaque worker-plane id with its stable prefix.
pub(crate) fn mint(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_bounded_and_revalidated_on_deserialize() {
        assert!(WorkerId::try_new("").is_err());
        assert!(WorkerId::try_new("has space").is_err());
        assert!(WorkerId::try_new("a".repeat(MAX_ID_BYTES + 1)).is_err());
        assert!(WorkerId::try_new("wrk_01h").is_ok());
        assert!(serde_json::from_str::<WorkerId>("\"bad id\"").is_err());
        assert_eq!(
            serde_json::from_str::<WorkerId>("\"wrk_1\"")
                .unwrap()
                .as_str(),
            "wrk_1"
        );
    }

    #[test]
    fn job_generation_is_checked() {
        assert_eq!(JobGeneration::FIRST.as_u64(), 1);
        assert_eq!(JobGeneration::FIRST.next().unwrap().as_u64(), 2);
        assert!(JobGeneration(u64::MAX).next().is_err());
    }

    #[test]
    fn job_keys_are_bounded() {
        assert!(JobKey::try_new("").is_err());
        assert!(JobKey::try_new("a b").is_err());
        assert!(JobKey::try_new("x".repeat(MAX_JOB_KEY_BYTES + 1)).is_err());
        assert_eq!(
            JobKey::try_new("sess-1/run-1").unwrap().as_str(),
            "sess-1/run-1"
        );
    }
}
