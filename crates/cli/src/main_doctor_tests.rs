//! `main_doctor_tests`: out-of-line slice of the CLI test module.

use super::*;

#[test]
fn doctor_plain_passes_on_a_healthy_store_and_fails_on_corruption() {
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "healthy store: {:?}", report.lines);
    assert!(report.lines.iter().any(|l| l == "store: ok"));
    assert!(report
        .lines
        .iter()
        .any(|l| l.contains("\"journal_mode\": \"wal\"")));
    // Corrupt store: plain doctor fails loudly (never exits the process
    // from doctor_run; the wrapper owns the exit code).
    let garbage = tempfile::tempdir().unwrap();
    std::fs::write(
        garbage.path().join("store"),
        b"not a directory; the open must fail cleanly",
    )
    .unwrap();
    let report = doctor_run(garbage.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(report.lines.iter().any(|l| l.starts_with("store: FAILED")));
}

/// Audit item 8: doctor reports the platform's network-isolation
/// strength and the browser selection honestly — app-level proxy
/// configuration is never worded as confinement and no missing backend
/// is masked.
#[test]
fn doctor_reports_network_isolation_strength_honestly() {
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let sandbox = report
        .lines
        .iter()
        .find(|l| l.starts_with("sandbox network isolation:"))
        .expect("sandbox network isolation line");
    let browser = report
        .lines
        .iter()
        .find(|l| l.starts_with("browser network isolation:"))
        .expect("browser network isolation line");
    assert!(
        sandbox.contains("runtime_proof="),
        "the spawn-proof state must be named: {sandbox}"
    );
    if faktor_terminal::broker_only_supported() {
        assert!(
            sandbox.contains("backend=deny_all+broker_only_available"),
            "{sandbox}"
        );
        assert!(browser.contains("BrokerOnly requested"), "{browser}");
    } else {
        assert!(sandbox.contains("backend=unavailable"), "{sandbox}");
        assert!(sandbox.contains("refused typed"), "{sandbox}");
        // Audit 18/19(a): the missing backend is reported as a TYPED
        // platform boundary for THIS OS, with the honest reason — never a
        // silently healthy state and never app-level proxy presented as
        // confinement.
        assert!(
            sandbox.contains("typed platform boundary"),
            "the refusal must read as a typed platform boundary: {sandbox}"
        );
        assert!(sandbox.contains(std::env::consts::OS), "{sandbox}");
        assert!(browser.contains("NOT OS-confined"), "{browser}");
        assert!(browser.contains(std::env::consts::OS), "{browser}");
        assert!(!browser.contains("OS-confined per child"), "{browser}");
    }
}

/// The doctor's shell-execution surface reads the SAME sources the
/// daemon injects and reports BOTH trust classes separately and
/// honestly: an unset `[sandbox] shell` keeps the agent shell tool on
/// the fail-closed `os_isolated`/`required` default while the
/// interactive session-terminal class carries the honest user-granted
/// network-capable default; explicit settings show up on both classes.
#[test]
fn doctor_prints_both_shell_trust_classes_separately_and_honestly() {
    // No --config: two DISTINCT class lines (agent secure, terminals
    // user-granted), never merged into one implied contract.
    let empty = tempfile::tempdir().unwrap();
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_sandbox_shell_line(None, empty.path(), &mut lines, &mut issues);
    assert_eq!(issues, 0);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].contains("(agent shell tool)")
            && lines[0].contains("mode=os_isolated")
            && lines[0].contains("network_guarantee=required")
            && lines[0].contains("OS-isolated"),
        "the agent class default is the fail-closed OS-isolated contract: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("(interactive session terminals)")
            && lines[1].contains("mode=network_capable_user_granted")
            && lines[1].contains("network_guarantee=none")
            && lines[1].contains("GRANTED BY USER"),
        "the terminal class default is the honest user grant, never isolation: {}",
        lines[1]
    );

    // Explicit OS isolation in config: BOTH classes report the
    // isolated contract, from the same source the daemon injects.
    let dir = tempfile::tempdir().unwrap();
    let os_config = dir.path().join("os.json");
    std::fs::write(
        &os_config,
        r#"{"model": "m", "sandbox": {"network_guarantee": "required", "shell": "os_isolated"}}"#,
    )
    .unwrap();
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_sandbox_shell_line(Some(&os_config), empty.path(), &mut lines, &mut issues);
    assert_eq!(issues, 0, "{lines:?}");
    assert_eq!(lines.len(), 2, "{lines:?}");
    for line in &lines {
        assert!(
            line.contains("mode=os_isolated") && line.contains("network_guarantee=required"),
            "{line}"
        );
    }

    // The EXPLICIT operator grant: the doctor reports exactly the mode
    // the authority constructed from the same config enforces, for both
    // classes.
    let grant_config = dir.path().join("grant.json");
    std::fs::write(
            &grant_config,
            r#"{"model": "m", "sandbox": {"network_guarantee": "none", "shell": "network_capable_user_granted"}}"#,
        )
        .unwrap();
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_sandbox_shell_line(Some(&grant_config), empty.path(), &mut lines, &mut issues);
    assert_eq!(issues, 0, "{lines:?}");
    assert_eq!(lines.len(), 2, "{lines:?}");
    for line in &lines {
        assert!(
            line.contains("mode=network_capable_user_granted")
                && line.contains("network_guarantee=none")
                && line.contains("GRANTED BY USER"),
            "the grant is surfaced as strictly weaker, never as isolation: {line}"
        );
    }

    // The SAME config feeds the authority the daemon injects, and the
    // authority reports the mode doctor printed.
    let cfg = serve_config_and_semantic(Some(grant_config))
        .expect("the grant config is valid")
        .0;
    let policy = terminal_authority_policy(&cfg).expect("the grant config resolves a policy");
    assert_eq!(
        policy.shell_execution_state().mode,
        faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted
    );
}

/// Doctor RE-VERIFIES the restore point and fails loudly (naming the DB)
/// when it is missing while the recorded policy requires one, or when its
/// content does not match its name's version claim.
#[test]
fn doctor_fails_loudly_on_missing_or_mislabeled_restore_points() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    drop(faktor_cloud::SqliteControlPlaneStore::open(&path).unwrap());
    let (point, _) =
        faktor_cloud::durability::latest_migration_backup(&path).expect("v0 restore point");
    assert!(point
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .contains("-pre-migration-v0-"));

    // Missing while required: doctor fails and names the database.
    std::fs::remove_file(&point).unwrap();
    let report = doctor_run(dir.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db control-plane.db:")
                && l.contains("migration restore point FAILED verification")
                && l.contains("no pre-migration restore point exists")),
        "{:?}",
        report.lines
    );

    // Mislabeled: a full copy of the LIVE (post-migration) database under
    // a `-pre-migration-v4-` name must fail verification, naming the DB.
    let backup_dir = faktor_cloud::durability::backup_dir(&path);
    std::fs::create_dir_all(&backup_dir).unwrap();
    let mislabeled = backup_dir.join(format!(
        "control-plane-pre-migration-v4-{}-9.db",
        std::process::id()
    ));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        faktor_cloud::durability::backup_to(&conn, &mislabeled).unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db control-plane.db:")
                && l.contains("migration restore point FAILED verification")
                && l.contains("mislabeled")),
        "{:?}",
        report.lines
    );
}

/// A corrupt commercial database fails doctor loudly and the `cloud-db`
/// section names it (never silently skipped, never auto-repaired).
#[test]
fn doctor_fails_loudly_on_a_corrupt_commercial_db_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("control-plane.db"),
        b"garbage that is not a sqlite database",
    )
    .unwrap();
    let report = doctor_run(dir.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db control-plane.db: FAILED")),
        "{:?}",
        report.lines
    );
}

