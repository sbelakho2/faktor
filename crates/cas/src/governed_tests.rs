use std::sync::Arc;

use faktor_storage::{StorageClass, StorageGovernor, StorageGovernorConfig};

use crate::{Cas, CasError};

fn noisy(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn governor(dir: &std::path::Path, class: StorageClass, class_bytes: u64) -> Arc<StorageGovernor> {
    let config = StorageGovernorConfig {
        total_budget_bytes: 10_000_000,
        watermark_permille: 900,
        reservation_lease_ms: 60_000,
        quotas: StorageClass::ALL
            .into_iter()
            .map(|candidate| {
                (
                    candidate,
                    if candidate == class {
                        class_bytes
                    } else {
                        1_000_000
                    },
                )
            })
            .collect(),
    }
    .validated()
    .unwrap();
    Arc::new(StorageGovernor::open(dir, config).unwrap())
}

fn governed(dir: &std::path::Path, class: StorageClass, bytes: u64) -> (Cas, Arc<StorageGovernor>) {
    let governor = governor(dir, class, bytes);
    let mut cas = Cas::open_in_class(dir.join("cas"), class).unwrap();
    cas.attach_governor(governor.clone(), class);
    (cas, governor)
}

#[test]
fn cas_put_refuses_before_publishing_over_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (cas, governor) = governed(dir.path(), StorageClass::MainCas, 1_000);
    let error = cas.put(&noisy(2_000)).unwrap_err();
    assert!(matches!(error, CasError::Governor(_)), "{error}");
    assert_eq!(cas.blob_count(), 0, "a refused put must publish nothing");
    assert_eq!(governor.counters().unwrap().refusals, 1);
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
    // The boundary is admitted and indexed.
    let hash = cas.put_bounded(&vec![1u8; 900], 900).unwrap();
    assert!(cas.has(hash));
    assert_eq!(
        governor.object_class_of_digest(&hash.to_hex()).unwrap(),
        Some(StorageClass::MainCas)
    );
}

#[test]
fn cas_eviction_verifies_integrity_before_delete() {
    let dir = tempfile::tempdir().unwrap();
    let (cas, governor) = governed(dir.path(), StorageClass::MainCas, 10_000_000);
    let healthy = cas.put(b"healthy eviction candidate").unwrap();
    let corrupt = cas.put(b"corrupt eviction candidate").unwrap();
    let path = cas.blob_path(corrupt);
    std::fs::write(&path, b"not a zstd frame at all").unwrap();
    let error = governor
        .evict_object(StorageClass::MainCas, &corrupt.to_hex())
        .unwrap_err();
    assert!(
        error.to_string().contains("integrity verification refused"),
        "{error}"
    );
    assert!(path.exists(), "unverified bytes must never be deleted");
    let freed = governor
        .evict_object(StorageClass::MainCas, &healthy.to_hex())
        .unwrap();
    assert!(freed > 0);
    assert!(!cas.has(healthy));
    let index = governor.eviction_index().unwrap();
    assert!(index
        .iter()
        .any(|(class, key, _, verified)| class == "main_cas"
            && key == &healthy.to_hex()
            && *verified));
    // An absent object is an idempotent replay, not corruption.
    assert_eq!(
        governor
            .evict_object(StorageClass::MainCas, &healthy.to_hex())
            .unwrap(),
        0
    );
}

#[test]
fn archive_cas_eviction_records_a_durable_loss_marker_with_verified_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let (cas, governor) = governed(dir.path(), StorageClass::MainCas, 10_000_000);
    let chunk = cas
        .put_in_class(StorageClass::CompactionArchive, "archive", b"chunk")
        .unwrap();
    assert_eq!(
        governor.object_class_of_digest(&chunk.to_hex()).unwrap(),
        Some(StorageClass::CompactionArchive)
    );
    let freed = governor
        .evict_object(StorageClass::CompactionArchive, &chunk.to_hex())
        .unwrap();
    assert!(freed > 0);
    assert!(!cas.has(chunk));
    let markers = governor
        .loss_markers(Some(StorageClass::CompactionArchive))
        .unwrap();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].0, chunk.to_hex());
    assert_eq!(markers[0].2, "archive_evicted");
    assert!(governor
        .eviction_index()
        .unwrap()
        .iter()
        .any(|(_, key, _, verified)| key == &chunk.to_hex() && *verified));
}

#[test]
fn governed_cas_restart_reconciles_accounting_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let stored_size;
    {
        let (cas, _governor) = governed(dir.path(), StorageClass::MainCas, 1_000_000);
        let hash = cas.put(b"restart accounting").unwrap();
        stored_size = cas.stored_size(hash).unwrap();
    }
    let governor = StorageGovernor::open_existing(dir.path()).unwrap();
    let cas = Cas::open_in_class(dir.path().join("cas"), StorageClass::MainCas).unwrap();
    assert!(cas.governor_attached());
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.per_class["main_cas"].bytes, stored_size);
    assert_eq!(metrics.per_class["main_cas"].objects, 1);
    assert!(metrics.total_bytes >= stored_size);
}

