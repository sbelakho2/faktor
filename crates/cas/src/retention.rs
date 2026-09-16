//! Referential-protection deletion for the content-addressed store.
//!
//! The retention plane (in `faktor-cloud`) decides WHICH artifact rows are
//! expired and eligible; this module is the last line of defense at the
//! storage layer: [`Cas::delete_blob_guarded`] asks a [`RetentionGuard`]
//! whether a digest is still referenced by recoverable durable state (an
//! open landing transaction's rollback blob, a verification record, a run
//! base) and REFUSES the deletion, typed, when it is. A guard error is a
//! refusal too (fail closed) — a broken reference scan can never lose
//! rollback material.
//!
//! Deleting an absent blob is idempotent ([`BlobDeleteOutcome::Absent`]), so
//! a crash between the blob removal and the artifact row update replays
//! cleanly.

use std::collections::BTreeMap;

use faktor_core::hash::FileHash;
use serde::{Deserialize, Serialize};

use crate::{Cas, CasError, CasResult};

/// What protects a digest from deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionKind {
    /// An open/recoverable landing transaction.
    IntegrationTxn,
    /// A durable verification record.
    VerificationRecord,
    /// A recorded run base snapshot.
    RunBase,
    /// An open durable edit transaction (its staged base content).
    OpenEditTxn,
}

impl ProtectionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProtectionKind::IntegrationTxn => "integration_txn",
            ProtectionKind::VerificationRecord => "verification_record",
            ProtectionKind::RunBase => "run_base",
            ProtectionKind::OpenEditTxn => "open_edit_txn",
        }
    }
}

/// One durable reference that protects a digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedReference {
    pub kind: ProtectionKind,
    pub reference: String,
    pub detail: String,
}

/// Typed reference-scan failure: the guard could not answer, so nothing may
/// be deleted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RetentionScanError {
    #[error("retention reference scan unavailable: {0}")]
    Unavailable(String),
    #[error("retention reference scan exceeded its bound: {0}")]
    BoundExceeded(String),
    #[error("retention reference scan found corrupt durable state: {0}")]
    Malformed(String),
}

/// The reference oracle of one deletion. Implementations scan durable state
/// (the session ledger) with their own explicit bound and return typed
/// errors — never an empty answer for a failed scan.
pub trait RetentionGuard: Send + Sync {
    /// `Ok(Some(_))` = the digest MUST NOT be deleted; `Ok(None)` = no live
    /// reference found; `Err(_)` = unknown (refuse).
    fn protect(&self, digest_hex: &str) -> Result<Option<ProtectedReference>, RetentionScanError>;
}

/// A guard for stores whose owners wire no recoverable state: it protects
/// nothing (the artifact-row checks upstream remain in force).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProtection;

impl RetentionGuard for NoProtection {
    fn protect(&self, _digest_hex: &str) -> Result<Option<ProtectedReference>, RetentionScanError> {
        Ok(None)
    }
}

/// The outcome of one guarded deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobDeleteOutcome {
    /// The blob was present and removed (`size` = stored compressed size).
    Deleted { size: u64 },
    /// The blob was already absent (idempotent replay).
    Absent,
    /// A live reference protects the digest; the blob is intact.
    Refused { reference: ProtectedReference },
}

/// Typed guarded-deletion failure.
#[derive(Debug, thiserror::Error)]
pub enum CasRetentionError {
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error("retention guard could not answer ({code}): {message}")]
    GuardUnavailable { code: &'static str, message: String },
}

fn guard_code(error: &RetentionScanError) -> &'static str {
    match error {
        RetentionScanError::Unavailable(_) => "unavailable",
        RetentionScanError::BoundExceeded(_) => "bound_exceeded",
        RetentionScanError::Malformed(_) => "malformed",
    }
}