/// The nested discovery probe: databases below the data root (workspace /
/// session / cloud paths) are found with a bounded depth and a stable,
/// deterministic order; hidden/backup trees, in-progress temp files and
/// paths beyond the depth bound are never probed.
#[test]
fn doctor_discovers_nested_databases_deterministically_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    // A nested cloud-family database (marker + verified backup) ...
    let cloud_dir = dir.path().join("cloud-state");
    std::fs::create_dir_all(&cloud_dir).unwrap();
    drop(faktor_cloud::SqliteControlPlaneStore::open(&cloud_dir.join("control-plane.db")).unwrap());
    // ... and a nested SCM store database (its own child backup tree).
    let scm_dir = dir.path().join("store").join("nested");
    std::fs::create_dir_all(&scm_dir).unwrap();
    drop(faktor_scm::SqliteScmStore::open(&scm_dir.join("scm.db")).unwrap());
    // Below the depth bound: `a/b/c/d/buried.db` (depth 4) is not probed.
    let deep = dir.path().join("a").join("b").join("c").join("d");
    std::fs::create_dir_all(&deep).unwrap();
    {
        let conn = rusqlite::Connection::open(deep.join("buried.db")).unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
    }
    // In-progress snapshots are invisible: the name carries `.tmp`.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    {
        let conn = rusqlite::Connection::open(scratch.join("half.db.tmp")).unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
    }
    let rels = |report: &DoctorReport| -> Vec<String> {
        report
            .lines
            .iter()
            .filter_map(|l| {
                let rest = l.strip_prefix("cloud-db ")?;
                let (rel, _) = rest.split_once(": journal_mode=")?;
                Some(rel.to_string())
            })
            .collect()
    };
    let first = doctor_run(dir.path(), false);
    let second = doctor_run(dir.path(), false);
    let first_rels = rels(&first);
    assert_eq!(
        first_rels,
        rels(&second),
        "nested discovery order must be deterministic"
    );
    assert!(
        first_rels.contains(&"cloud-state/control-plane.db".to_string()),
        "{:?}",
        first.lines
    );
    assert!(
        first_rels.contains(&"store/nested/scm.db".to_string()),
        "{:?}",
        first.lines
    );
    assert!(
        !first_rels.iter().any(|rel| rel.contains("buried")),
        "beyond the depth bound: {:?}",
        first.lines
    );
    assert!(
        !first_rels.iter().any(|rel| rel.contains("half")),
        "temp snapshots are not probed: {:?}",
        first.lines
    );
    let nested = first
        .lines
        .iter()
        .find(|l| l.starts_with("cloud-db store/nested/scm.db:"))
        .expect("the nested store database is named with its relative path");
    assert!(nested.contains("journal_mode=wal"), "{nested}");
    assert!(nested.contains("synchronous=FULL"), "{nested}");
    assert!(nested.contains("integrity=ok"), "{nested}");
    assert!(
        first
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db store/nested/scm.db: last verified backup")),
        "{:?}",
        first.lines
    );
    assert_eq!(
        first.issues, 0,
        "healthy nested databases: {:?}",
        first.lines
    );
}

/// A corrupt nested database fails doctor loudly and is named by its
/// data-root-relative path (never silently skipped, never auto-repaired).
#[test]
fn doctor_fails_loudly_on_a_corrupt_nested_db_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("store").join("sessions");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("broken.db"),
        b"garbage that is not a sqlite database",
    )
    .unwrap();
    let report = doctor_run(dir.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db store/sessions/broken.db: FAILED")),
        "{:?}",
        report.lines
    );
}

/// The probe budget is a bound, not a silent truncation: databases past
/// it are counted, named as unprobed, and the run fails loudly.
#[test]
fn doctor_bounds_the_database_probe_budget_and_names_the_excess() {
    let dir = tempfile::tempdir().unwrap();
    let many = dir.path().join("many");
    std::fs::create_dir_all(&many).unwrap();
    for i in 0..33 {
        let conn = rusqlite::Connection::open(many.join(format!("db-{i:02}.db"))).unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("beyond the 32-database probe budget were NOT probed")),
        "{:?}",
        report.lines
    );
    assert!(report.issues >= 1, "{:?}", report.lines);
    // The probed subset itself stays named and healthy.
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("cloud-db many/db-00.db:")),
        "{:?}",
        report.lines
    );
}

/// The `index embeddings` doctor section renders the TYPED status of the
/// published generation exactly as `IndexView::embedding_index(ws)
/// .build_status()` exposes it: a provider-failure fixture renders
/// `degraded` with the affected chunk count and the bounded reason, a
/// working source renders `complete`, and a build with no configured
/// source renders `unconfigured` — never a silent lexical-only success.
#[test]
fn doctor_reports_the_published_index_embedding_build_status_typed_states() {
    use faktor_core::error::{Error, ErrorKind};
    use faktor_index::embedding::{EmbeddingBuildStatus, EmbeddingModel, EmbeddingSource};
    use faktor_index::IndexService;

    struct FailingSource;
    impl EmbeddingSource for FailingSource {
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
            Err(Error::new(
                ErrorKind::Provider {
                    code: "embedding_backend_down".into(),
                    retryable: false,
                },
                "embedding backend down",
            ))
        }
    }
    struct WorkingSource;
    impl EmbeddingSource for WorkingSource {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
            Ok(texts.iter().map(|_| vec![0.25, 0.5, 0.75, 1.0]).collect())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    // The daemon derives its index data root from the store path; the
    // doctor reads exactly that root.
    let index = IndexService::open(
        session.store(),
        dir.path().join("store").join("index_data"),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();

    let build = |name: &str, source: Option<Arc<dyn EmbeddingSource>>| -> u64 {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), b"pub fn fixture() -> i64 { 1 }\n").unwrap();
        let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
        index.set_embedding_source(source, EmbeddingModel::new("fixture-model", "r1"));
        let view = index
            .ensure_ready(
                ws,
                std::time::Instant::now() + std::time::Duration::from_secs(120),
            )
            .unwrap();
        // The fixture asserts the SAME typed chain the doctor renders off
        // the published generation. A source-less, prior-less build
        // installs NO embedding map entry in the live view (callers read
        // that absence as "no embeddings" = unconfigured), while the
        // persisted generation carries the typed `Unconfigured` record
        // the doctor reads; both spell the same state.
        let status = view
            .index()
            .lock()
            .unwrap()
            .embedding_index(ws)
            .map(|index| index.build_status())
            .unwrap_or(EmbeddingBuildStatus::Unconfigured);
        match name {
            "degraded_repo" => assert!(
                matches!(status, EmbeddingBuildStatus::Degraded { affected: 1, .. }),
                "the failing fixture must persist a degraded status: {status:?}"
            ),
            "complete_repo" => assert!(
                matches!(status, EmbeddingBuildStatus::Complete { embedded: 1, .. }),
                "the working fixture must persist a complete status: {status:?}"
            ),
            _ => assert_eq!(status, EmbeddingBuildStatus::Unconfigured),
        }
        ws.raw()
    };
    let degraded = build("degraded_repo", Some(Arc::new(FailingSource)));
    let complete = build("complete_repo", Some(Arc::new(WorkingSource)));
    let unconfigured = build("unconfigured_repo", None);
    drop(index);
    drop(session);

    let report = doctor_run(dir.path(), false);
    let line_for = |raw: u64| {
        report
            .lines
            .iter()
            .find(|l| l.contains(&format!("workspace {raw}:")))
            .unwrap_or_else(|| panic!("no index line for workspace {raw}: {:?}", report.lines))
            .clone()
    };
    let degraded_line = line_for(degraded);
    assert!(
        degraded_line.contains(": degraded affected=1"),
        "{degraded_line}"
    );
    assert!(
        degraded_line.contains("reason=")
            && degraded_line.contains("embedding_backend_down")
            && degraded_line.contains("embedding backend down"),
        "{degraded_line}"
    );
    let complete_line = line_for(complete);
    assert!(complete_line.contains(": complete"), "{complete_line}");
    assert!(complete_line.contains("embedded=1"), "{complete_line}");
    let unconfigured_line = line_for(unconfigured);
    assert!(
        unconfigured_line.contains(": unconfigured"),
        "{unconfigured_line}"
    );
    assert_eq!(report.issues, 0, "{:?}", report.lines);
}