#[test]
fn governed_dedup_hit_costs_no_new_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (cas, governor) = governed(dir.path(), StorageClass::MainCas, 1_000);
    let bytes = vec![7u8; 800];
    let first = cas.put(&bytes).unwrap();
    let used = governor.metrics().unwrap().total_bytes;
    let second = cas.put(&bytes).unwrap();
    assert_eq!(first, second);
    assert_eq!(governor.metrics().unwrap().total_bytes, used);
    assert!(!governor
        .reserve(StorageClass::MainCas, "does-not-matter", 10_000)
        .is_ok());
}

#[test]
fn crash_after_rename_before_commit_is_reconciled_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let (cas, governor) = governed(dir.path(), StorageClass::CompactionArchive, 1_000_000);
    let payload = vec![9u8; 4_000];
    let expected = faktor_core::hash::FileHash::from(blake3::hash(&payload).into());
    cas.crash_arm(crate::CrashArm {
        point: "cas_renamed",
        ordinal: 0,
    });
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = cas.put_in_class(StorageClass::CompactionArchive, "crashed", &payload);
    }));
    assert!(crashed.is_err(), "the armed seam must trip");
    // The atomic rename published the blob, but the reservation was never
    // committed: accounting is reconstructed from disk, never trusted.
    assert!(cas.blob_path(expected).exists());
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
    let report = governor.reconcile().unwrap();
    assert_eq!(report.removed, 0);
    let usage = governor.metrics().unwrap();
    assert_eq!(
        usage.per_class["compaction_archive"].bytes,
        cas.stored_size(expected).unwrap()
    );
    assert!(usage.per_class["compaction_archive"].bytes > 0);
    assert_eq!(
        governor.object_class_of_digest(&expected.to_hex()).unwrap(),
        Some(StorageClass::CompactionArchive)
    );
    assert_eq!(cas.put(&payload).unwrap(), expected, "dedup after recovery");
}

#[test]
fn identical_content_in_main_and_evidence_roots_is_two_governed_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let governor = governor(dir.path(), StorageClass::MainCas, 1_000_000);
    let mut main = Cas::open_in_class(dir.path().join("cas"), StorageClass::MainCas).unwrap();
    main.attach_governor(governor.clone(), StorageClass::MainCas);
    let mut evidence =
        Cas::open_in_class(dir.path().join("evidence-cas"), StorageClass::EvidenceCas).unwrap();
    evidence.attach_governor(governor.clone(), StorageClass::EvidenceCas);
    let payload = b"same bytes, two physical roots";
    let from_main = main.put(payload).unwrap();
    let from_evidence = evidence.put(payload).unwrap();
    assert_eq!(from_main, from_evidence, "content addresses agree");
    assert_eq!(
        governor
            .object_class_of_digest_in_family(&from_main.to_hex(), "main")
            .unwrap(),
        Some(StorageClass::MainCas)
    );
    assert_eq!(
        governor
            .object_class_of_digest_in_family(&from_main.to_hex(), "evidence")
            .unwrap(),
        Some(StorageClass::EvidenceCas)
    );
    let metrics = governor.metrics().unwrap();
    assert_eq!(
        metrics.per_class["main_cas"].bytes,
        main.stored_size(from_main).unwrap()
    );
    assert_eq!(
        metrics.per_class["evidence_cas"].bytes,
        evidence.stored_size(from_evidence).unwrap()
    );
    assert_eq!(
        metrics.total_bytes,
        main.stored_size(from_main).unwrap() + evidence.stored_size(from_evidence).unwrap(),
        "the two physical copies are both counted against the hard budget"
    );
    governor
        .evict_object(StorageClass::MainCas, &from_main.to_hex())
        .unwrap();
    assert!(evidence.has(from_evidence), "the evidence copy survives");
}

#[test]
fn evidence_class_backend_is_the_evidence_root() {
    let dir = tempfile::tempdir().unwrap();
    let governor = governor(dir.path(), StorageClass::EvidenceCas, 10_000);
    let mut cas = Cas::open_in_class(
        dir.path().join("store").join("evidence-cas"),
        StorageClass::EvidenceCas,
    )
    .unwrap();
    cas.attach_governor(governor.clone(), StorageClass::EvidenceCas);
    let hash = cas.put(b"evidence backing").unwrap();
    assert_eq!(
        governor
            .object_class_of_digest_in_family(&hash.to_hex(), "evidence")
            .unwrap(),
        Some(StorageClass::EvidenceCas)
    );
    let objects = governor
        .backend(StorageClass::EvidenceCas)
        .unwrap()
        .scan(&governor)
        .unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].key, hash.to_hex());
    governor
        .pin(StorageClass::EvidenceCas, &hash.to_hex(), "evidence")
        .unwrap();
    assert_eq!(
        governor
            .evict_object(StorageClass::EvidenceCas, &hash.to_hex())
            .unwrap(),
        0,
        "required evidence is never an eviction candidate"
    );
    assert!(cas.has(hash));
    governor
        .unpin(StorageClass::EvidenceCas, &hash.to_hex())
        .unwrap();
    assert!(
        governor
            .evict_object(StorageClass::EvidenceCas, &hash.to_hex())
            .unwrap()
            > 0
    );
}
