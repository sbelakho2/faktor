//! Legacy pre-coverage migration (audit item 4): a generation envelope that
//! predates the coverage members is LEGACY-UNKNOWN — served as stale data,
//! marked LegacyUnknown freshness, fallback-eligible on misses, and rebuilt
//! (resumably, with NO unrelated dirtiness event) until complete. A truly
//! complete new-format generation still reports complete.
//!
//! This is a CHILD module of [`crate::service`] on purpose: the migration
//! proof must observe the instant between "the legacy generation loaded" and
//! "the resumable rebuild published its first batch", which only the
//! service's own accessors expose.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_fs::WorkspaceFileService;
use faktor_store::Store;
use tempfile::TempDir;

use super::*;

use crate::coverage::LEGACY_UNKNOWN_REASON;
use crate::generation::GenerationFile;
use crate::state::WorkspaceIndexState as St;
use crate::WorkspaceIndex;

/// The old one-shot build's cap: the legacy generation contains exactly the
/// first 4,000 files.
const LEGACY_FILES: usize = 4_000;
/// The repository actually has 4,500 files; the target symbol lives in the
/// LAST one, absent from the legacy generation.
const TOTAL_FILES: usize = 4_500;
const NEEDLE: &str = "legacy_migration_needle";
const NEEDLE_FILE: &str = "f04499.rs";
const DEADLINE: Duration = Duration::from_secs(300);

struct Env {
    _dir: TempDir,
    repo: PathBuf,
    store_root: PathBuf,
    data_root: PathBuf,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let store_root = dir.path().join("store");
    let data_root = dir.path().join("index_data");
    Env {
        _dir: dir,
        repo,
        store_root,
        data_root,
    }
}