/// Doctor never trusts the index data it reads: a corrupt published
/// generation is named by workspace and counted as an issue, a numeric
/// generation directory with no durable state row is flagged, and
/// non-workspace junk (non-numeric names, the reserved id 0) is ignored
/// without a line or a panic.
#[test]
fn doctor_reports_index_coverage_and_partial_fingerprint() {
    use faktor_index::{FingerprintCoverage, IndexCoverage, IndexService};

    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let index_root = dir.path().join("store").join("index_data");
    let index = IndexService::open(
        session.store(),
        index_root,
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("lib.rs"), b"pub fn covered() -> i64 { 1 }\n").unwrap();
    let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
    index
        .ensure_ready(
            ws,
            std::time::Instant::now() + std::time::Duration::from_secs(120),
        )
        .unwrap();
    drop(index);
    drop(session);

    let report = doctor_run(dir.path(), false);
    let line = report
        .lines
        .iter()
        .find(|l| l.starts_with(&format!("index coverage: workspace {}", ws.raw())))
        .unwrap_or_else(|| panic!("coverage line missing: {:?}", report.lines));
    assert!(line.contains("coverage=complete"), "{line}");
    assert!(line.contains("fingerprint=complete"), "{line}");
    assert!(line.contains("files_indexed=1"), "{line}");

    // The PARTIAL rendering is typed and names the batch reason/shard.
    let mut partial = IndexCoverage::empty();
    partial.files_seen = 7;
    partial.files_indexed = 5;
    partial.bytes_indexed = 123;
    partial.truncated_reason = Some("batch_files".into());
    let mut fingerprint = FingerprintCoverage::round(3);
    fingerprint.truncated_reason = Some("fingerprint_files".into());
    let line = format_index_coverage_line(ws.raw(), 9, Some(&partial), Some(&fingerprint));
    assert!(
        line.contains("coverage=INCOMPLETE reason=batch_files"),
        "{line}"
    );
    assert!(line.contains("files_indexed=5"), "{line}");
    assert!(line.contains("fingerprint=PARTIAL shard=3/"), "{line}");
    assert!(line.contains("reason=fingerprint_files"), "{line}");
    // Pre-coverage envelopes are NAMED as legacy-unknown INCOMPLETE, never
    // silently reported complete: absence of coverage metadata can never
    // mean "clean".
    let legacy = format_index_coverage_line(ws.raw(), 1, None, None);
    assert!(
        legacy.contains("coverage=legacy_unknown")
            && legacy.contains("fingerprint=legacy_unknown")
            && legacy.contains("INCOMPLETE")
            && legacy.contains("PARTIAL"),
        "{legacy}"
    );
    assert!(
        !legacy.contains("treated complete") && !legacy.contains("coverage=complete"),
        "{legacy}"
    );
}

#[test]
fn doctor_surfaces_corrupt_and_unmanaged_index_generations_without_panicking() {
    use faktor_index::IndexService;

    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let index_root = dir.path().join("store").join("index_data");
    let index = IndexService::open(
        session.store(),
        index_root.clone(),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("lib.rs"), b"pub fn corrupt_me() -> i64 { 1 }\n").unwrap();
    let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
    index
        .ensure_ready(
            ws,
            std::time::Instant::now() + std::time::Duration::from_secs(120),
        )
        .unwrap();
    drop(index);
    drop(session);

    // Corrupt the published generation's bytes in place (the envelope is
    // present, the payload is garbage).
    let gen_dir = index_root.join("generations").join(ws.raw().to_string());
    let published = std::fs::read_dir(&gen_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .expect("the published generation file exists");
    std::fs::write(&published, b"{ not a generation").unwrap();

    // A numeric workspace dir with no durable state row, plus junk the
    // doctor must ignore.
    std::fs::create_dir_all(index_root.join("generations").join("7777")).unwrap();
    std::fs::write(index_root.join("generations").join("not-a-workspace"), b"x").unwrap();
    std::fs::create_dir_all(index_root.join("generations").join("0")).unwrap();

    let report = doctor_run(dir.path(), false);
    assert!(
        report.lines.iter().any(|l| l.contains(&format!(
            "index embeddings: workspace {}: generation 1 unreadable",
            ws.raw()
        ))),
        "{:?}",
        report.lines
    );
    assert!(
        report.lines.iter().any(|l| l.contains(
            "index embeddings: workspace 7777: generation data exists with no durable index state"
        )),
        "{:?}",
        report.lines
    );
    assert!(
        !report.lines.iter().any(|l| l.contains("workspace 0:")),
        "the reserved id 0 must never be probed: {:?}",
        report.lines
    );
    assert!(
        !report.lines.iter().any(|l| l.contains("not-a-workspace")),
        "{:?}",
        report.lines
    );
    assert!(report.issues >= 2, "{:?}", report.lines);
}

/// The workspace probe budget is a bound, not a silent truncation:
/// generation directories past it are counted, named as unprobed, and
/// the run fails loudly.
#[test]
fn doctor_bounds_the_index_workspace_probe_budget_and_names_the_excess() {
    let dir = tempfile::tempdir().unwrap();
    let generations = dir
        .path()
        .join("store")
        .join("index_data")
        .join("generations");
    for raw in 1..=33u64 {
        std::fs::create_dir_all(generations.join(raw.to_string())).unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l
                .contains("index embeddings: 33 workspace(s) with persisted index generations")),
        "{:?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l
                .contains("1 workspace(s) beyond the 32-workspace probe budget were NOT probed")),
        "{:?}",
        report.lines
    );
    assert!(report.issues >= 33, "{:?}", report.lines);
}

#[test]
fn doctor_deep_flags_a_corrupt_cas_blob_and_never_silently_heals() {
    // Audit 73/74: doctor --deep lists a corrupted CAS blob as an issue
    // and a SECOND run finds the SAME issue — corruption is surfaced,
    // never repaired.
    let dir = tempfile::tempdir().unwrap();
    let hash_hex = {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = session.create_workspace("/w").unwrap();
        let sid = session.create_session(ws, "t", "p", "m").unwrap().id();
        let cas = session.cas();
        let hash = cas.put(b"doctor blob").unwrap();
        session
            .store()
            .put_artifact(sid, "command_output", &hash.to_hex(), "sum", 11)
            .unwrap();
        // Corrupt the blob behind the CAS's back: present file, wrong
        // content (not zstd, so verify_integrity flags it).
        let blob = cas.root().join(hash.cas_path());
        std::fs::write(&blob, b"this is not zstd-compressed content").unwrap();
        hash.to_hex()
    };
    let first = doctor_run(dir.path(), true);
    assert!(first.issues > 0, "{:?}", first.lines);
    assert!(
        first
            .lines
            .iter()
            .any(|l| l.contains("cas blob corrupt") && l.contains(&hash_hex)),
        "{:?}",
        first.lines
    );
    // Second run: still failing — no silent healing.
    let second = doctor_run(dir.path(), true);
    assert!(second.issues > 0, "{:?}", second.lines);
    assert!(
        second.lines.iter().any(|l| l.contains(&hash_hex)),
        "{:?}",
        second.lines
    );
}

#[test]
fn doctor_deep_flags_dangling_and_malformed_cas_references() {
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = session.create_workspace("/w").unwrap();
        let sid = session.create_session(ws, "t", "p", "m").unwrap().id();
        let missing = "ab".repeat(32);
        session
            .store()
            .put_artifact(sid, "command_output", &missing, "sum", 10)
            .unwrap();
        session
            .store()
            .put_artifact(sid, "command_output", "not-a-hex-hash", "sum", 10)
            .unwrap();
        let real = session.cas().put(b"present").unwrap();
        session
            .store()
            .put_artifact(sid, "command_output", &real.to_hex(), "sum", 7)
            .unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 2, "{:?}", report.lines);
    assert!(
        report.lines.iter().any(|l| {
            l.contains("dangling cas reference")
                && l.contains(&"ab".repeat(32))
                && l.contains("missing CAS blob")
        }),
        "{:?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("malformed CAS hash")),
        "{:?}",
        report.lines
    );
    // Rerun: the same refs are still dangling (no repair happened).
    let again = doctor_run(dir.path(), true);
    assert!(again.issues >= 2, "{:?}", again.lines);
}