impl Cas {
    /// Delete one blob WITHOUT a reference guard. The caller is responsible
    /// for proving nothing references the digest; the retention plane uses
    /// [`Cas::delete_blob_guarded`] instead.
    pub fn delete_blob(&self, hash: FileHash) -> CasResult<BlobDeleteOutcome> {
        let path = self.blob_path(hash);
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                let size = metadata.len();
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    // A concurrent delete won: the digest is absent either
                    // way, which is the postcondition.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.forget_verified(hash);
                        return Ok(BlobDeleteOutcome::Absent);
                    }
                    Err(e) => return Err(e.into()),
                }
                self.forget_verified(hash);
                Ok(BlobDeleteOutcome::Deleted { size })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.forget_verified(hash);
                Ok(BlobDeleteOutcome::Absent)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// THE guarded deletion: a digest referenced by recoverable durable
    /// state is never removed. The guard runs BEFORE any filesystem effect
    /// (including for an absent blob), so a scan outage is a refusal, not a
    /// silent success.
    pub fn delete_blob_guarded(
        &self,
        hash: FileHash,
        guard: &dyn RetentionGuard,
    ) -> Result<BlobDeleteOutcome, CasRetentionError> {
        let hex = hash.to_hex();
        match guard.protect(&hex) {
            Ok(Some(reference)) => return Ok(BlobDeleteOutcome::Refused { reference }),
            Ok(None) => {}
            Err(error) => {
                return Err(CasRetentionError::GuardUnavailable {
                    code: guard_code(&error),
                    message: error.to_string(),
                })
            }
        }
        Ok(self.delete_blob(hash)?)
    }

    /// Invalidate the advisory verified-cache entry of one digest (a deleted
    /// or replaced blob must never be answered from the LRU).
    pub(crate) fn forget_verified(&self, hash: FileHash) {
        let mut cache = self.verified.lock().unwrap_or_else(|p| p.into_inner());
        cache.retain(|entry| entry.hash != hash);
    }

    /// The blob digests currently stored, as a bounded map (tests and GC
    /// planning). Not a validity claim: use `verify_now` when the answer
    /// must mean "healthy".
    pub fn stored_digests(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        let Ok(read_dir) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if let Ok(inner) = std::fs::read_dir(&path) {
                for file in inner.flatten() {
                    let Some(fname) = file.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    let hex = format!("{name}{fname}");
                    if FileHash::from_hex(&hex).is_none() {
                        continue;
                    }
                    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                    out.insert(hex, size);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeGuard {
        protected: Mutex<BTreeMap<String, ProtectedReference>>,
        fail: Mutex<bool>,
    }

    impl Default for FakeGuard {
        fn default() -> Self {
            Self {
                protected: Mutex::new(BTreeMap::new()),
                fail: Mutex::new(false),
            }
        }
    }

    impl FakeGuard {
        fn protect_digest(&self, digest: &str, reference: &str) {
            self.protected.lock().unwrap().insert(
                digest.to_string(),
                ProtectedReference {
                    kind: ProtectionKind::IntegrationTxn,
                    reference: reference.to_string(),
                    detail: "live landing transaction".into(),
                },
            );
        }

        fn release(&self, digest: &str) {
            self.protected.lock().unwrap().remove(digest);
        }
    }

    impl RetentionGuard for FakeGuard {
        fn protect(
            &self,
            digest_hex: &str,
        ) -> Result<Option<ProtectedReference>, RetentionScanError> {
            if *self.fail.lock().unwrap() {
                return Err(RetentionScanError::Unavailable("store read failed".into()));
            }
            Ok(self.protected.lock().unwrap().get(digest_hex).cloned())
        }
    }

    fn cas() -> (tempfile::TempDir, Cas) {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path().join("cas")).unwrap();
        (dir, cas)
    }

    #[test]
    fn protected_rollback_blob_survives_and_is_deleted_after_release() {
        let (_dir, cas) = cas();
        let payload = b"authority-critical base payload";
        let hash = cas.put(payload).unwrap();
        let hex = hash.to_hex();
        let guard = FakeGuard::default();

        guard.protect_digest(&hex, "run-42");
        let refused = cas.delete_blob_guarded(hash, &guard).unwrap();
        assert_eq!(
            refused,
            BlobDeleteOutcome::Refused {
                reference: ProtectedReference {
                    kind: ProtectionKind::IntegrationTxn,
                    reference: "run-42".into(),
                    detail: "live landing transaction".into(),
                }
            }
        );
        assert_eq!(
            cas.get_verified_now(hash).unwrap(),
            payload,
            "the protected blob is byte-intact"
        );

        // Re-running the refusal changes nothing.
        assert!(matches!(
            cas.delete_blob_guarded(hash, &guard).unwrap(),
            BlobDeleteOutcome::Refused { .. }
        ));
        assert!(cas.has(hash));

        // Once the transaction is terminal the release deletes it.
        guard.release(&hex);
        match cas.delete_blob_guarded(hash, &guard).unwrap() {
            BlobDeleteOutcome::Deleted { size } => assert!(size > 0),
            other => panic!("expected a deletion, got {other:?}"),
        }
        assert!(!cas.has(hash));
        assert_eq!(cas.blob_count(), 0);
    }

    #[test]
    fn guard_outage_is_a_typed_refusal_and_never_deletes() {
        let (_dir, cas) = cas();
        let hash = cas.put(b"payload").unwrap();
        let guard = FakeGuard::default();
        *guard.fail.lock().unwrap() = true;
        let error = cas.delete_blob_guarded(hash, &guard).unwrap_err();
        assert!(matches!(
            error,
            CasRetentionError::GuardUnavailable {
                code: "unavailable",
                ..
            }
        ));
        assert!(cas.has(hash), "a scan outage must not delete");
        assert_eq!(cas.get_verified_now(hash).unwrap(), b"payload");
    }

    #[test]
    fn absent_blob_deletion_is_idempotent_and_cache_safe() {
        let (_dir, cas) = cas();
        let hash = cas.put(b"temporary").unwrap();
        cas.verify_now(&hash.to_hex()).unwrap();
        assert!(cas.has_cached_verified(&hash.to_hex()).unwrap());
        assert!(matches!(
            cas.delete_blob_guarded(hash, &NoProtection).unwrap(),
            BlobDeleteOutcome::Deleted { .. }
        ));
        assert!(!cas.has(hash));
        assert!(
            !cas.has_cached_verified(&hash.to_hex()).unwrap(),
            "the advisory cache is invalidated by deletion"
        );
        assert_eq!(
            cas.delete_blob_guarded(hash, &NoProtection).unwrap(),
            BlobDeleteOutcome::Absent,
            "replaying the deletion is idempotent"
        );
        assert_eq!(cas.blob_count(), 0);
        assert!(cas.stored_digests().is_empty());
    }

    #[test]
    fn the_guard_runs_even_for_an_absent_blob_so_a_refusal_is_never_a_silent_success() {
        let (_dir, cas) = cas();
        let hash =
            FileHash::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        let guard = FakeGuard::default();
        guard.protect_digest(&hash.to_hex(), "run-absent");
        assert!(matches!(
            cas.delete_blob_guarded(hash, &guard).unwrap(),
            BlobDeleteOutcome::Refused { .. }
        ));
        assert_eq!(
            cas.delete_blob_guarded(hash, &NoProtection).unwrap(),
            BlobDeleteOutcome::Absent
        );
    }
}
