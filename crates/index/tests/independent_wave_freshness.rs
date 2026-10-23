//! Independent final-wave probe: the index detects a watcher-missed
//! same-size/same-mtime source edit through its bounded EXACT recovery sweep.
//!
//! The file service is deliberately configured with a preflight cap of one
//! entry, so this multi-entry root is attached WITHOUT a recursive watcher.
//! No `request_build` is issued after the edit: the periodic reconciliation
//! must run the metadata shortlist and then the exact BLAKE3 round.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_index::{IndexService, ServiceConfig};
use faktor_store::Store;

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn write_same_size_same_mtime(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    let meta = fs::metadata(&path).unwrap();
    let previous_size = meta.len();
    let modified = meta.modified().unwrap();
    let accessed = meta.accessed().unwrap();
    fs::write(&path, text).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), previous_size);
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(modified)
            .set_accessed(accessed),
    )
    .unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.len(), previous_size);
    assert_eq!(after.modified().unwrap(), modified);
}

#[test]
fn a_watcher_missed_stealth_edit_is_recovered_and_served() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    write(&root, "Cargo.toml", "[workspace]\nmembers = [\"app\"]\n");
    write(
        &root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    let old = "pub fn alpha() -> i64 { 1 }\npub struct Beta {}\n";
    let new = "pub fn omega() -> i64 { 1 }\npub struct Beta {}\n";
    assert_eq!(old.len(), new.len());
    write(&root, "app/src/lib.rs", old);

    let store = Arc::new(Store::open(dir.path().join("store"), true).unwrap());
    let ws = store.create_workspace(root.to_str().unwrap()).unwrap();
    let fs = faktor_fs::WorkspaceFileService::with_watch_preflight_entries(1);
    let service = IndexService::open(store, dir.path().join("index-data"), fs).unwrap();
    service.set_config(ServiceConfig {
        poll: Duration::from_millis(20),
        fingerprint_interval: Duration::from_millis(25),
        build_lease: Duration::from_millis(200),
        ..ServiceConfig::default()
    });
    let view = service
        .ensure_ready(ws, Instant::now() + Duration::from_secs(120))
        .unwrap();
    assert_eq!(view.generation(), 1);
    {
        let index = view.index();
        let index = index.lock().unwrap();
        assert!(!index.symbol_lookup(ws, "alpha", 10).is_empty());
    }

    write_same_size_same_mtime(&root, "app/src/lib.rs", new);
    let deadline = Instant::now() + Duration::from_secs(120);
    while service.view(ws).map(|view| view.generation()).unwrap_or(0) <= 1 {
        assert!(
            Instant::now() < deadline,
            "the exact recovery sweep never superseded the generation"
        );
        service.reconcile_now(ws).unwrap();
        std::thread::sleep(Duration::from_millis(25));
    }
    let view = service
        .ensure_ready(ws, Instant::now() + Duration::from_secs(120))
        .unwrap();
    assert!(view.generation() > 1);
    let index = view.index();
    let index = index.lock().unwrap();
    assert!(
        !index.symbol_lookup(ws, "omega", 10).is_empty(),
        "the rebuilt index serves the new symbol"
    );
    assert!(
        index.symbol_lookup(ws, "alpha", 10).is_empty(),
        "the stale symbol is gone"
    );
    assert!(index
        .symbols_for(ws, "app/src/lib.rs")
        .iter()
        .any(|symbol| symbol.name == "omega"));
}