#[test]
fn doctor_deep_reports_global_running_rows_without_failing() {
    // Deep doctor surfaces cross-session recovery rows as INFORMATION
    // (a live daemon legitimately has running rows) — zero issues on an
    // otherwise healthy store.
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = session.create_workspace("/w").unwrap();
        let sid = session.create_session(ws, "t", "p", "m").unwrap().id();
        session
            .store()
            .start_tool_run(
                sid,
                faktor_core::id::OpId::new(7),
                "echo",
                serde_json::json!({}),
                serde_json::json!({"strategy": "none"}),
                None,
                None,
            )
            .unwrap();
        // The active turn's durable anchor: admission materializes the
        // prompt message at the PromptReceived journal seq, and the turn
        // record names that seq. Without the anchor the turn would be an
        // unrecoverable active turn (nothing could own it after a crash).
        session
            .store()
            .put_message(sid, 2, "user", serde_json::json!({"text": "x"}))
            .unwrap();
        session
            .store()
            .start_turn_record(
                sid,
                faktor_core::id::OpId::new(9),
                None,
                Some(2),
                "p",
                "m",
                None,
            )
            .unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("running tool runs across all sessions: 1")),
        "{:?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("active logical turns across all sessions: 1")),
        "{:?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("active turns with recoverable owners: 1 of 1")),
        "{:?}",
        report.lines
    );
}

// ---------------------------------- doctor deep invariants (P0-97/74/100)

/// Drive one REAL task to `VerifiedComplete` through the session APIs —
/// the ONLY legal path — with a live OPEN reservation on its row. Used
/// by every corruption test as the healthy baseline.
pub(crate) fn seed_verified_complete(
    m: &Arc<SessionManager>,
) -> (faktor_core::id::SessionId, faktor_core::id::TaskId, i64) {
    use faktor_core::id::{SessionId, TaskId};
    use faktor_core::state::{
        CriterionVerification, TaskState, TaskTransition, VerificationStatus,
    };
    let ws = m.create_workspace("/w").unwrap();
    let s = m.create_session(ws, "t", "p", "m").unwrap();
    let sid: SessionId = s.id();
    let task_id = TaskId::new(42);
    let now = m.now_ms();
    s.create_task(faktor_session::Task {
        task_id,
        session_id: sid,
        goal: "make it so".into(),
        acceptance_criteria: vec!["c1".into()],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        contract: faktor_core::contract::EngineeringContract::default(),
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let r1 = s.task_revision(task_id).unwrap();
    s.transition_task(task_id, r1, TaskTransition::RequestVerification, None)
        .unwrap();
    let r2 = s.task_revision(task_id).unwrap();
    s.transition_task(task_id, r2, TaskTransition::StartVerification, None)
        .unwrap();
    let r3 = s.task_revision(task_id).unwrap();
    let record = s
        .create_verification_record(
            task_id,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: true,
                evidence: None,
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    s.complete_verified_task(task_id, r3, record).unwrap();
    // A live open reservation against a REAL task row in a
    // provider-permitting state: a VerifiedComplete task forbids new
    // provider operations (and a completion cannot carry an open
    // reservation), so the healthy baseline parks the reservation on a
    // second Running task of the same session.
    let reservation_task = TaskId::new(43);
    s.create_task(faktor_session::Task {
        task_id: reservation_task,
        session_id: sid,
        goal: "hold a live reservation".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        contract: faktor_core::contract::EngineeringContract::default(),
        state: TaskState::Running,
        created_ms: m.now_ms(),
        updated_ms: m.now_ms(),
    })
    .unwrap();
    m.store()
        .cost_task_cap_set(sid, reservation_task, Some(1_000_000))
        .unwrap();
    let op = m.try_next_op_id().unwrap();
    let granted = m
        .store()
        .cost_reserve(sid, reservation_task, op, 1000, now)
        .unwrap();
    let reservation_id = match granted {
        faktor_store::CostReserveOutcome::Granted(id) => id,
        _ => panic!("reservation must be granted"),
    };
    (sid, task_id, reservation_id)
}

/// Raw sqlite handle for crafting corruption AFTER the manager closed
/// (doctor reopens the same file afterwards). The db lives at
/// `store/faktor-plus.db` under the doctor data dir.
pub(crate) fn raw_corruption_conn(data_dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(data_dir.join("store").join("faktor-plus.db")).unwrap()
}

#[test]
fn doctor_deep_passes_every_p097_section_on_a_healthy_store() {
    // The full audit surface passes on data produced ONLY through the
    // real APIs: a VerifiedComplete task + its Passed record, a live
    // reservation on the real task row, an orchestrated child identity
    // row + one well-formed non-terminal registry row whose worktree
    // directory exists.
    let dir = tempfile::tempdir().unwrap();
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = m.create_workspace("/w").unwrap();
        let parent = m.create_session(ws, "parent", "p", "m").unwrap();
        let (sid, task_id, _reservation) = seed_verified_complete(&m);
        let wt_path = dir.path().join("wt");
        std::fs::create_dir_all(&wt_path).unwrap();
        let wt_id = m
            .put_worktree(ws, wt_path.to_str().unwrap(), "feat/x")
            .unwrap();
        let child = m
            .create_child_session(
                parent.id(),
                ws,
                faktor_core::WorktreeId::new(wt_id as u64),
                faktor_core::TaskId::new(42),
                "p",
                "m",
                "child",
                faktor_session::ChildOwnership::ReadOnlyShared,
            )
            .unwrap();
        assert_eq!(parent.id().raw(), 1, "parent session id");
        assert_eq!(sid.raw(), 2, "verified task's session id");
        assert_eq!(child.id().raw(), 3, "child session id");
        // A well-formed NON-terminal registry row over the child (the
        // executor's durable shape, parent row space), whose worktree
        // dir exists.
        let runtime = faktor_orchestrator::runtime::ChildRuntime {
            child_id: "child-3".into(),
            parent_session_id: parent.id().raw(),
            run_id: "run-1".into(),
            item_id: "w1".into(),
            kind: faktor_orchestrator::WorkKind::Exploration,
            session_id: child.id().raw(),
            operation_id: 0,
            workspace_id: ws.raw(),
            worktree_id: wt_id as u64,
            ownership: faktor_session::ChildOwnership::ReadOnlyShared,
            ownership_paths: vec![],
            state: faktor_orchestrator::ChildState::Running,
            budget_max_tokens: None,
            permissions: faktor_orchestrator::caps::CapabilitySet::default(),
            model_policy: faktor_orchestrator::runtime::ModelPolicy::default(),
            blocker_kind: None,
            blocker_reason: None,
            blocker_dependency: None,
            blocker_resolution: None,
            last_progress_ms: None,
            execution_phase: faktor_orchestrator::runtime::ExecutionPhase::default(),
            created_ms: 1,
            updated_ms: 1,
            base_snapshot_id: None,
            run_base_snapshot: None,
            env_snapshot_id: None,
        };
        let value = serde_json::to_string(&runtime).unwrap();
        parent
            .upsert_memory_fact(
                "orchestrator_registry",
                &format!("run-1/{}", runtime.child_id),
                &value,
            )
            .unwrap();
        // Keep the (unused) id referenced so the data stays typed.
        let _ = task_id;
    }
    let report = doctor_run(dir.path(), true);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        report
            .lines
            .iter()
            .any(|l| l == "cost reservations: 1 (open 1, settled 0, refunded 0, uncertain 0)"),
        "{text}"
    );
    assert!(text.contains("dangling cost reservations: none"), "{text}");
    assert!(
            text.contains("verification records: 1 record(s), 1 completion-relevant task(s), 1 VerifiedComplete task(s)"),
            "{text}"
        );
    assert!(text.contains("verification consistency: ok"), "{text}");
    assert!(text.contains("journal consistency: ok"), "{text}");
    assert!(
        text.contains("orphan children: 1 child identity row(s), 1 registry row(s) scanned"),
        "{text}"
    );
    assert!(text.contains("orphan children: none"), "{text}");
    assert!(
        text.contains("active turns with recoverable owners: 0 of 0"),
        "{text}"
    );
    assert!(
        text.contains("process ownership: 0 durable session-owned process row(s)"),
        "{text}"
    );
    // None of the failing prefixes may appear.
    for bad in [
        "dangling cost reservation:",
        "verification inconsistency",
        "orphan child:",
        "active turn without recoverable owner",
    ] {
        assert!(!text.contains(bad), "{bad} present in: {text}");
    }
}

#[test]
fn doctor_deep_flags_dangling_open_and_settled_cost_reservations() {
    // Raw insert: RESERVED + SETTLED + UNCERTAIN reservation rows whose
    // task row does not exist (no store API can produce them —
    // cost_reserve refuses a missing task). Deep doctor must report the
    // typed section with the per-status count (reserved folds into the
    // in-flight "open" bucket) and one failing line per dangling row.
    // The legacy 'open'/'abandoned' vocabulary is dead: the v17 schema
    // CHECK rejects them at insert.
    let dir = tempfile::tempdir().unwrap();
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        m.create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    {
        let conn = raw_corruption_conn(dir.path());
        conn.execute(
                "INSERT INTO cost_reservation(session_id, task_id, op_id, predicted_micro, status, created_ms)
                 VALUES (1, 424242, 5, 1234, 'reserved', 1),
                        (1, 424243, 6, 999, 'settled', 1),
                        (1, 424244, 7, 100, 'uncertain', 1)",
                [],
            )
            .unwrap();
        for dead in ["open", "abandoned"] {
            let legacy = conn.execute(
                    "INSERT INTO cost_reservation(session_id, task_id, op_id, predicted_micro, status, created_ms)
                     VALUES (1, 424245, 8, 50, ?, 1)",
                    [dead],
                );
            assert!(
                legacy.is_err(),
                "the v17 CHECK forbids the legacy {dead:?} vocabulary"
            );
        }
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 3, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        text.contains("cost reservations: 3 (open 1, settled 1, refunded 0, uncertain 1)"),
        "{text}"
    );
    assert!(
        report.lines.iter().any(|l| {
            l.contains("dangling cost reservation: reservation 1")
                && l.contains("task 424242")
                && l.contains("status reserved")
        }),
        "{text}"
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("task 424243") && l.contains("status settled")),
        "{text}"
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("task 424244") && l.contains("status uncertain")),
        "{text}"
    );
}