fn write(repo: &Path, rel: &str, content: &str) {
    let p = repo.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn fast_cfg() -> ServiceConfig {
    ServiceConfig {
        poll: Duration::from_millis(25),
        fingerprint_interval: Duration::from_millis(50),
        build_lease: Duration::from_millis(200),
        ..ServiceConfig::default()
    }
}

/// Same process-global serial as the other heavy index fixtures (seams and
/// load-sensitive IO).
fn serial() -> std::sync::MutexGuard<'static, ()> {
    crate::cold::TEST_SERIAL
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[test]
fn legacy_generation_is_unknown_served_fallback_then_fully_rebuilt() {
    let _serial = serial();
    let env = env();
    // The disk: 4,500 files, the needle only in file 4,499 (1-based 4,500).
    let mut legacy_fingerprint: Vec<FingerprintEntry> = Vec::new();
    for i in 0..TOTAL_FILES {
        let body = if i == TOTAL_FILES - 1 {
            format!("pub fn {NEEDLE}() -> u64 {{ 4500 }}\n")
        } else {
            // Deliberately no token overlap with the needle: the fallback
            // assertion below must prove the DIRECT read served the miss,
            // not incidental lexical matches of the stale generation.
            format!("pub fn bulk_{i:05}() -> u64 {{ {i} }}\n")
        };
        let rel = format!("f{i:05}.rs");
        write(&env.repo, &rel, &body);
        if i < LEGACY_FILES {
            legacy_fingerprint.push(FingerprintEntry {
                path: rel,
                size: body.len() as u64,
                modified_ms: (i as i64) + 1,
            });
        }
    }
    let store = Arc::new(Store::open(&env.store_root, true).unwrap());
    let ws = store.create_workspace(env.repo.to_str().unwrap()).unwrap();
    let svc = IndexService::open(
        store.clone(),
        env.data_root.clone(),
        WorkspaceFileService::new(),
    )
    .unwrap();
    svc.set_config(fast_cfg());

    // Serialize the OLD format exactly: capture a new-format envelope for the
    // first 4,000 files, then strip the coverage members a pre-coverage
    // build never wrote.
    let mut idx = WorkspaceIndex::new();
    for i in 0..LEGACY_FILES {
        let rel = format!("f{i:05}.rs");
        let bytes = fs::read(env.repo.join(&rel)).unwrap();
        idx.index_file(ws, Path::new(&rel), &bytes, (i as i64) + 1)
            .unwrap();
    }
    let envelope = GenerationFile::capture(ws.raw(), 1, &idx, legacy_fingerprint);
    let mut value: serde_json::Value =
        serde_json::from_slice(&envelope.to_bytes().unwrap()).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.remove("coverage");
    obj.remove("cursor");
    obj.remove("fingerprint_coverage");
    let gen_path = generation_file_path(&env.data_root, ws, 1);
    fs::create_dir_all(gen_path.parent().unwrap()).unwrap();
    fs::write(&gen_path, serde_json::to_vec(&value).unwrap()).unwrap();
    // The durable row names it Ready{1}, exactly like a daemon upgraded in
    // place; no coverage metadata exists anywhere for it.
    store
        .index_state_put(
            ws,
            &St::Ready { generation: 1 }.to_row_json(),
            1,
            JOURNAL_READY,
        )
        .unwrap();

    // Opening the pre-coverage generation keeps it SERVED as stale data.
    svc.attach(ws).unwrap();
    svc.load_content(ws).unwrap();
    {
        let view = svc.view(ws).expect("legacy generation stays serveable");
        assert_eq!(view.generation(), 1);
        assert!(view.coverage().is_legacy_unknown(), "{:?}", view.coverage());
        assert_eq!(
            view.coverage().truncated_reason.as_deref(),
            Some(LEGACY_UNKNOWN_REASON)
        );
        assert!(!view.coverage().complete);
        assert!(view.coverage().needs_fallback_on_miss());
        assert!(view.fingerprint_coverage().is_legacy_unknown());
        assert!(!view.fingerprint_coverage().complete, "never fully clean");
        assert_eq!(view.freshness(), EvidenceFreshness::LegacyUnknown);
        assert!(view.miss_needs_fallback(), "a legacy miss must fall back");
        assert!(
            view.index()
                .lock()
                .unwrap()
                .symbol_lookup(ws, NEEDLE, 4)
                .is_empty(),
            "the needle is in file 4,500, absent from the legacy content"
        );
        let snapshot = svc.coverage_snapshot(ws).unwrap();
        assert!(snapshot.serving);
        assert_eq!(snapshot.published_generation, Some(1));
        // The snapshot is the STRICT IDE-panel wire shape: legacy-unknown
        // maps to `partial` (never current) and the legacy marker surfaces
        // in the coverage record itself.
        assert_eq!(snapshot.freshness, EvidenceFreshness::Partial);
        assert!(snapshot.coverage.is_legacy_unknown());
        assert_eq!(
            snapshot.coverage.truncated_reason.as_deref(),
            Some(LEGACY_UNKNOWN_REASON)
        );
        assert_eq!(
            snapshot.fingerprint.truncated_reason.as_deref(),
            Some(LEGACY_UNKNOWN_REASON)
        );
        assert!(!snapshot.coverage.complete && !snapshot.fingerprint.complete);
    }

    // The bounded direct filesystem fallback serves the missed symbol while
    // the rebuild has not completed.
    let provider = svc.cold_provider(ws).expect("cold provider");
    let cold = provider.evidence(&crate::cold::ColdQuery {
        prompt: NEEDLE.to_string(),
        changed_files: vec![NEEDLE_FILE.to_string()],
        referenced_paths: Vec::new(),
        failures: Vec::new(),
    });
    assert!(
        cold.hits.iter().any(|h| h.snippet.contains(NEEDLE)),
        "the bounded fallback must serve the missed file: {cold:?}"
    );

    // The ABSENT coverage alone schedules the resumable full rebuild of the
    // SAME generation: no watcher event, no `request_build`, no file touched.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let _ = svc.reconcile_now(ws);
        if svc
            .view(ws)
            .map(|v| v.coverage().complete && v.fingerprint_coverage().complete)
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "legacy generation never rebuilt: state={:?} cov={:?} fp={:?}",
            svc.state(ws),
            svc.coverage(ws),
            svc.fingerprint_coverage(ws)
        );
    }
    let view = svc.view(ws).unwrap();
    assert_eq!(
        view.generation(),
        1,
        "the rebuild stays on the same generation"
    );
    assert_eq!(view.freshness(), EvidenceFreshness::Current);
    assert!(!view.miss_needs_fallback());
    assert!(
        !view
            .index()
            .lock()
            .unwrap()
            .symbol_lookup(ws, NEEDLE, 4)
            .is_empty(),
        "the needle must be indexed after the resumable full rebuild"
    );
    let snapshot = svc.coverage_snapshot(ws).unwrap();
    assert!(snapshot.coverage.complete, "{snapshot:?}");
    assert!(snapshot.fingerprint.complete, "{snapshot:?}");
    assert_eq!(snapshot.coverage.truncated_reason, None);
    assert_eq!(snapshot.freshness, EvidenceFreshness::Current);
    // No unrelated dirtiness event was involved: the journal has no dirty
    // hop, only continuation claims of the same generation.
    let log: Vec<(String, i64)> = store
        .index_state_log(ws, 100_000)
        .unwrap()
        .into_iter()
        .map(|r| (r.kind, r.generation))
        .collect();
    assert!(
        log.iter().all(|(kind, _)| kind != "dirty"),
        "the legacy rebuild must not need a dirtiness event: {log:?}"
    );
    assert!(
        log.iter().any(|(kind, _)| kind == "continue"),
        "the resumable continuation must be journaled: {log:?}"
    );
    drop(svc);
    drop(store);
}
