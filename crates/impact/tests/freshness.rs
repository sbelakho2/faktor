//! Repeated-edit freshness: every refresh re-examines every changed path, so
//! a second real edit replaces the edges produced by the first. Byte-identical
//! refreshes are cheap no-ops that keep the graph digest stable.

use std::fs;
use std::path::Path;

use faktor_impact::{
    AffectedOptions, BuildOptions, ChangedTarget, GenerationLimits, ImpactSession,
    MaterialGeneration, RefreshMode, RefreshPolicy, Revision, DEFAULT_MAX_DEPTH,
};

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

/// The generation authority — not the paths a query happens to name — is
/// what makes an out-of-band edit visible. The scan only selects candidates;
/// the refresh still proves the change by content digest.
#[test]
fn generation_sync_refreshes_an_out_of_band_edit() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let generation = MaterialGeneration::new();

    let (affected_by_b, names_b) = affected_names(&session, "b/src/lib.rs", "target");
    assert!(affected_by_b, "initial graph must depend on b: {names_b:?}");

    // Out-of-band: the query will be seeded by b, but app is the edited file.
    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    let scan = session.scan_material_changes().unwrap();
    assert!(
        scan.seeds.iter().any(|path| path == "app/src/lib.rs"),
        "the whole-workspace scan must see the out-of-band edit: {scan:?}"
    );
    assert!(scan.seeds.iter().all(|path| path == "app/src/lib.rs"));
    generation.observe(scan.seeds);
    let dirty = generation.dirty_since(0);
    assert!(!dirty.lost);
    let report = session
        .refresh_from_generation(&dirty, &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Incremental);
    assert_eq!(report.generation, 1);
    assert!(!report.degraded);

    let (affected_by_b_again, names_b) = affected_names(&session, "b/src/lib.rs", "target");
    assert!(
        !affected_by_b_again,
        "the stale b relationship must be gone: {names_b:?}"
    );
    let (affected_by_c, names_c) = affected_names(&session, "c/src/lib.rs", "target");
    assert!(
        affected_by_c,
        "the out-of-band c relationship must be present: {names_c:?}"
    );
    assert!(references(&session, "c/src/lib.rs"));
    assert!(!references(&session, "b/src/lib.rs"));

    // The same generation is a cheap no-op: no re-parse, no digest change.
    let digest = session.graph().graph().digest.clone();
    let current = generation.current();
    let noop = session
        .refresh_from_generation(&generation.dirty_since(current), &RefreshPolicy::default())
        .unwrap();
    assert_eq!(noop.mode, RefreshMode::Unchanged);
    assert_eq!(noop.stats.files_reparsed, 0);
    assert_eq!(session.graph().graph().digest, digest);
}

#[test]
fn rapid_multi_file_edits_are_one_generation_and_settle_on_disk_content() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let generation = MaterialGeneration::new();

    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    write(
        root,
        "b/src/lib.rs",
        "pub fn target() -> u32 { 1 }\npub fn extra() -> u32 { 2 }\n",
    );
    let scan = session.scan_material_changes().unwrap();
    assert_eq!(scan.seeds.len(), 2, "both edits are dirty: {scan:?}");
    generation.observe(scan.seeds);
    let report = session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Incremental);
    assert_eq!(report.stats.files_changed, 2);
    assert!(report.stats.files_reparsed <= 2);
    assert!(references(&session, "c/src/lib.rs"));
    assert!(!references(&session, "b/src/lib.rs"));
    assert!(
        session
            .graph()
            .node("symbol:b/src/lib.rs#extra@2")
            .is_some(),
        "the second file edit must be indexed"
    );
}

#[test]
fn dirty_history_overflow_rebuilds_the_whole_workspace() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let generation = MaterialGeneration::with_limits(GenerationLimits {
        max_generations: 1,
        max_dirty_paths: 1024,
    });

    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    write(
        root,
        "b/src/lib.rs",
        "pub fn target() -> u32 { 1 }\npub fn rebound() -> u32 { 3 }\n",
    );
    // Two observations while the cached graph is still at generation 0:
    // the first generation is dropped, so the exact dirty set is lost.
    generation.observe(["app/src/lib.rs"]);
    generation.observe(["b/src/lib.rs"]);
    let dirty = generation.dirty_since(0);
    assert!(dirty.lost, "bounded history overflow must be typed lost");
    let report = session
        .refresh_from_generation(&dirty, &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Rebuilt);
    assert!(report.lost);
    assert!(references(&session, "c/src/lib.rs"));
    assert!(
        session
            .graph()
            .node("symbol:b/src/lib.rs#rebound@2")
            .is_some(),
        "the rebuild must include every file, not a partial dirty view"
    );
}

#[test]
fn explicit_history_loss_rebuilds_and_never_returns_a_silent_partial() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let generation = MaterialGeneration::new();

    write(
        root,
        "app/src/lib.rs",
        "use c::target;\n\npub fn dispatch() -> u32 { target() }\n",
    );
    generation.observe(["app/src/lib.rs"]);
    generation.mark_history_lost();
    let dirty = generation.dirty_since(0);
    assert!(dirty.lost);
    let report = session
        .refresh_from_generation(&dirty, &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Rebuilt);
    assert!(references(&session, "c/src/lib.rs"));
}