#[test]
fn doctor_deep_flags_a_passed_record_for_a_missing_task() {
    // The store's raw record insert is deliberately unvalidated (the
    // session layer is the guard) — so a Passed record can reference a
    // task that never existed. Deep doctor must FAIL the wave-16
    // section with the exact kind and counts.
    let dir = tempfile::tempdir().unwrap();
    let session_id;
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let s = m
            .create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
        session_id = s.id();
        let row = faktor_store::VerificationRecordRow {
            id: faktor_core::id::VerificationRecordId::new(1), // ignored
            session_id: Some(s.id()),
            task_id: faktor_core::id::TaskId::new(777),
            revision: faktor_core::id::TaskRevision::new(1),
            workspace_id: faktor_core::id::WorkspaceId::new(1),
            worktree_id: faktor_core::id::WorktreeId::new(1),
            tree_hash: None,
            criteria: vec![],
            checks: vec![],
            changed_files: vec![],
            unrelated_changes: vec![],
            reviewer: None,
            status: faktor_core::state::VerificationStatus::Passed,
            started_ms: 1,
            completed_ms: None,
        };
        m.store().verification_record_put(&row).unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 1, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        report.lines.iter().any(|l| {
            l.contains("verification inconsistency [record_without_task]")
                && l.contains(&format!("task {}/777", session_id.raw()))
                && l.contains("which has no task row")
        }),
        "{text}"
    );
    assert!(
            text.contains("verification records: 1 record(s), 0 completion-relevant task(s), 0 VerifiedComplete task(s)"),
            "{text}"
        );
}

#[test]
fn doctor_deep_flags_passed_record_certifying_an_uncompleted_task() {
    // A Passed record may only certify the CURRENT revision of a
    // VerifiedComplete task. Raw-putting one against a Running task at
    // its revision is the exact wave-16 bypass deep doctor must catch.
    let dir = tempfile::tempdir().unwrap();
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = m.create_workspace("/w").unwrap();
        let s = m.create_session(ws, "t", "p", "m").unwrap();
        let task_id = faktor_core::id::TaskId::new(9);
        let now = m.now_ms();
        s.create_task(faktor_session::Task {
            task_id,
            session_id: s.id(),
            goal: "g".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            contract: faktor_core::contract::EngineeringContract::default(),
            state: faktor_core::state::TaskState::Running,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
        let row = faktor_store::VerificationRecordRow {
            id: faktor_core::id::VerificationRecordId::new(1), // ignored
            session_id: Some(s.id()),
            task_id,
            revision: faktor_core::id::TaskRevision::new(1),
            workspace_id: ws,
            worktree_id: faktor_core::id::WorktreeId::new(1),
            tree_hash: None,
            criteria: vec![],
            checks: vec![],
            changed_files: vec![],
            unrelated_changes: vec![],
            reviewer: None,
            status: faktor_core::state::VerificationStatus::Passed,
            started_ms: 1,
            completed_ms: None,
        };
        m.store().verification_record_put(&row).unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 1, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        report.lines.iter().any(|l| {
            l.contains("verification inconsistency [passed_on_uncompleted]")
                && l.contains("task 1/9")
                && l.contains("revision 1")
        }),
        "{text}"
    );
}

#[test]
fn doctor_deep_flags_verified_complete_task_whose_record_was_deleted() {
    // A legitimately completed task first passes every section; deleting
    // its consumed Passed record (raw SQL) must make the same dir FAIL
    // the wave-16 section — VerifiedComplete without its completion
    // proof.
    let dir = tempfile::tempdir().unwrap();
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        seed_verified_complete(&m);
    }
    let clean = doctor_run(dir.path(), true);
    assert_eq!(clean.issues, 0, "{:?}", clean.lines);
    {
        let conn = raw_corruption_conn(dir.path());
        conn.execute("DELETE FROM verification_record", []).unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 1, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        report.lines.iter().any(|l| {
            l.contains("verification inconsistency [verified_without_record]")
                && l.contains("task 1/42")
                && l.contains("at revision 4")
                && l.contains("completion revision 3")
        }),
        "{text}"
    );
    assert!(
            text.contains("verification records: 0 record(s), 1 completion-relevant task(s), 1 VerifiedComplete task(s)"),
            "{text}"
        );
    assert!(
        text.contains("cost reservations: 1 (open 1, settled 0, refunded 0, uncertain 0)"),
        "{text}"
    );
}

#[test]
fn doctor_deep_flags_active_turn_without_recoverable_owner() {
    // Raw insert of an active turn_record with NO durable anchor (no
    // prompt message, no queue row, no journal event, no tool-run row
    // names its op): after a crash nothing could own this turn.
    let dir = tempfile::tempdir().unwrap();
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        m.create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    {
        let conn = raw_corruption_conn(dir.path());
        conn.execute(
            "INSERT INTO turn_record(session_id, turn_op_id, started_at, status, updated_ms)
                 VALUES (1, 999999, 1, 'active', 1)",
            [],
        )
        .unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 1, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        text.contains("active turns with recoverable owners: 0 of 1"),
        "{text}"
    );
    assert!(
        report.lines.iter().any(|l| {
            l.contains("active turn without recoverable owner") && l.contains("op 999999")
        }),
        "{text}"
    );
}

