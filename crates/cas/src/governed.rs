//! Governor backend for one CAS root. The same physical root carries several
//! logical classes; the durable object index decides which blobs belong to
//! which class. Eviction verifies integrity before deleting and never deletes
//! a pinned/required object (the governor filters those first).

use std::sync::Arc;

use faktor_storage::{
    EvictionOutcome, GovernorError, GovernorResult, StorageBackend, StorageClass, StorageGovernor,
    StorageObject,
};

use crate::{Cas, CasError};

pub struct CasStorageBackend {
    cas: Cas,
    class: StorageClass,
    /// Class of blobs not (yet) named in the durable object index.
    default_class: StorageClass,
}

impl CasStorageBackend {
    pub fn new(cas: Cas, class: StorageClass, default_class: StorageClass) -> Self {
        Self {
            cas,
            class,
            default_class,
        }
    }
}

impl StorageBackend for CasStorageBackend {
    fn class(&self) -> StorageClass {
        self.class
    }

    fn scan(&self, governor: &StorageGovernor) -> GovernorResult<Vec<StorageObject>> {
        // ONE index load for the whole physical family, then classify in
        // memory. The old scan issued two point queries per stored blob, which
        // made reconciliation O(2n) round trips at 100k objects.
        let index = governor.family_index(self.default_class.family())?;
        let mut out = Vec::new();
        for (hex, bytes) in self.cas.stored_digests() {
            let (class, last_used_ms) = index.get(&hex).copied().unwrap_or((self.default_class, 0));
            if class != self.class {
                continue;
            }
            out.push(StorageObject {
                key: hex.clone(),
                bytes,
                last_used_ms,
                digest: Some(hex),
                required: false,
            });
        }
        Ok(out)
    }

    fn evict(
        &self,
        _governor: &StorageGovernor,
        object: &StorageObject,
    ) -> GovernorResult<EvictionOutcome> {
        let hash = faktor_core::hash::FileHash::from_hex(&object.key).ok_or_else(|| {
            GovernorError::Malformed(format!("CAS eviction key {:?} is not a digest", object.key))
        })?;
        match self.cas.evict_verified(hash) {
            Ok((bytes, verified)) => Ok(EvictionOutcome::Evicted {
                bytes,
                verified,
                digest: Some(object.key.clone()),
            }),
            Err(CasError::NotFound(_)) => Ok(EvictionOutcome::Absent),
            Err(error) => Err(GovernorError::Backend(format!(
                "CAS integrity verification refused eviction of {}: {error}",
                object.key
            ))),
        }
    }
}

/// A helper for hosts that hold only `Arc<Cas>`: install the backends for the
/// root's class (idempotent).
pub fn install_backends(cas: &Arc<Cas>) -> GovernorResult<()> {
    let Some(governor) = cas.governor() else {
        return Ok(());
    };
    // The CAS root can live outside the data directory; measure its backing
    // filesystem too (deduplicated with every other root on that volume).
    governor.register_capacity_root(cas.root());
    let classes: &[StorageClass] = match cas.storage_class() {
        StorageClass::EvidenceCas => &[StorageClass::EvidenceCas],
        _ => &[
            StorageClass::MainCas,
            StorageClass::CompactionArchive,
            StorageClass::TerminalCapture,
            StorageClass::Checkpoint,
        ],
    };
    for class in classes {
        governor.register_backend(Arc::new(CasStorageBackend::new(
            (**cas).clone(),
            *class,
            cas.storage_class(),
        )))?;
    }
    Ok(())
}
