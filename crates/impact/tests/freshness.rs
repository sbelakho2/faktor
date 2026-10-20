//! Repeated-edit freshness: every refresh re-examines every changed path, so
//! a second real edit replaces the edges produced by the first. Byte-identical
//! refreshes are cheap no-ops that keep the graph digest stable.

use std::fs;
use std::path::Path;

use faktor_impact::{AffectedOptions, ChangedTarget, ImpactSession, Revision, DEFAULT_MAX_DEPTH};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn revision(commit: &str) -> Revision {
    Revision {
        commit: Some(commit.to_string()),
        tree: Some(format!("tree-{commit}")),
        dirty: false,
    }
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

fn affected_names(session: &ImpactSession, path: &str, symbol: &str) -> (bool, Vec<String>) {
    let result = session
        .affected(
            &[ChangedTarget::symbol(path, symbol)],
            &AffectedOptions {
                crate_tests: false,
                max_depth: DEFAULT_MAX_DEPTH,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    let names: Vec<String> = result
        .affected
        .iter()
        .map(|node| node.name.clone())
        .collect();
    (names.iter().any(|name| name == "dispatch"), names)
}

fn references(session: &ImpactSession, target_path: &str) -> bool {
    session.graph().graph().edges.iter().any(|edge| {
        edge.from == "file:app/src/lib.rs"
            && edge.to == format!("file:{target_path}")
            && matches!(
                edge.kind,
                faktor_impact::EdgeKind::Imports | faktor_impact::EdgeKind::Uses
            )
    })
}

#[test]
fn repeated_edit_replaces_the_stale_reference_edge() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build(root, revision("one")).unwrap();

    let (affected_by_b, _) = affected_names(&session, "b/src/lib.rs", "target");
    assert!(affected_by_b, "the initial app must depend on b");
    assert!(references(&session, "b/src/lib.rs"));

    // First edit: switch the same file from b to c.
    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    let stats = session
        .refresh(&["app/src/lib.rs".into()], revision("two"))
        .unwrap();
    assert_eq!(stats.files_changed, 1);
    assert_eq!(stats.files_reparsed, 1, "only the edited file is parsed");

    let (affected_by_c, names_c) = affected_names(&session, "c/src/lib.rs", "target");
    assert!(
        affected_by_c,
        "the second edit must depend on c: {names_c:?}"
    );
    let (affected_by_b_again, names_b) = affected_names(&session, "b/src/lib.rs", "target");
    assert!(
        !affected_by_b_again,
        "the stale b relationship must be gone: {names_b:?}"
    );
    assert!(references(&session, "c/src/lib.rs"));
    assert!(!references(&session, "b/src/lib.rs"));
}

#[test]
fn same_content_refresh_is_a_cheap_noop() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();

    let initial = session.graph().graph().revision.clone();
    assert!(initial.dirty, "material revision must be dirty");
    assert!(
        initial
            .tree
            .as_deref()
            .is_some_and(|tree| tree.contains("#generation:1")),
        "material revision must carry a content digest and generation: {initial:?}"
    );

    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    let changed = session
        .refresh_material(&["app/src/lib.rs".into()])
        .unwrap();
    assert_eq!(changed.files_reparsed, 1);
    let after_edit = session.graph().graph().revision.clone();
    let digest_after_edit = session.graph().graph().digest.clone();
    assert!(after_edit.dirty);
    assert!(
        after_edit
            .tree
            .as_deref()
            .is_some_and(|tree| tree.contains("#generation:2")),
        "a real content change must advance the material generation: {after_edit:?}"
    );

    // Rewrite exactly the same bytes: the refresh must not re-parse, must not
    // advance the generation and must not change the graph digest.
    let same = fs::read_to_string(root.join("app/src/lib.rs")).unwrap();
    write(root, "app/src/lib.rs", &same);
    let noop = session
        .refresh_material(&["app/src/lib.rs".into()])
        .unwrap();
    assert_eq!(noop.files_changed, 0);
    assert_eq!(noop.files_unchanged, 1);
    assert_eq!(noop.files_reparsed, 0);
    assert_eq!(session.graph().graph().digest, digest_after_edit);
    assert_eq!(session.graph().graph().revision, after_edit);
}

#[test]
fn rapid_double_edit_settles_on_the_last_content() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build(root, revision("one")).unwrap();

    // Two writes between refreshes (same path, no intermediate read).
    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() + 1 }\n",
    );
    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() + 2 }\n",
    );
    let stats = session
        .refresh(&["app/src/lib.rs".into()], revision("two"))
        .unwrap();
    assert_eq!(stats.files_reparsed, 1);
    assert!(references(&session, "c/src/lib.rs"));
    assert!(!references(&session, "b/src/lib.rs"));

    // Immediately edit again and refresh without waiting: the content digest,
    // not a timestamp or lifetime set, decides what is stale.
    write(
        root,
        "app/src/lib.rs",
        "use b::target;\n\npub fn dispatch() -> u32 { target() + 3 }\n",
    );
    session
        .refresh(&["app/src/lib.rs".into()], revision("three"))
        .unwrap();
    let (affected_by_b, names_b) = affected_names(&session, "b/src/lib.rs", "target");
    assert!(
        affected_by_b,
        "the newest content must depend on b: {names_b:?}"
    );
    let (affected_by_c, names_c) = affected_names(&session, "c/src/lib.rs", "target");
    assert!(
        !affected_by_c,
        "the superseded c relationship must be gone: {names_c:?}"
    );
    assert!(references(&session, "b/src/lib.rs"));
    assert!(!references(&session, "c/src/lib.rs"));
}