/// Three durable wedges that used to be reported as "all checks passed":
/// (1) an op-active session with an active turn record and no drive at all,
/// (2) a pending permission whose waiter lived only in a dead process, and
/// (3) an applied workspace write with no verification/integration record.
/// Each must appear as a typed issue line and make `doctor --deep` fail.
#[test]
fn doctor_deep_flags_wedged_sessions_and_fails_nonzero() {
    use faktor_core::capability::Capability;
    use faktor_core::event::EventKind;
    use faktor_core::state::AgentState;

    let dir = tempfile::tempdir().unwrap();
    {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        // (1) admitted into Preparing, then never driven: active record, no
        // queue row, no permission, no running tool run.
        let ws1 = m.create_workspace("/wedge-ws-1").unwrap();
        let s1 = m.create_session(ws1, "no-drive", "p", "m").unwrap();
        s1.submit_prompt("never driven", &[]).unwrap();

        // (2) parked on a permission request: pending row, no live waiter.
        let ws2 = m.create_workspace("/wedge-ws-2").unwrap();
        let s2 = m
            .create_session(ws2, "orphan-permission", "p", "m")
            .unwrap();
        let receipt = s2.submit_prompt("park", &[]).unwrap();
        for (kind, state) in [
            (EventKind::ContextPrepared, AgentState::BuildingContext),
            (EventKind::ModelStarted, AgentState::WaitingForModel),
            (EventKind::ModelChunkReceived, AgentState::Streaming),
        ] {
            s2.append_event(kind, state, None, None).unwrap();
        }
        s2.request_permission(
            receipt.op_id,
            &Capability::ReadWorkspace {
                path: "/wedge-ws-2/a".into(),
            },
        )
        .unwrap();

        // (3) an applied workspace write (durable postcondition) on a
        // session with no task verification and no integration record.
        let ws3 = m.create_workspace("/wedge-ws-3").unwrap();
        let s3 = m.create_session(ws3, "applied", "p", "m").unwrap();
        let op = m.try_next_op_id().unwrap();
        m.store()
            .start_tool_run(
                s3.id(),
                op,
                "write_file",
                serde_json::json!({"path": "a.txt", "content": "x"}),
                serde_json::json!({"strategy": "mark_unknown"}),
                None,
                None,
            )
            .unwrap();
        m.store()
            .record_tool_postcondition(
                s3.id(),
                op,
                &serde_json::json!({
                    "workspace_id": 1,
                    "worktree_id": 1,
                    "relative_path": "a.txt",
                    "expected_hash": "ab".repeat(32),
                }),
            )
            .unwrap();
        m.store()
            .finish_tool_run(s3.id(), op, "completed", "applied")
            .unwrap();
    }

    let report = doctor_run(dir.path(), true);
    let text = report.lines.join("\n");
    for kind in [
        "active_turn_without_drive",
        "ownerless_pending_permission",
        "applied_run_without_verification",
    ] {
        assert!(
            text.contains(&format!("session wedge [{kind}]")),
            "missing typed issue {kind} in:\n{text}"
        );
    }
    assert!(text.contains("session wedges:"), "{text}");
    assert!(
        report.issues >= 3,
        "wedged sessions must fail doctor --deep: issues={} lines:\n{text}",
        report.issues
    );
}

#[test]
fn doctor_deep_flags_orphan_child_rows_and_a_missing_worktree_dir() {
    // Three orphan-child corruptions: an unparseable registry row, a
    // child identity row naming a parent session that does not exist,
    // and a well-formed NON-terminal registry row whose worktree
    // directory vanished from disk.
    let dir = tempfile::tempdir().unwrap();
    let wt_row_id = {
        let m =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = m.create_workspace("/w").unwrap();
        let parent = m.create_session(ws, "parent", "p", "m").unwrap();
        let _child = m.create_session(ws, "child", "p", "m").unwrap();
        let wt_path = dir.path().join("vanished-wt");
        std::fs::create_dir_all(&wt_path).unwrap();
        let wt_id = m
            .put_worktree(ws, wt_path.to_str().unwrap(), "feat/x")
            .unwrap();
        // A well-formed NON-terminal registry row (child session 2 is
        // live; the worktree DIRECTORY is removed below).
        let runtime = faktor_orchestrator::runtime::ChildRuntime {
            child_id: "child-2".into(),
            parent_session_id: parent.id().raw(),
            run_id: "run-1".into(),
            item_id: "w1".into(),
            kind: faktor_orchestrator::WorkKind::Exploration,
            session_id: 2,
            operation_id: 0,
            workspace_id: ws.raw(),
            worktree_id: wt_id as u64,
            ownership: faktor_session::ChildOwnership::ReadOnlyShared,
            ownership_paths: vec![],
            state: faktor_orchestrator::ChildState::Running,
            budget_max_tokens: None,
            permissions: faktor_orchestrator::caps::CapabilitySet::default(),
            model_policy: faktor_orchestrator::runtime::ModelPolicy::default(),
            blocker_kind: None,
            blocker_reason: None,
            blocker_dependency: None,
            blocker_resolution: None,
            last_progress_ms: None,
            execution_phase: faktor_orchestrator::runtime::ExecutionPhase::default(),
            created_ms: 1,
            updated_ms: 1,
            base_snapshot_id: None,
            run_base_snapshot: None,
            env_snapshot_id: None,
        };
        let value = serde_json::to_string(&runtime).unwrap();
        parent
            .upsert_memory_fact(
                "orchestrator_registry",
                &format!("run-1/{}", runtime.child_id),
                &value,
            )
            .unwrap();
        std::fs::remove_dir_all(&wt_path).unwrap();
        wt_id
    };
    {
        let conn = raw_corruption_conn(dir.path());
        // Unparseable registry row under session 1 (corruption, never a
        // silent skip).
        conn.execute(
            "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
                 VALUES (1, 'orchestrator_registry', 'run-1/child-x', '{not-json', 1)",
            [],
        )
        .unwrap();
        // Child identity naming a parent session that has no row.
        conn.execute(
                "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
                 VALUES (2, 'orchestrator', 'identity',
                         '{\"parent_session_id\":777,\"workspace_id\":1,\"worktree_id\":1,\"item_id\":\"i\",\"task_goal\":\"\",\"operation_id\":0,\"ownership\":\"read_only_shared\",\"model\":\"\",\"created_ms\":1}',
                         1)",
                [],
            )
            .unwrap();
    }
    let report = doctor_run(dir.path(), true);
    assert!(report.issues >= 3, "{:?}", report.lines);
    let text = report.lines.join("\n");
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("orphan child: unparseable orchestrator registry row")),
        "{text}"
    );
    assert!(
        report.lines.iter().any(|l| {
            l.contains(
                "orphan child: child session 2 carries an identity row naming parent session 777",
            )
        }),
        "{text}"
    );
    assert!(
        report.lines.iter().any(|l| {
            l.contains("orphan child: non-terminal child child-2")
                && l.contains("no worktree directory")
        }),
        "{text}"
    );
    assert!(
        text.contains("orphan children: 1 child identity row(s), 2 registry row(s) scanned"),
        "{text}"
    );
    let _ = wt_row_id;
}

