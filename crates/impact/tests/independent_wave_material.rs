//! Independent final-wave probe: the impact graph's own bounded exact
//! recovery scan finds a watcher-missed same-size/same-mtime dependency edit,
//! and a manifest-only edit changes the material revision even when every
//! source is byte-identical.

use std::fs;
use std::path::Path;

use faktor_impact::{
    AffectedOptions, ChangedTarget, ImpactSession, MaterialGeneration, RefreshPolicy,
};

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

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"b\", \"c\"]\n",
    );
    for name in ["b", "c"] {
        write(
            root,
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        );
        write(
            root,
            &format!("{name}/src/lib.rs"),
            "pub fn target() -> u32 { 1 }\n",
        );
    }
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nb = { path = \"../b\" }\nc = { path = \"../c\" }\n",
    );
    write(
        root,
        "app/src/lib.rs",
        "use b::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    dir
}

fn affected_has_dispatch(session: &ImpactSession, path: &str, symbol: &str) -> bool {
    session
        .affected(
            &[ChangedTarget::symbol(path, symbol)],
            &AffectedOptions {
                crate_tests: false,
                ..AffectedOptions::default()
            },
        )
        .unwrap()
        .affected
        .iter()
        .any(|node| node.name == "dispatch")
}

#[test]
fn recovery_finds_a_stealth_dependency_edit_and_a_manifest_edit_invalidates_material() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    assert!(affected_has_dispatch(&session, "b/src/lib.rs", "target"));

    write_same_size_same_mtime(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    let mut seeds = Vec::new();
    for _ in 0..32 {
        let scan = session.scan_material_changes().unwrap();
        seeds = scan.seeds;
        if seeds.iter().any(|path| path == "app/src/lib.rs") {
            break;
        }
    }
    assert!(
        seeds.iter().any(|path| path == "app/src/lib.rs"),
        "the exact recovery scan must find the stealth edit: {seeds:?}"
    );
    let generation = MaterialGeneration::new();
    generation.observe(seeds);
    session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert!(
        affected_has_dispatch(&session, "c/src/lib.rs", "target"),
        "the new dependency edge is served"
    );
    assert!(
        !affected_has_dispatch(&session, "b/src/lib.rs", "target"),
        "the stale dependency edge is gone"
    );

    let before_revision = session.graph().graph().revision.clone();
    let before_digest = session.graph().graph().digest.clone();
    write_same_size_same_mtime(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.2.0\"\n\n[dependencies]\nb = { path = \"../b\" }\nc = { path = \"../c\" }\n",
    );
    let scan = session.scan_material_changes().unwrap();
    assert!(
        scan.seeds.iter().any(|path| path == "app/Cargo.toml"),
        "a manifest-only edit must be material: {scan:?}"
    );
    let generation = MaterialGeneration::new();
    generation.observe(scan.seeds);
    session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert_ne!(
        before_revision,
        session.graph().graph().revision,
        "the material revision must change on a manifest-only edit"
    );
    assert_ne!(
        before_digest,
        session.graph().graph().digest,
        "the sealed graph identity must be invalidated too"
    );
    assert!(session
        .graph()
        .graph()
        .material_inputs
        .iter()
        .any(|input| input.path == "app/Cargo.toml"));
}