/// The REBUILD policy budget is a typed refusal, not a silent partial: a
/// rebuild that would exceed `max_rebuild_files` leaves the old graph in
/// place, reports `Degraded` with a reason, and every affected result
/// carries the degraded flag/note.
#[test]
fn rebuild_policy_budget_exceeded_is_a_typed_degraded_refusal() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let generation = MaterialGeneration::new();
    generation.observe(["app/src/lib.rs"]);
    generation.mark_history_lost();
    let policy = RefreshPolicy {
        max_incremental_paths: 8,
        max_rebuild_files: 1,
    };
    let report = session
        .refresh_from_generation(&generation.dirty_since(0), &policy)
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Degraded, "{report:?}");
    assert!(report.degraded);
    assert!(
        report
            .note
            .as_deref()
            .is_some_and(|note| note.contains("exceed")),
        "the refusal must name the budget: {report:?}"
    );

    let result = session
        .affected(
            &[ChangedTarget::file("app/src/lib.rs")],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(result.degraded, "the query must never look complete");
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.contains("not current") || note.contains("budget")),
        "the typed reason must ride the result: {result:?}"
    );
}

#[test]
fn rebuild_budget_exhaustion_is_a_typed_degraded_flag() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material_for_workspace_with_options(
        root,
        faktor_core::id::WorkspaceId::new(1),
        BuildOptions {
            max_files: 2,
            ..BuildOptions::default()
        },
    )
    .unwrap();
    assert!(session.builder().report().truncated);
    let generation = MaterialGeneration::new();
    generation.observe(["app/src/lib.rs"]);
    generation.mark_history_lost();
    let report = session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Rebuilt);
    assert!(report.degraded, "truncated rebuild must be degraded");
    assert!(report
        .note
        .as_deref()
        .is_some_and(|note| note.contains("bound")));

    let result = session
        .affected(
            &[ChangedTarget::file("app/src/lib.rs")],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(result.degraded);
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.contains("bound") || note.contains("not current")),
        "the degraded flag must carry its reason: {result:?}"
    );
}

/// Bounded 10k-file probe: a single edit must re-parse a bounded set, and an
/// unchanged generation must not rebuild. Ignored in CI long-test lanes.
#[test]
#[ignore = "[perf] impact 10k-file incremental generation refresh"]
fn perf_10k_files_incremental_refresh_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[workspace]\nmembers = [\"big\"]\n");
    write(
        root,
        "big/Cargo.toml",
        "[package]\nname = \"big\"\nversion = \"0.1.0\"\n",
    );
    write(root, "big/src/lib.rs", "pub fn root() -> u32 { 1 }\n");
    for index in 0..10_000 {
        write(
            root,
            &format!("big/src/f{index:05}.rs"),
            &format!("pub fn f{index}() -> u32 {{ {index} }}\n"),
        );
    }
    let mut session = ImpactSession::build_material(root).unwrap();
    assert_eq!(session.graph().graph().stats.files, 10_001);
    let generation = MaterialGeneration::new();

    let changed = "big/src/f00042.rs";
    write(
        root,
        changed,
        "pub fn f42() -> u32 { 42 }\npub fn extra() -> u32 { 43 }\n",
    );
    let started = std::time::Instant::now();
    let scan = session.scan_material_changes().unwrap();
    assert_eq!(scan.seeds, vec![changed.to_string()]);
    generation.observe(scan.seeds);
    let report = session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Incremental);
    assert!(
        report.stats.files_reparsed <= 8,
        "10k-file refresh must stay bounded: {report:?}"
    );
    assert!(session
        .graph()
        .node("symbol:big/src/f00042.rs#extra@2")
        .is_some());

    let noop_started = std::time::Instant::now();
    for _ in 0..20 {
        let dirty = generation.dirty_since(generation.current());
        let report = session
            .refresh_from_generation(&dirty, &RefreshPolicy::default())
            .unwrap();
        assert_eq!(report.mode, RefreshMode::Unchanged);
        assert_eq!(report.stats.files_reparsed, 0);
    }
    println!(
        "[perf] 10k files: edit scan+refresh {:?}, 20 no-op generation checks {:?}",
        started.elapsed(),
        noop_started.elapsed()
    );
}

#[test]
fn manifest_edit_invalidates_topology_without_a_source_change() {
    let dir = workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let generation = MaterialGeneration::new();

    let depends_on = |session: &ImpactSession, from: &str, to: &str| {
        session.graph().graph().edges.iter().any(|edge| {
            edge.from == from && edge.to == to && edge.kind == faktor_impact::EdgeKind::DependsOn
        })
    };
    assert!(depends_on(&session, "crate:app", "crate:b"));

    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nc = { path = \"../c\" }\n",
    );
    let scan = session.scan_material_changes().unwrap();
    assert!(
        scan.seeds.iter().any(|path| path == "app/Cargo.toml"),
        "the manifest stamp must be part of the whole-workspace scan: {scan:?}"
    );
    generation.observe(scan.seeds);
    let report = session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Rebuilt);
    assert!(!depends_on(&session, "crate:app", "crate:b"));
    assert!(depends_on(&session, "crate:app", "crate:c"));
}