/// Economic audit item 7: the doctor's engineering-outcome ledger section is
/// a truthful read-only fold — an empty ledger says so, and a recorded run
/// is summarized with the durable metrics (never invented).
#[test]
fn doctor_reports_the_engineering_outcome_ledger_honestly() {
    use faktor_agent::{
        ChangedCodeScale, EconomicSettlement, FinalVerificationBasis, MeasuredMoney, MoneyState,
        OutcomeLedger, ProofCoverage, RunModelStep, SettlementStep, TerminalEngineeringOutcome,
    };
    use faktor_core::model::{RiskBucket, TaskClass};

    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l == "outcomes: no completed runs recorded"),
        "{:?}",
        report.lines
    );
    let record = TerminalEngineeringOutcome {
        schema_version: faktor_agent::OUTCOME_RECORD_SCHEMA_V,
        run_id: "doctor-run-1".into(),
        session_id: 1,
        task_id: 1,
        task_class: TaskClass::Medium,
        risk_class: RiskBucket::Low,
        candidate_snapshot: None,
        final_snapshot: "tm1:final".into(),
        accepted: true,
        integrated: true,
        verified_first_verification_pass: true,
        repair_attempts: 0,
        model_sequence: vec![RunModelStep {
            provider: "ollama".into(),
            model: "local-7b".into(),
            local: true,
            execution_class: faktor_core::model::ExecutionClass::Local,
        }],
        first_execution_class: faktor_core::model::ExecutionClass::Local,
        final_execution_class: faktor_core::model::ExecutionClass::Local,
        attribution_state: faktor_agent::AttributionState::Complete,
        wall_time_ms: 900,
        human_interventions: 0,
        proof: ProofCoverage {
            checks_passed: 3,
            checks_required: 3,
        },
        change_scale: ChangedCodeScale {
            files: 1,
            added_lines: 5,
            removed_lines: 1,
        },
        rollback_events: 0,
        recovery_events: 0,
        final_basis: FinalVerificationBasis::DeterministicChecks,
        frontier_escalations: 0,
        completed_at_ms: 1_700_000_000_000,
        verified_samples: vec![],
    };
    let settlement = EconomicSettlement {
        schema_version: faktor_agent::SETTLEMENT_SCHEMA_V,
        run_id: "doctor-run-1".into(),
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "doctor test".into(),
        model_spend_micro: Some(0),
        verification_spend_micro: Some(40_000),
        rework_spend_micro: Some(0),
        avoided_hosted: MeasuredMoney::measured(250_000),
        steps: vec![SettlementStep {
            provider: "ollama".into(),
            model: "local-7b".into(),
            local: true,
            execution_class: faktor_core::model::ExecutionClass::Local,
            spend_micro: 0,
        }],
        settled_at_ms: 1_700_000_000_000,
    };
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        OutcomeLedger::new(session.store())
            .record(&record, &settlement)
            .expect("a valid fixture record records exactly once");
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let line = report
        .lines
        .iter()
        .find(|l| l.starts_with("outcomes: "))
        .expect("ledger line");
    assert!(line.contains("1 run(s), 1 verified"), "{line}");
    assert!(line.contains("first-verification-pass 100.0%"), "{line}");
    assert!(
        line.contains("successful-run cost 40000 microUSD"),
        "{line}"
    );
    assert!(
        line.contains("all-in cost/verified change 40000 microUSD"),
        "{line}"
    );
    assert!(
        !line.contains("money-incomplete excluded"),
        "a fully settled cohort excludes nothing: {line}"
    );
    assert!(
        line.contains("local-class coverage measured"),
        "a fully boundary-classed ledger reports measured coverage: {line}"
    );
    assert!(
        !line.contains("attribution-incomplete"),
        "a complete-attribution ledger excludes nothing from learning: {line}"
    );

    // A second run whose money is still pending: excluded and counted, and
    // when NO run's money is settled the doctor says `unavailable`, never a
    // fabricated zero.
    let mut pending_record = record.clone();
    pending_record.run_id = "doctor-run-2".into();
    pending_record.model_sequence = vec![];
    pending_record.first_execution_class = faktor_core::model::ExecutionClass::Unknown;
    pending_record.final_execution_class = faktor_core::model::ExecutionClass::Unknown;
    let pending_settlement = EconomicSettlement {
        schema_version: faktor_agent::SETTLEMENT_SCHEMA_V,
        run_id: "doctor-run-2".into(),
        seq: 0,
        money_state: MoneyState::Pending,
        reason: "doctor test pending".into(),
        model_spend_micro: None,
        verification_spend_micro: None,
        rework_spend_micro: None,
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: vec![],
        settled_at_ms: 0,
    };
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        OutcomeLedger::new(session.store())
            .record(&pending_record, &pending_settlement)
            .expect("the pending fixture records exactly once");
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let line = report
        .lines
        .iter()
        .find(|l| l.starts_with("outcomes: "))
        .expect("ledger line");
    assert!(line.contains("2 run(s), 2 verified"), "{line}");
    assert!(
        line.contains("1 money-incomplete excluded (money metrics over 1 complete run(s))"),
        "{line}"
    );
    assert!(
        line.contains("successful-run cost 40000 microUSD"),
        "the complete cohort's money is unchanged by the excluded row: {line}"
    );

    // A third run with a classless boundary step and PARTIAL attribution:
    // the doctor reports the partial local coverage and the learning-feed
    // exclusion — a folded/classless history is never rounded to full
    // coverage and an attribution-incomplete run is never silent.
    let mut partial_record = record.clone();
    partial_record.run_id = "doctor-run-3".into();
    partial_record.model_sequence = vec![RunModelStep {
        provider: "hosted".into(),
        model: "m-unknown".into(),
        local: false,
        execution_class: faktor_core::model::ExecutionClass::Unknown,
    }];
    partial_record.first_execution_class = faktor_core::model::ExecutionClass::Unknown;
    partial_record.final_execution_class = faktor_core::model::ExecutionClass::Unknown;
    partial_record.attribution_state = faktor_agent::AttributionState::Partial;
    partial_record.verified_samples = vec![];
    let partial_settlement = EconomicSettlement {
        schema_version: faktor_agent::SETTLEMENT_SCHEMA_V,
        run_id: "doctor-run-3".into(),
        seq: 0,
        money_state: MoneyState::Pending,
        reason: "doctor test partial attribution".into(),
        model_spend_micro: None,
        verification_spend_micro: None,
        rework_spend_micro: None,
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: vec![],
        settled_at_ms: 0,
    };
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        OutcomeLedger::new(session.store())
            .record(&partial_record, &partial_settlement)
            .expect("the partial-attribution fixture records exactly once");
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let line = report
        .lines
        .iter()
        .find(|l| l.starts_with("outcomes: "))
        .expect("ledger line");
    assert!(
        line.contains("local-class coverage partial (1/2 classed)"),
        "one of two runs with steps carries boundary authority: {line}"
    );
    assert!(
        line.contains("1 attribution-incomplete excluded from learning (partial 1, unavailable 0)"),
        "the partial attribution is counted loudly: {line}"
    );
}

