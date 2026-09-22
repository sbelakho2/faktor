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
///
/// Persistence domain: the durable stores keep a generation in a SIGNED
/// SQLite `INTEGER` column (and order/compare generations in SQL), so the
/// valid domain is `1..=i64::MAX` — construction is fallible and rejects 0
/// as well as every value above `i64::MAX`, making every store-side
/// `as_i64()` mapping exact and monotone. There is no infallible raw-`u64`
/// constructor that could smuggle an out-of-domain value in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct JobGeneration(u64);

impl JobGeneration {
    /// The first generation of a job.
    pub const FIRST: JobGeneration = JobGeneration(1);

    /// The only raw-`u64` constructor: zero is not a real generation and a
    /// value above [`i64::MAX`] cannot round-trip through the signed SQLite
    /// column, so both are typed errors. Untrusted/decoded values (wire,
    /// storage, DTOs) must enter through here.
    pub fn try_new(raw: u64) -> Result<Self, WorkerError> {
        if raw == 0 {
            return Err(WorkerError::Malformed(
                "job generation cannot be 0: generations are 1-based".into(),
            ));
        }
        if raw > i64::MAX as u64 {
            return Err(WorkerError::Malformed(format!(
                "job generation {raw} is outside the signed-SQLite persistence domain \
                 (maximum {})",
                i64::MAX
            )));
        }
        Ok(Self(raw))
    }

    /// Decode one persisted signed-SQLite generation: a zero, negative or
    /// otherwise out-of-domain value is a typed malformed row, never a
    /// silent wrap into the domain.
    pub fn try_from_i64(raw: i64) -> Result<Self, WorkerError> {
        u64::try_from(raw)
            .map_err(|_| {
                WorkerError::Malformed(format!(
                    "persisted job generation {raw} is not a valid generation (must be >= 1)"
                ))
            })
            .and_then(Self::try_new)
    }

    /// The exact signed-SQLite mapping. Infallible because construction
    /// bounds the value to `1..=i64::MAX`.
    pub fn as_i64(self) -> i64 {
        self.0 as i64
    }

    /// The next generation (checked; a saturated generation refuses).
    pub fn next(self) -> Result<Self, WorkerError> {
        let next = self
            .0
            .checked_add(1)
            .ok_or_else(|| WorkerError::Malformed("job generation overflow".into()))?;
        Self::try_new(next)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for JobGeneration {
    type Error = WorkerError;

    fn try_from(raw: u64) -> Result<Self, WorkerError> {
        Self::try_new(raw)
    }
}

impl TryFrom<i64> for JobGeneration {
    type Error = WorkerError;

    fn try_from(raw: i64) -> Result<Self, WorkerError> {
        Self::try_from_i64(raw)
    }
}

impl<'de> Deserialize<'de> for JobGeneration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(d)?;
        Self::try_new(raw).map_err(D::Error::custom)
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
        assert_eq!(JobGeneration::try_new(1).unwrap(), JobGeneration::FIRST);
        // 0 and the values above the signed-SQLite domain are typed refusals.
        assert!(JobGeneration::try_new(0).is_err());
        assert!(JobGeneration::try_new(i64::MAX as u64 + 1).is_err());
        assert!(JobGeneration::try_new(u64::MAX).is_err());
        // The domain ceiling round-trips exactly and refuses to advance.
        let max = JobGeneration::try_new(i64::MAX as u64).unwrap();
        assert_eq!(max.as_u64(), i64::MAX as u64);
        assert_eq!(max.as_i64(), i64::MAX);
        assert!(max.next().is_err(), "the signed domain is the ceiling");
        // Decoding persisted values is bounded the same way.
        assert_eq!(JobGeneration::try_from_i64(7).unwrap().as_u64(), 7);
        assert!(JobGeneration::try_from_i64(0).is_err());
        assert!(JobGeneration::try_from_i64(-37).is_err());
        assert_eq!(
            JobGeneration::try_from_i64(i64::MAX).unwrap().as_i64(),
            i64::MAX
        );
        assert!(JobGeneration::try_from_i64(i64::MIN).is_err());
    }

    #[test]
    fn job_generation_deserialize_revalidates() {
        assert_eq!(
            serde_json::from_str::<JobGeneration>("1").unwrap(),
            JobGeneration::FIRST
        );
        assert_eq!(
            serde_json::from_str::<JobGeneration>(&i64::MAX.to_string())
                .unwrap()
                .as_i64(),
            i64::MAX
        );
        for hostile in [
            "0",
            "-1",
            "-37",
            "9223372036854775808",
            "18446744073709551615",
        ] {
            assert!(
                serde_json::from_str::<JobGeneration>(hostile).is_err(),
                "hostile generation {hostile} must be refused on decode"
            );
        }
        assert_eq!(serde_json::to_string(&JobGeneration::FIRST).unwrap(), "1");
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