/// Item 6 (classification/parity): the doctor's cost lines are gated on
/// their OWN served denominators like the IDE panels — a money-complete run
/// that did NOT verify can never render the folded `0` as a measured cost,
/// and neither can a zero money-complete spend render a `0.0%` rework ratio.
/// A measured zero (verified money-complete changes with genuinely zero
/// spend) still renders as an exact zero, never `unavailable`.
#[test]
fn doctor_cost_lines_guard_their_real_denominators_never_a_fabricated_zero() {
    use faktor_agent::{
        ChangedCodeScale, EconomicSettlement, FinalVerificationBasis, MeasuredMoney, MoneyState,
        OutcomeLedger, ProofCoverage, RunModelStep, SettlementStep, TerminalEngineeringOutcome,
    };
    use faktor_core::model::{RiskBucket, TaskClass};

    fn record(
        run_id: &str,
        verified_basis: FinalVerificationBasis,
        spend: u64,
        rework: u64,
    ) -> (TerminalEngineeringOutcome, EconomicSettlement) {
        let outcome = TerminalEngineeringOutcome {
            schema_version: faktor_agent::OUTCOME_RECORD_SCHEMA_V,
            run_id: run_id.into(),
            session_id: 1,
            task_id: 1,
            task_class: TaskClass::Medium,
            risk_class: RiskBucket::Low,
            candidate_snapshot: None,
            final_snapshot: "tm1:final".into(),
            accepted: true,
            integrated: true,
            // Only a deterministic basis is a verified change; a review-only
            // run must never claim a first-verification-pass.
            verified_first_verification_pass: verified_basis.is_deterministic_verified(),
            repair_attempts: 0,
            model_sequence: vec![RunModelStep {
                provider: "hosted".into(),
                model: "m".into(),
                local: false,
                execution_class: faktor_core::model::ExecutionClass::HostedEconomy,
            }],
            first_execution_class: faktor_core::model::ExecutionClass::HostedEconomy,
            final_execution_class: faktor_core::model::ExecutionClass::HostedEconomy,
            attribution_state: faktor_agent::AttributionState::Complete,
            wall_time_ms: 900,
            human_interventions: 0,
            proof: ProofCoverage {
                checks_passed: 3,
                checks_required: 3,
            },
            change_scale: ChangedCodeScale {
                files: 1,
                added_lines: 5,
                removed_lines: 1,
            },
            rollback_events: 0,
            recovery_events: 0,
            final_basis: verified_basis,
            frontier_escalations: 0,
            completed_at_ms: 1_700_000_000_000,
            verified_samples: vec![],
        };
        let settlement = EconomicSettlement {
            schema_version: faktor_agent::SETTLEMENT_SCHEMA_V,
            run_id: run_id.into(),
            seq: 0,
            money_state: MoneyState::Complete,
            reason: "doctor denominator test".into(),
            model_spend_micro: Some(spend),
            verification_spend_micro: Some(0),
            rework_spend_micro: Some(rework.min(spend)),
            avoided_hosted: MeasuredMoney::Unavailable,
            steps: vec![SettlementStep {
                provider: "hosted".into(),
                model: "m".into(),
                local: false,
                execution_class: faktor_core::model::ExecutionClass::HostedEconomy,
                spend_micro: spend,
            }],
            settled_at_ms: 1_700_000_000_000,
        };
        (outcome, settlement)
    }

    // Case 1: one money-complete run that did NOT verify (review-only basis)
    // with real spend. Every verified-denominator figure is `unavailable`;
    // the rework ratio still measures over the complete cohort's spend.
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let (outcome, settlement) = record(
        "doctor-unverified-1",
        FinalVerificationBasis::ReviewOnly,
        40_000,
        0,
    );
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        OutcomeLedger::new(session.store())
            .record(&outcome, &settlement)
            .expect("the unverified fixture records");
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let line = report
        .lines
        .iter()
        .find(|l| l.starts_with("outcomes: "))
        .expect("ledger line");
    assert!(
        line.contains("first-verification-pass unavailable"),
        "zero verified changes is an empty denominator: {line}"
    );
    assert!(
        line.contains("successful-run cost unavailable"),
        "no money-complete verified change must never render a folded $0: {line}"
    );
    assert!(
        line.contains("all-in cost/verified change unavailable"),
        "VCSC's denominator is verified money-complete changes, not complete runs: {line}"
    );
    assert!(
        line.contains("rework ratio 0.0%"),
        "a nonzero complete-cohort spend measures a real zero rework ratio: {line}"
    );

    // Case 2: one money-complete VERIFIED run whose spend is a measured zero
    // — the cost lines render the exact zero (measured), while the rework
    // ratio's empty spend denominator is `unavailable`.
    let zero_dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(zero_dir.path().join("store"), zero_dir.path().join("cas"))
                .unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let (outcome, settlement) = record(
        "doctor-local-zero-1",
        FinalVerificationBasis::DeterministicChecks,
        0,
        0,
    );
    {
        let session =
            SessionManager::open_quick(zero_dir.path().join("store"), zero_dir.path().join("cas"))
                .unwrap();
        OutcomeLedger::new(session.store())
            .record(&outcome, &settlement)
            .expect("the local-zero fixture records");
    }
    let report = doctor_run(zero_dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let line = report
        .lines
        .iter()
        .find(|l| l.starts_with("outcomes: "))
        .expect("ledger line");
    assert!(
        line.contains("first-verification-pass 100.0%"),
        "a verified cohort measures the real rate: {line}"
    );
    assert!(
        line.contains("successful-run cost 0 microUSD"),
        "a measured zero is an exact zero, never `unavailable`: {line}"
    );
    assert!(
        line.contains("rework ratio unavailable"),
        "zero complete-cohort spend is an empty rework denominator: {line}"
    );
}

/// Audit item 5: the doctor certifies the native engineering-contract
/// compiler with a deterministic self-check (non-empty contract, bound
/// selector obligation, byte-identical repeated compile). A broken compiler
/// is an ISSUE, never silence.
#[test]
fn doctor_certifies_the_native_contract_compiler_deterministically() {
    let mut lines: Vec<String> = Vec::new();
    let mut issues = 0usize;
    doctor_contract_compiler_line(&mut lines, &mut issues);
    assert_eq!(issues, 0, "{lines:?}");
    let line = lines
        .iter()
        .find(|l| l.starts_with("engineering-contract compiler:"))
        .expect("the compiler line");
    assert!(line.contains("state=verified"), "{line}");
    assert!(line.contains("determinism=byte_identical"), "{line}");
    // The line is stable across runs (the compile is deterministic).
    let mut again: Vec<String> = Vec::new();
    let mut again_issues = 0usize;
    doctor_contract_compiler_line(&mut again, &mut again_issues);
    assert_eq!(again_issues, 0);
    assert_eq!(lines, again);
}

/// Marketplace audit item 8: the doctor reports one informational
/// live-canary line per marketplace source (`configured` when every opt-in
/// canary environment variable is present, else `absent`) and absence is
/// never an issue.
#[test]
fn doctor_reports_live_canary_status_per_source_without_failing() {
    let mut lines: Vec<String> = Vec::new();
    doctor_live_canary_line(&mut lines);
    for source in ["mouser", "digikey", "lcsc", "1688", "alibaba"] {
        let prefix = format!("live-canary {source}: ");
        let line = lines
            .iter()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("missing live-canary line for {source}: {lines:?}"));
        let status = line.strip_prefix(&prefix).expect("prefix");
        assert!(
            status == "configured" || status == "absent",
            "unexpected live-canary status: {line}"
        );
    }
    // The full doctor path prints the same section and stays green on a
    // healthy store with default (absent) canary configuration.
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l == "live-canary mouser: configured" || l == "live-canary mouser: absent"),
        "{:?}",
        report.lines
    );
}

/// Audit items 6 + 17: doctor reports the agent-HOME contract and the
/// typed first-run choice honestly — the scratch default with the
/// kernel-denial state of this platform, and (on backend-less platforms)
/// the pending choice with BOTH options and the exact trade-off; an
/// unreadable decision document is an ISSUE while the state stays
/// fail-closed; a recorded grant is surfaced as strictly weaker.
#[test]
fn doctor_reports_agent_home_and_first_run_choice_honestly() {
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let home_line = report
        .lines
        .iter()
        .find(|l| l.starts_with("sandbox agent home:"))
        .expect("agent home line");
    assert!(
        home_line.contains("policy=scratch") && home_line.contains("scratch_root="),
        "{home_line}"
    );
    let creds = report
        .lines
        .iter()
        .find(|l| l.starts_with("sandbox credential dirs:"))
        .expect("credential dirs line");
    if faktor_sandbox::filesystem_backend_available() {
        assert!(creds.contains("kernel-denied"), "{creds}");
        assert!(creds.contains("~/.ssh"), "{creds}");
    } else {
        assert!(
            creds.contains("honest limitation") && creds.contains(std::env::consts::OS),
            "a missing backend must be an honest limitation, never a claim: {creds}"
        );
        assert!(creds.contains("scratch HOME"), "{creds}");
    }
    let choice = report
        .lines
        .iter()
        .find(|l| l.starts_with("sandbox first-run choice:"))
        .expect("first-run choice line");
    assert!(choice.contains("platform="), "{choice}");
    assert!(choice.contains("network_backend="), "{choice}");
    if faktor_terminal::broker_only_supported() {
        assert!(choice.contains("decision=fail_closed"), "{choice}");
    } else {
        assert!(choice.contains("decision=pending"), "{choice}");
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("--network-capable-shell")
                    && l.contains(config::NETWORK_CAPABLE_SHELL_TRADE_OFF)),
            "the pending presentation names both options and the trade-off: {:?}",
            report.lines
        );
    }

    // A recorded grant: doctor reports it and the agent class contract as
    // the app-level network-capable mode.
    config::record_decision(
        dir.path(),
        config::SandboxShellChoice::NetworkCapableUserGranted,
        7,
    )
    .unwrap();
    let report = doctor_run(dir.path(), false);
    assert_eq!(report.issues, 0, "{:?}", report.lines);
    let choice = report
        .lines
        .iter()
        .find(|l| l.starts_with("sandbox first-run choice:"))
        .expect("choice line");
    assert!(
        choice.contains("decision=network_capable_user_granted"),
        "{choice}"
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("best_effort") && l.contains("agent shell tool")),
        "the granted agent class must show the app-level guarantee: {:?}",
        report.lines
    );

    // A corrupt decision document: ISSUE, and never a grant.
    std::fs::write(config::decision_path(dir.path()), b"{corrupt").unwrap();
    let report = doctor_run(dir.path(), false);
    assert!(report.issues >= 1, "{:?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("decision unreadable")),
        "{:?}",
        report.lines
    );
}

/// The explicit operator real-home grant is surfaced as a grant on the
/// doctor home line, and the default is never presented as such.
#[test]
fn doctor_surfaces_the_explicit_real_home_grant() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("home-grant.json");
    std::fs::write(
        &cfg,
        r#"{"model": "m", "sandbox": {"home": "operator_real_home"}}"#,
    )
    .unwrap();
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_sandbox_agent_home_line(dir.path(), Some(&cfg), &mut lines, &mut issues);
    assert_eq!(issues, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("policy=operator_real_home")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("EXPLICIT operator grant")),
        "{lines:?}"
    );
    // Default: scratch, no grant line.
    let mut lines = Vec::new();
    let mut issues = 0usize;
    doctor_sandbox_agent_home_line(dir.path(), None, &mut lines, &mut issues);
    assert!(lines[0].contains("policy=scratch"), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("EXPLICIT operator grant")),
        "{lines:?}"
    );
}
