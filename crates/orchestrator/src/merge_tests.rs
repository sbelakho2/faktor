use super::*;
use faktor_core::authority::AuthorityDigestKind;

/// A canonical regular (100644) entry state for a hex payload digest.
fn state(hex: &str) -> EntryState {
    EntryState::regular(
        CanonicalMode::RegularFile,
        FileHash::from_hex(hex).expect("hex"),
    )
    .expect("regular state")
}

fn entry(path: &str, child: Option<&str>, base: Option<&str>) -> ChangeEntry {
    ChangeEntry {
        path: PathBuf::from(path),
        child_hash: child.map(|h| FileHash::from_hex(h).expect("hex")),
        base_hash: base.map(|h| FileHash::from_hex(h).expect("hex")),
        child: child.map(state),
        base: base.map(state),
    }
}

fn cs(files: Vec<ChangeEntry>) -> ChangeSet {
    ChangeSet {
        child_id: "child-0".into(),
        base_id: "base-child-0".into(),
        run_base_snapshot: None,
        child_start_snapshot: None,
        final_child_snapshot: None,
        files,
        created_ms: 1,
    }
}

const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn change_set_and_base_map_ids_are_canonical_stable_and_one_byte_sensitive() {
    let set = cs(vec![entry("src/a.rs", Some(H), Some(H2))]);
    let id = set.id();
    assert!(id.starts_with("cs-"), "{id}");
    let body = id.strip_prefix("cs-").unwrap();
    assert_eq!(body.len(), 64, "the id body is a full BLAKE3 hex: {id}");
    assert_eq!(
        classify_authority_digest(body),
        AuthorityDigestKind::Blake3Hex,
        "change-set ids are canonical BLAKE3, never a 64-bit FNV fold"
    );
    // Identical content re-derives the identical id, including through
    // the durable serde shape a reopen decodes.
    assert_eq!(set.id(), id);
    let json = serde_json::to_string(&set).unwrap();
    let decoded: ChangeSet = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.id(), id, "the content id survives decode/reopen");
    // A one-byte payload drift derives a different id.
    let drifted = cs(vec![entry("src/a.rs", Some(H2), Some(H2))]);
    assert_ne!(drifted.id(), id);
    let renamed = cs(vec![entry("src/b.rs", Some(H), Some(H2))]);
    assert_ne!(renamed.id(), id);

    // Base-map identity: canonical BLAKE3, stable and one-byte sensitive.
    let map_a = vec![(PathBuf::from("a.rs"), state(H))];
    let map_b = vec![(PathBuf::from("a.rs"), state(H2))];
    assert_eq!(base_map_digest(&map_a), base_map_digest(&map_a.clone()));
    assert_ne!(base_map_digest(&map_a), base_map_digest(&map_b));
    let map_digest = base_map_digest(&map_a);
    assert_eq!(map_digest.len(), 64);
    assert_eq!(
        classify_authority_digest(&map_digest),
        AuthorityDigestKind::Blake3Hex
    );
}

#[test]
fn change_set_ids_survive_a_real_reopen_and_legacy_rows_never_authorize() {
    let dir = tempfile::tempdir().unwrap();
    let m = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let parent = m
        .create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
        .unwrap()
        .id();
    let set = cs(vec![entry("src/a.rs", Some(H), Some(H2))]);
    put_change_set(&m, parent, "run-1", &set).unwrap();
    let id = set.id();
    drop(m);
    let m2 = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let read = read_change_set(&m2, parent, "run-1", "child-0", &id).unwrap();
    assert_eq!(read.id(), id, "the content id survives a real reopen");
    assert_eq!(read.id(), set.id());

    // A stored row without the canonical identity marker decodes for
    // VIEWING but is refused by the authorization read: it forces a
    // restage/reverification instead of an integration.
    let handle = parent_handle(&m2, parent).unwrap();
    let legacy_key = cs_key("run-legacy", "child-0", "cs-legacy");
    let chunks = pack_chunks(&set.files).unwrap();
    let legacy_header = serde_json::json!({
        "child_id": "child-0",
        "base_id": "base-child-0",
        "cs_id": "cs-legacy",
        "files": set.files.len(),
        "chunks": chunks.len(),
        "created_ms": 1,
    })
    .to_string();
    put_chunks(&handle, KIND_CS, &legacy_key, &legacy_header, &chunks).unwrap();
    let viewed = read_change_set(&m2, parent, "run-legacy", "child-0", "cs-legacy").unwrap();
    assert_eq!(
        viewed.id(),
        set.id(),
        "legacy rows still decode for viewing"
    );
    let err = read_change_set_authoritative(&m2, parent, "run-legacy", "child-0", "cs-legacy")
        .unwrap_err();
    assert!(
        err.to_string().contains("legacy 64-bit FNV identity"),
        "{err}"
    );
}

#[test]
fn compute_entries_skips_unchanged_and_anchors_deletes_and_creates() {
    // Seeded start (copy of the parent): modify + delete + unchanged.
    let start = vec![
        (PathBuf::from("a.rs"), state(H)),
        (PathBuf::from("gone.rs"), state(H2)),
        (PathBuf::from("same.rs"), state(H)),
    ];
    let now = vec![
        (PathBuf::from("a.rs"), state(H2)),
        (PathBuf::from("same.rs"), state(H)),
        (PathBuf::from("new.rs"), state(H)),
    ];
    let parent_base = vec![
        (PathBuf::from("a.rs"), state(H)),
        (PathBuf::from("gone.rs"), state(H2)),
        (PathBuf::from("same.rs"), state(H)),
    ];
    let entries = compute_change_entries("c", &start, &now, &parent_base).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path, PathBuf::from("a.rs"));
    assert_eq!(entries[0].child_state(), Some(state(H2)));
    assert_eq!(entries[0].base_state(), Some(state(H)));
    assert_eq!(entries[1].path, PathBuf::from("gone.rs"));
    assert_eq!(entries[1].child_state(), None);
    assert_eq!(entries[1].base_state(), Some(state(H2)));
    assert_eq!(entries[2].path, PathBuf::from("new.rs"));
    assert_eq!(entries[2].child_state(), Some(state(H)));
    assert_eq!(entries[2].base_state(), None);
}

#[test]
fn compute_entries_refuses_unsound_deletion_and_caps_loudly() {
    // The child deleted a file its start map had, but the parent base
    // map never contained it: staging an unsound deletion is refused.
    let start = vec![(PathBuf::from("x"), state(H))];
    let now: Vec<(PathBuf, EntryState)> = vec![];
    let parent_base: Vec<(PathBuf, EntryState)> = vec![];
    let err = compute_change_entries("c", &start, &now, &parent_base).unwrap_err();
    assert!(matches!(err, ExecError::InvalidState(_)), "{err:?}");
    // An empty-seeded wave-12 child that wrote one file beyond the cap
    // must fail loudly at MAX_CHANGES, never truncate.
    let many: Vec<(PathBuf, EntryState)> = (0..(MAX_CHANGES + 1))
        .map(|i| (PathBuf::from(format!("f{i:05}.rs")), state(H)))
        .collect();
    let err = compute_change_entries("c", &[], &many, &[]).unwrap_err();
    assert!(matches!(err, ExecError::Oversized(_)), "{err:?}");
    let exactly: Vec<(PathBuf, EntryState)> = (0..MAX_CHANGES)
        .map(|i| (PathBuf::from(format!("f{i:05}.rs")), state(H)))
        .collect();
    assert_eq!(
        compute_change_entries("c", &[], &exactly, &[])
            .unwrap()
            .len(),
        MAX_CHANGES
    );
}

#[test]
fn decision_must_cover_every_file_and_reject_hostile_paths() {
    let c = cs(vec![
        entry("src/a.rs", Some(H), Some(H2)),
        entry("src/b.rs", Some(H), None),
    ]);
    // Undecided b: explicit error, nothing merges.
    let err = validate_decision(&c, &[PathBuf::from("src/a.rs")], &[]).unwrap_err();
    assert!(matches!(err, ExecError::UndecidedPaths(_)), "{err:?}");
    assert!(format!("{err:?}").contains("src/b.rs"));
    // Traversal / absolute / dot components are typed InvalidApproval.
    for evil in ["../evil", "/etc/passwd", "src/../a.rs", "./src/a.rs", ".."] {
        let err = validate_decision(
            &c,
            &[PathBuf::from(evil), PathBuf::from("src/b.rs")],
            &[PathBuf::from("src/a.rs")],
        )
        .unwrap_err();
        assert!(
            matches!(err, ExecError::InvalidApproval(_)),
            "{evil:?} must be a typed invalid approval: {err:?}"
        );
    }
    // Case-variant approval is a typed error with a case hint.
    let err = validate_decision(
        &c,
        &[PathBuf::from("SRC/A.RS"), PathBuf::from("src/b.rs")],
        &[PathBuf::from("src/a.rs")],
    )
    .unwrap_err();
    assert!(
        matches!(err, ExecError::InvalidApproval(_)) && format!("{err:?}").contains("case"),
        "{err:?}"
    );
    // Unknown paths and approve/reject overlap are typed errors.
    let err = validate_decision(
        &c,
        &[PathBuf::from("nope.rs"), PathBuf::from("src/b.rs")],
        &[PathBuf::from("src/a.rs")],
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)), "{err:?}");
    let err = validate_decision(
        &c,
        &[PathBuf::from("src/a.rs")],
        &[PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)), "{err:?}");
    // Duplicates are refused.
    let err = validate_decision(
        &c,
        &[
            PathBuf::from("src/a.rs"),
            PathBuf::from("src/a.rs"),
            PathBuf::from("src/b.rs"),
        ],
        &[],
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)), "{err:?}");
    // A complete, disjoint decision passes and round-trips sorted.
    let (a, r) = validate_decision(
        &c,
        &[PathBuf::from("src/b.rs"), PathBuf::from("src/a.rs")],
        &[],
    )
    .unwrap();
    assert_eq!(
        a,
        vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
    );
    assert!(r.is_empty());
}

#[test]
fn pack_chunks_roundtrips_and_empty_payloads_stay_readable() {
    let items: Vec<ChangeEntry> = (0..1000)
        .map(|i| entry(&format!("dir/f{i:04}.rs"), Some(H), None))
        .collect();
    let chunks = pack_chunks(&items).unwrap();
    assert!(chunks.len() > 1, "chunking splits big payloads");
    for c in &chunks {
        assert!(c.len() <= CHUNK_BUDGET, "chunk of {} bytes", c.len());
    }
    let back: Vec<ChangeEntry> = unpack_chunks(&chunks).unwrap();
    assert_eq!(back, items);
    // Empty payload: one readable `[]` chunk.
    let chunks = pack_chunks::<ChangeEntry>(&[]).unwrap();
    assert_eq!(chunks, vec!["[]".to_string()]);
    let back: Vec<ChangeEntry> = unpack_chunks(&chunks).unwrap();
    assert!(back.is_empty());
}

#[test]
fn stored_rows_decode_never_accepts_hostile_values() {
    // Unpacked values pass through typed validators: a hostile path in
    // a stored row must be caught by validate_rel_path_str.
    let err = validate_rel_path_str("a/../../b").unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)));
    let err = validate_rel_path_str("/abs").unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)));
    let err = validate_rel_path_str("").unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)));
    assert!(validate_rel_path_str("src/a.rs").is_ok());
    assert!(validate_rel_path_str(&"p".repeat(MAX_DECISION_PATH_CHARS)).is_ok());
    let err = validate_rel_path_str(&"p".repeat(MAX_DECISION_PATH_CHARS + 1)).unwrap_err();
    assert!(matches!(err, ExecError::InvalidApproval(_)));
}

// ------------------------------------------------ semantic preflight
// (audit 79) + risk-adjusted ceilings (audit 54)

use faktor_semantic::{RiskLevel, SemanticCapabilities};
use faktor_semantic::{
    SemanticDelta, SemanticDeltaChange, SemanticDeltaKind, SemanticEntityId, SemanticEntityRef,
    SemanticEnvelope, SemanticProviderId, WorkspacePath,
};

const H_BASE: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const H_CHILD: &str = "0202020202020202020202020202020202020202020202020202020202020202";
const H_OTHER: &str = "0303030303030303030303030303030303030303030303030303030303030303";

fn delta_change(
    path: &str,
    kind: SemanticDeltaKind,
    old: Option<&str>,
    new: Option<&str>,
) -> SemanticDeltaChange {
    SemanticDeltaChange {
        entity: SemanticEntityRef::new(
            WorkspaceId::new(1),
            WorkspacePath::parse(path).unwrap(),
            SemanticEntityId::parse(path).unwrap(),
        ),
        kind,
        old_hash: old.map(|h| FileHash::from_hex(h).unwrap()),
        new_hash: new.map(|h| FileHash::from_hex(h).unwrap()),
    }
}

fn provider_delta(changes: Vec<SemanticDeltaChange>) -> SemanticDelta {
    let workspace = WorkspaceId::new(1);
    let provider = SemanticProviderId::parse("fake-delta").unwrap();
    let snapshot = SemanticSnapshotId::derive(
        workspace,
        "candidate",
        &provider,
        1,
        SEMANTIC_SCHEMA_VERSION,
    );
    SemanticDelta {
        workspace,
        from_snapshot: snapshot,
        to_snapshot: snapshot,
        changes,
        degraded: false,
    }
}

#[test]
fn semantic_delta_conflicts_are_typed_and_bounded() {
    let cs = cs(vec![entry("src/a.rs", Some(H_CHILD), Some(H_BASE))]);
    let approved = vec![PathBuf::from("src/a.rs")];
    // Consistent delta: nothing.
    assert!(semantic_delta_conflicts(
        &cs,
        &provider_delta(vec![delta_change(
            "src/a.rs",
            SemanticDeltaKind::Modified,
            Some(H_BASE),
            Some(H_CHILD),
        )]),
        &approved,
    )
    .is_empty());
    // Candidate hash contradiction: typed conflict with the path.
    let conflicts = semantic_delta_conflicts(
        &cs,
        &provider_delta(vec![delta_change(
            "src/a.rs",
            SemanticDeltaKind::Modified,
            Some(H_BASE),
            Some(H_OTHER),
        )]),
        &approved,
    );
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].0, PathBuf::from("src/a.rs"));
    assert!(conflicts[0].1.contains("disagrees"), "{:?}", conflicts[0]);
    assert!(conflicts[0].1.contains(H_OTHER), "{:?}", conflicts[0]);
    // Base hash contradiction: conflict.
    let conflicts = semantic_delta_conflicts(
        &cs,
        &provider_delta(vec![delta_change(
            "src/a.rs",
            SemanticDeltaKind::Modified,
            Some(H_OTHER),
            Some(H_CHILD),
        )]),
        &approved,
    );
    assert_eq!(conflicts.len(), 1);
    assert!(
        conflicts[0].1.contains("composed over base"),
        "{:?}",
        conflicts[0]
    );
    // Removal contradiction (provider removes what the candidate keeps).
    let conflicts = semantic_delta_conflicts(
        &cs,
        &provider_delta(vec![delta_change(
            "src/a.rs",
            SemanticDeltaKind::Removed,
            Some(H_BASE),
            None,
        )]),
        &approved,
    );
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].1.contains("removes"), "{:?}", conflicts[0]);
    // A path outside the approved set is ignored.
    assert!(semantic_delta_conflicts(
        &cs,
        &provider_delta(vec![delta_change(
            "src/other.rs",
            SemanticDeltaKind::Modified,
            Some(H_BASE),
            Some(H_OTHER),
        )]),
        &approved,
    )
    .is_empty());
}

#[test]
fn risk_adjusted_ceilings_reduce_only_on_escalation() {
    let base = Ceilings {
        max_live: 8,
        max_reasoning_active: 4,
        max_mutating_active: 2,
    };
    assert_eq!(risk_adjusted_ceilings(&base, None), base);
    assert_eq!(risk_adjusted_ceilings(&base, Some(RiskLevel::Safe)), base);
    assert_eq!(risk_adjusted_ceilings(&base, Some(RiskLevel::Low)), base);
    let high = risk_adjusted_ceilings(&base, Some(RiskLevel::High));
    assert_eq!(high.max_mutating_active, 1);
    assert_eq!(high.max_reasoning_active, 1);
    assert_eq!(
        high.max_live, base.max_live,
        "the hard live bound is untouched"
    );
    let unknown = risk_adjusted_ceilings(&base, Some(RiskLevel::Unknown));
    assert_eq!(unknown.max_mutating_active, 1);
    let wide = Ceilings {
        max_live: 8,
        max_reasoning_active: 6,
        max_mutating_active: 5,
    };
    let medium = risk_adjusted_ceilings(&wide, Some(RiskLevel::Medium));
    assert_eq!(medium.max_mutating_active, 2);
    assert_eq!(medium.max_reasoning_active, 2);
}

mod preflight_integration {
    use super::*;
    use faktor_agent::{AgentDeps, AgentRuntime, NoEvidence, PermissionRequester};
    use faktor_core::capability::PermissionDecision;
    use faktor_core::id::{TaskId, WorktreeId};

    struct AlwaysAllow;

    impl PermissionRequester for AlwaysAllow {
        fn request(
            &self,
            _session: SessionId,
            _permission: &faktor_session::ops::PermissionRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
        > {
            Box::pin(async { Ok(PermissionDecision::Allow) })
        }
    }

    /// One isolated child ready to merge `src/a.rs` (v1 -> v2), plus a
    /// parent tree. `semantic` is the registry the agent carries.
    struct Fixture {
        orch: Arc<OrchestratorRuntime>,
        parent: SessionId,
        owner_root: PathBuf,
        cs: ChangeSet,
        _dir: tempfile::TempDir,
    }

    fn build_fixture(semantic: Arc<faktor_semantic::SemanticProviderRegistry>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let owner_root = dir.path().join("owner");
        std::fs::create_dir_all(owner_root.join("src")).unwrap();
        std::fs::write(owner_root.join("src/a.rs"), b"v1-base").unwrap();
        let owner_ws = manager
            .create_workspace(owner_root.to_str().unwrap())
            .unwrap();
        let owner_wt = WorktreeId::new(
            manager
                .put_worktree(owner_ws, owner_root.to_str().unwrap(), "main")
                .unwrap() as u64,
        );
        let parent = manager
            .create_session(owner_ws, "orchestrator", "fake", "m")
            .unwrap()
            .id();
        manager
            .adopt_identity(parent, owner_wt, TaskId::new(1))
            .unwrap();

        let child_dir = dir.path().join("isolated/run-1/child-0");
        std::fs::create_dir_all(child_dir.join("src")).unwrap();
        std::fs::write(child_dir.join("src/a.rs"), b"v2-child").unwrap();
        let child_ws = manager
            .create_workspace(child_dir.to_str().unwrap())
            .unwrap();
        let child_wt = WorktreeId::new(
            manager
                .put_worktree(child_ws, child_dir.to_str().unwrap(), "child")
                .unwrap() as u64,
        );

        let mut providers = faktor_provider::ProviderRegistry::new();
        providers
            .try_register(Arc::new(faktor_provider::FakeProvider::with_script(
                "fake",
                faktor_core::model::ModelCapabilities {
                    tools: true,
                    ..Default::default()
                },
                vec![],
            )))
            .unwrap();

        let deps = AgentDeps {
            session: manager.clone(),
            providers: Arc::new(providers),
            chunk_sink: None,
            permission_requester: Arc::new(AlwaysAllow),
            evidence: Arc::new(NoEvidence),
            tools: Arc::new(faktor_agent::ToolRegistry::new()),
            cas: Some(Arc::new(
                faktor_cas::Cas::open(dir.path().join("cas")).unwrap(),
            )),
            workspaces: faktor_fs::WorkspaceFileService::new(),
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            verification: faktor_agent::VerificationService::disabled(),
            hooks: None,
            instructions_resolver: faktor_instructions::no_roots_resolver(),
            routing: faktor_agent::FixedRoutingPolicy::passthrough(),
            budgets: Arc::new(faktor_session::NoopBudget),
            model: "m".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "test agent".into(),
            clock: Arc::new(faktor_core::time::SystemClock),
            tool_call_mode: faktor_agent::ToolCallMode::Native,
            tool_deadline_ms: 2000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic,
            context_prior: None,
            secret_registry: None,
            efficiency: Default::default(),
        };
        let agent = AgentRuntime::new(deps).unwrap();
        let orch = OrchestratorRuntime::new(manager.clone(), agent);

        // Durable plan row (owner root is what the merge applies into).
        let plan = crate::TaskPlan {
            goal: "merge test".into(),
            non_goals: vec![],
            constraints: vec![],
            work_items: vec![],
        };
        let owner = OwnerContext {
            parent_session: parent,
            workspace_id: owner_ws.raw(),
            worktree_id: owner_wt.raw(),
            root: owner_root.clone(),
        };
        let config = ExecConfig {
            run_id: "run-1".into(),
            ceilings: Ceilings::default(),
            parent_caps: CapabilitySet::new(),
            provider: "fake".into(),
            default_model: "m".into(),
            isolated_root: dir.path().join("isolated"),
            crash_seam: None,
        };
        orch.put_plan_row(&plan, &owner, &config, &[]).unwrap();

        // Durable child registry row: Done + isolated worktree.
        // A REAL child session carries the durable identity (the
        // semantic requirement flag lives there).
        let child_session = manager
            .create_session(child_ws, "child-0", "fake", "m")
            .unwrap()
            .id();
        let row = ChildRuntime {
            child_id: "child-0".into(),
            parent_session_id: parent.raw(),
            run_id: "run-1".into(),
            item_id: "impl".into(),
            kind: WorkKind::Implementation,
            session_id: child_session.raw() as u64,
            operation_id: 0,
            workspace_id: child_ws.raw(),
            worktree_id: child_wt.raw(),
            ownership: ChildOwnership::IsolatedWorktree,
            ownership_paths: vec![],
            state: ChildState::Done,
            budget_max_tokens: None,
            permissions: CapabilitySet::new(),
            model_policy: ModelPolicy { model: None },
            blocker_kind: None,
            blocker_reason: None,
            blocker_dependency: None,
            blocker_resolution: None,
            last_progress_ms: None,
            created_ms: 1,
            updated_ms: 1,
            base_snapshot_id: Some("base-child-0".into()),
            run_base_snapshot: None,
            env_snapshot_id: None,
            execution_phase: ExecutionPhase::default(),
        };
        let handle = manager.get_session(parent).unwrap().unwrap();
        handle
            .upsert_memory_fact(
                REGISTRY_ROW_KIND,
                "run-1/child-0",
                &serde_json::to_string(&row).unwrap(),
            )
            .unwrap();

        // Base maps: the parent tree and the child's spawn state.
        let base = EntryState::regular(
            CanonicalMode::RegularFile,
            FileHash::from(*blake3::hash(b"v1-base").as_bytes()),
        )
        .unwrap();
        put_base_map(
            &manager,
            parent,
            "run-1",
            "child-0",
            "parent",
            std::slice::from_ref(&(PathBuf::from("src/a.rs"), base.clone())),
        )
        .unwrap();
        put_base_map(
            &manager,
            parent,
            "run-1",
            "child-0",
            "start",
            &[(PathBuf::from("src/a.rs"), base)],
        )
        .unwrap();

        let cs = orch.stage_child_changes("child-0").unwrap();
        assert_eq!(
            cs.files[0].child_hash,
            Some(FileHash::from(*blake3::hash(b"v2-child").as_bytes()))
        );
        {
            let handle = manager.get_session(child_session).unwrap().unwrap();
            handle
                .orchestrator_child_identity_put(&faktor_session::child::ChildIdentity {
                    parent_session_id: parent,
                    workspace_id: child_ws.raw(),
                    worktree_id: child_wt.raw(),
                    item_id: "impl".into(),
                    task_goal: "child goal".into(),
                    operation_id: 0,
                    ownership: ChildOwnership::IsolatedWorktree,
                    model: "m".into(),
                    require_semantic_delta: false,
                    permissions: faktor_core::CapabilitySet::ALL,
                    created_ms: 1,
                })
                .unwrap();
        }
        Fixture {
            orch,
            parent,
            owner_root,
            cs,
            _dir: dir,
        }
    }

    /// A provider that composes deltas; `new_hash` selects the reported
    /// candidate hash for `src/a.rs` (None => a REMOVED change).
    struct ScriptedDeltaProvider {
        new_hash: Option<FileHash>,
    }

    impl faktor_semantic::SemanticProvider for ScriptedDeltaProvider {
        fn id(&self) -> SemanticProviderId {
            SemanticProviderId::parse("scripted-delta").unwrap()
        }

        fn version(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> SemanticCapabilities {
            SemanticCapabilities::DELTA.with_compose_delta(true)
        }

        fn delta(
            &self,
            request: faktor_semantic::SemanticDeltaRequest,
        ) -> faktor_semantic::BoxFuture<
            '_,
            Result<SemanticEnvelope<SemanticDelta>, faktor_semantic::SemanticError>,
        > {
            let workspace = request.workspace;
            let from_snapshot = request.from_snapshot;
            let provider = SemanticProviderId::parse("scripted-delta").unwrap();
            let new_hash = self.new_hash;
            Box::pin(async move {
                let to_snapshot = SemanticSnapshotId::derive(
                    workspace,
                    &request.to_source_revision,
                    &provider,
                    1,
                    SEMANTIC_SCHEMA_VERSION,
                );
                let base = FileHash::from(*blake3::hash(b"v1-base").as_bytes());
                let change = match new_hash {
                    Some(hash) => SemanticDeltaChange {
                        entity: SemanticEntityRef::new(
                            workspace,
                            WorkspacePath::parse("src/a.rs").unwrap(),
                            SemanticEntityId::parse("src/a.rs").unwrap(),
                        ),
                        kind: SemanticDeltaKind::Modified,
                        old_hash: Some(base),
                        new_hash: Some(hash),
                    },
                    None => SemanticDeltaChange {
                        entity: SemanticEntityRef::new(
                            workspace,
                            WorkspacePath::parse("src/a.rs").unwrap(),
                            SemanticEntityId::parse("src/a.rs").unwrap(),
                        ),
                        kind: SemanticDeltaKind::Removed,
                        old_hash: Some(base),
                        new_hash: None,
                    },
                };
                Ok(SemanticEnvelope::new(
                    provider,
                    1,
                    workspace,
                    to_snapshot,
                    0,
                    SemanticDelta {
                        workspace,
                        from_snapshot,
                        to_snapshot,
                        changes: vec![change],
                        degraded: false,
                    },
                ))
            })
        }
    }

    fn registry_with(
        provider: ScriptedDeltaProvider,
    ) -> Arc<faktor_semantic::SemanticProviderRegistry> {
        let mut registry = faktor_semantic::SemanticProviderRegistry::new(
            faktor_semantic::GenericSemanticFallback::default(),
        );
        registry.register(Arc::new(provider));
        Arc::new(registry)
    }

    fn staged_child_hash() -> FileHash {
        FileHash::from(*blake3::hash(b"v2-child").as_bytes())
    }

    fn other_hash() -> FileHash {
        FileHash::from(*blake3::hash(b"not-the-staged-content").as_bytes())
    }

    fn parent_bytes(fixture: &Fixture) -> Vec<u8> {
        std::fs::read(fixture.owner_root.join("src/a.rs")).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fake_semantic_conflict_blocks_with_a_typed_outcome_before_any_apply() {
        // The provider's delta contradicts the staged candidate hash:
        // the preflight refuses with the TYPED SemanticConflict variant
        // before any durable row or file apply; the parent is untouched.
        let fixture = build_fixture(registry_with(ScriptedDeltaProvider {
            new_hash: Some(other_hash()),
        }));
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("a semantic conflict must block the merge");
        match &err {
            ExecError::SemanticConflict(detail) => {
                assert!(detail.contains("src/a.rs"), "{detail}");
                assert!(detail.contains("scripted-delta"), "{detail}");
            }
            other => panic!("expected SemanticConflict, got {other:?}"),
        }
        assert_eq!(parent_bytes(&fixture), b"v1-base", "nothing applied");
        assert!(
            merge_envelopes(&fixture.orch.manager, fixture.parent, "run-1", "child-0")
                .unwrap()
                .is_empty(),
            "nothing durable was written before the refusal"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_cas_conflict_still_wins_over_a_provider_that_sees_no_conflict() {
        // The parent moved after the base snapshot AND the provider
        // reports a CONSISTENT delta (it sees no semantic conflict):
        // the real CAS conflict is reported exactly as without a
        // provider — a semantic answer can never clear it.
        let fixture = build_fixture(registry_with(ScriptedDeltaProvider {
            new_hash: Some(staged_child_hash()),
        }));
        std::fs::write(fixture.owner_root.join("src/a.rs"), b"v3-parent-moved").unwrap();
        let outcome = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect("the merge returns with the real conflict");
        assert!(outcome.merged.is_empty(), "{outcome:?}");
        assert_eq!(outcome.conflicts.len(), 1, "{outcome:?}");
        assert_eq!(outcome.conflicts[0].0, PathBuf::from("src/a.rs"));
        assert!(
            outcome.conflicts[0]
                .1
                .contains("changed since the base snapshot"),
            "{}",
            outcome.conflicts[0].1
        );
        assert_eq!(
            parent_bytes(&fixture),
            b"v3-parent-moved",
            "parent bytes intact"
        );

        // Parity: the fallback-only registry produces the SAME conflict
        // shape for the identical tree (details embed the temp root, so
        // compare paths + the typed reason + merged set).
        let baseline = build_fixture(faktor_agent::fallback_semantic_registry());
        std::fs::write(baseline.owner_root.join("src/a.rs"), b"v3-parent-moved").unwrap();
        let parity = baseline
            .orch
            .approve_and_merge(
                "child-0",
                &baseline.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(parity.conflicts.len(), outcome.conflicts.len());
        assert_eq!(parity.conflicts[0].0, outcome.conflicts[0].0);
        assert!(parity.conflicts[0]
            .1
            .contains("changed since the base snapshot"));
        assert_eq!(parity.merged, outcome.merged);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_absence_keeps_the_merge_byte_identical() {
        // No registered provider: the merge applies exactly as before.
        let fixture = build_fixture(faktor_agent::fallback_semantic_registry());
        let outcome = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(outcome.merged, vec![PathBuf::from("src/a.rs")]);
        assert!(outcome.conflicts.is_empty());
        assert_eq!(parent_bytes(&fixture), b"v2-child");
    }

    #[test]
    fn corrupt_child_identity_ids_are_typed_errors_never_panics() {
        let fixture = build_fixture(faktor_agent::fallback_semantic_registry());
        let (parent, run, row) = fixture.orch.locate_child("child-0").unwrap();
        assert_eq!(parent, fixture.parent);
        assert_eq!(run, "run-1");
        // Valid row: the worktree directory resolves.
        assert!(fixture.orch.child_worktree_dir(&row).unwrap().is_dir());

        // Zero workspace/worktree ids: typed Malformed naming the field,
        // never a panic (`WorkspaceId::new(0)`).
        for (field, corrupt) in [
            (
                "workspace_id",
                (|r: &mut ChildRuntime| r.workspace_id = 0) as fn(&mut ChildRuntime),
            ),
            ("worktree_id", |r: &mut ChildRuntime| r.worktree_id = 0),
        ] {
            let mut hostile = row.clone();
            corrupt(&mut hostile);
            let err = fixture
                .orch
                .child_worktree_dir(&hostile)
                .expect_err(&format!("zero {field} must refuse the worktree lookup"));
            assert!(matches!(err, ExecError::Malformed(_)), "{err:?}");
            assert!(err.to_string().contains(field), "{err}");
        }

        // A hostile registry row with session_id 0: the child result
        // must refuse typed instead of panicking in `SessionId::new(0)`.
        {
            let mut hostile = row.clone();
            hostile.session_id = 0;
            let handle = fixture
                .orch
                .manager
                .get_session(fixture.parent)
                .unwrap()
                .unwrap();
            handle
                .upsert_memory_fact(
                    REGISTRY_ROW_KIND,
                    "run-1/child-0",
                    &serde_json::to_string(&hostile).unwrap(),
                )
                .unwrap();
        }
        let err = fixture
            .orch
            .child_result("child-0")
            .expect_err("a zero session id must refuse the child result");
        assert!(matches!(err, ExecError::Malformed(_)), "{err:?}");
        assert!(err.to_string().contains("session_id 0"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_owner_workspace_id_refuses_the_semantic_preflight_typed() {
        let fixture = build_fixture(registry_with(ScriptedDeltaProvider {
            new_hash: Some(staged_child_hash()),
        }));
        // Overwrite the durable plan row with a zero owner workspace id:
        // the provider selection is delta-capable, so the preflight
        // reaches the owner decode and must refuse typed.
        let plan = crate::TaskPlan {
            goal: "merge test".into(),
            non_goals: vec![],
            constraints: vec![],
            work_items: vec![],
        };
        let owner = OwnerContext {
            parent_session: fixture.parent,
            workspace_id: 0,
            worktree_id: 1,
            root: fixture.owner_root.clone(),
        };
        let config = ExecConfig {
            run_id: "run-1".into(),
            ceilings: Ceilings::default(),
            parent_caps: CapabilitySet::new(),
            provider: "fake".into(),
            default_model: "m".into(),
            isolated_root: fixture.owner_root.join("iso"),
            crash_seam: None,
        };
        fixture
            .orch
            .put_plan_row(&plan, &owner, &config, &[])
            .unwrap();
        let err = fixture
            .orch
            .semantic_merge_preflight(
                fixture.parent,
                "run-1",
                &fixture.cs,
                &[PathBuf::from("src/a.rs")],
            )
            .await
            .expect_err("a zero owner workspace id must refuse the preflight");
        assert!(matches!(err, ExecError::Malformed(_)), "{err:?}");
        assert!(err.to_string().contains("workspace id 0"), "{err}");
    }
    /// A counting delta provider: handshake/delta counters + a failure mode.
    struct CountingDeltaProvider {
        handshakes: std::sync::atomic::AtomicUsize,
        deltas: std::sync::atomic::AtomicUsize,
        mode: CountingDeltaMode,
        fidelity: faktor_semantic::SemanticFidelity,
        completeness: faktor_semantic::SemanticCompleteness,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CountingDeltaMode {
        Empty,
        Fail,
        Pending,
    }

    impl faktor_semantic::SemanticProvider for CountingDeltaProvider {
        fn id(&self) -> faktor_semantic::SemanticProviderId {
            faktor_semantic::SemanticProviderId::parse("counting-delta").unwrap()
        }

        fn version(&self) -> u32 {
            3
        }

        fn capabilities(&self) -> faktor_semantic::SemanticCapabilities {
            faktor_semantic::SemanticCapabilities::DELTA.with_compose_delta(true)
        }

        fn fidelity(&self) -> faktor_semantic::SemanticFidelity {
            self.fidelity
        }

        fn completeness(&self) -> faktor_semantic::SemanticCompleteness {
            self.completeness
        }

        fn handshake(
            &self,
            _cancel: faktor_core::cancellation::CancellationToken,
        ) -> faktor_semantic::BoxFuture<
            '_,
            Result<faktor_semantic::SemanticProviderDescriptor, faktor_semantic::SemanticError>,
        > {
            self.handshakes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let descriptor = self.descriptor();
            Box::pin(async move { descriptor.validate().map(|()| descriptor) })
        }

        fn delta(
            &self,
            request: faktor_semantic::SemanticDeltaRequest,
        ) -> faktor_semantic::BoxFuture<
            '_,
            Result<
                faktor_semantic::SemanticEnvelope<faktor_semantic::SemanticDelta>,
                faktor_semantic::SemanticError,
            >,
        > {
            self.deltas
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mode = self.mode;
            let id = self.id();
            let version = self.version();
            Box::pin(async move {
                match mode {
                    CountingDeltaMode::Empty => Ok(faktor_semantic::SemanticEnvelope::new(
                        id,
                        version,
                        request.workspace,
                        request.from_snapshot,
                        1,
                        faktor_semantic::SemanticDelta {
                            workspace: request.workspace,
                            from_snapshot: request.from_snapshot,
                            to_snapshot: request.from_snapshot,
                            changes: Vec::new(),
                            degraded: false,
                        },
                    )),
                    CountingDeltaMode::Fail => Err(faktor_semantic::SemanticError::Refused(
                        "synthetic provider failure".into(),
                    )),
                    CountingDeltaMode::Pending => {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        Err(faktor_semantic::SemanticError::Refused(
                            "pending provider woke".into(),
                        ))
                    }
                }
            })
        }
    }

    fn counting_registry(
        provider: &Arc<CountingDeltaProvider>,
    ) -> Arc<faktor_semantic::SemanticProviderRegistry> {
        let mut registry = faktor_semantic::SemanticProviderRegistry::new(
            faktor_semantic::GenericSemanticFallback::default(),
        );
        registry.register(provider.clone());
        Arc::new(registry)
    }

    fn set_semantic_required(fixture: &Fixture, required: bool) {
        let (_, _, child) = fixture.orch.locate_child("child-0").unwrap();
        let session = SessionId::try_from(child.session_id).unwrap();
        let handle = fixture.orch.manager.get_session(session).unwrap().unwrap();
        let mut identity = handle.orchestrator_child_identity_get().unwrap().unwrap();
        identity.require_semantic_delta = required;
        handle.orchestrator_child_identity_put(&identity).unwrap();
    }

    /// The first-ever merge call AWAITS the descriptor handshake and really
    /// runs the delta (the old synchronous `select` could skip a provider
    /// that had not handshaken in some unrelated earlier operation).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_ever_merge_preflight_handshakes_and_runs() {
        let provider = Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Empty,
            fidelity: faktor_semantic::SemanticFidelity::CompilerExact,
            completeness: faktor_semantic::SemanticCompleteness::ConservativeComplete,
        });
        let fixture = build_fixture(counting_registry(&provider));
        let outcome = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect("an empty delta merges");
        assert_eq!(outcome.merged, vec![PathBuf::from("src/a.rs")]);
        assert!(
            provider
                .handshakes
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 1,
            "the first merge call must handshake the provider"
        );
        assert_eq!(
            provider.deltas.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the delta must actually run"
        );
    }

    /// A child whose task policy REQUIRES compiler-exact verification blocks
    /// the merge when no delta-capable provider is registered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_semantic_delta_blocks_without_a_provider() {
        let mut registry = faktor_semantic::SemanticProviderRegistry::new(
            faktor_semantic::GenericSemanticFallback::default(),
        );
        registry.register(Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Fail,
            fidelity: faktor_semantic::SemanticFidelity::CompilerExact,
            completeness: faktor_semantic::SemanticCompleteness::ConservativeComplete,
        }));
        let fixture = build_fixture(Arc::new(registry));
        set_semantic_required(&fixture, true);
        // No provider that can actually answer: remove the failing one by
        // building a fallback-only fixture for the absence case.
        let fallback_only = Arc::new(faktor_semantic::SemanticProviderRegistry::new(
            faktor_semantic::GenericSemanticFallback::default(),
        ));
        let fixture = build_fixture(fallback_only);
        set_semantic_required(&fixture, true);
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("required semantic verification must block");
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert_eq!(parent_bytes(&fixture), b"v1-base", "nothing applied");
    }

    /// A REQUIRED preflight blocks on provider failure — it never silently
    /// downgrades to the advisory path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_semantic_delta_blocks_on_provider_failure() {
        let provider = Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Fail,
            fidelity: faktor_semantic::SemanticFidelity::CompilerExact,
            completeness: faktor_semantic::SemanticCompleteness::ConservativeComplete,
        });
        let fixture = build_fixture(counting_registry(&provider));
        set_semantic_required(&fixture, true);
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("a required preflight must block on provider failure");
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert_eq!(provider.deltas.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A required preflight rejects a provider that declares sub-compiler
    /// fidelity (audit Tangerine-3/4) BEFORE running any delta.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_semantic_delta_rejects_sub_compiler_fidelity() {
        let provider = Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Empty,
            fidelity: faktor_semantic::SemanticFidelity::Structural,
            completeness: faktor_semantic::SemanticCompleteness::ConservativeComplete,
        });
        let fixture = build_fixture(counting_registry(&provider));
        set_semantic_required(&fixture, true);
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("sub-compiler fidelity cannot satisfy a required preflight");
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert_eq!(
            provider.deltas.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no delta may run under sub-compiler fidelity"
        );
        assert_eq!(parent_bytes(&fixture), b"v1-base", "nothing applied");
    }

    /// P1-SEMANTIC: a REQUIRED merge refuses a provider that declares NO
    /// fidelity/completeness at all — absence is UNTRUSTED, never the
    /// strongest level.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_semantic_delta_blocks_an_undeclared_provider() {
        let fixture = build_fixture(registry_with(ScriptedDeltaProvider {
            new_hash: Some(staged_child_hash()),
        }));
        set_semantic_required(&fixture, true);
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("an undeclared provider cannot satisfy a required preflight");
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert!(
            err.to_string().contains("fidelity"),
            "the refusal names the undeclared fidelity: {err}"
        );
        assert_eq!(parent_bytes(&fixture), b"v1-base", "nothing applied");
    }

    /// A required preflight rejects a provider whose DECLARED completeness
    /// is below conservative-complete (audit P1-SEMANTIC) BEFORE running
    /// any delta.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_semantic_delta_rejects_sub_conservative_completeness() {
        let provider = Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Empty,
            fidelity: faktor_semantic::SemanticFidelity::CompilerExact,
            completeness: faktor_semantic::SemanticCompleteness::Partial,
        });
        let fixture = build_fixture(counting_registry(&provider));
        set_semantic_required(&fixture, true);
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("partial completeness cannot satisfy a required preflight");
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert_eq!(
            provider.deltas.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no delta may run under partial completeness"
        );
        assert_eq!(parent_bytes(&fixture), b"v1-base", "nothing applied");
    }

    /// P0-POLICY: an UNREADABLE durable child-policy row (corrupt identity
    /// JSON or store failure) blocks a fresh merge typed — it is never
    /// silently downgraded to advisory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreadable_child_policy_blocks_a_fresh_merge() {
        let provider = Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Empty,
            fidelity: faktor_semantic::SemanticFidelity::CompilerExact,
            completeness: faktor_semantic::SemanticCompleteness::ConservativeComplete,
        });
        let fixture = build_fixture(counting_registry(&provider));
        {
            let (_, _, child) = fixture.orch.locate_child("child-0").unwrap();
            let session = SessionId::try_from(child.session_id).unwrap();
            let handle = fixture.orch.manager.get_session(session).unwrap().unwrap();
            handle
                .upsert_memory_fact("orchestrator", "identity", "{{ not json")
                .unwrap();
        }
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("an unreadable policy must block the merge");
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert!(
            err.to_string().contains("identity read failed"),
            "the refusal must name the unreadable policy: {err}"
        );
        assert_eq!(
            provider.deltas.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no delta may run under an unreadable policy"
        );
        assert_eq!(parent_bytes(&fixture), b"v1-base", "nothing applied");
    }

    /// A PENDING provider is awaited (bounded); it is never treated as
    /// absence. Required mode blocks at the bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_semantic_provider_is_awaited_not_skipped() {
        let provider = Arc::new(CountingDeltaProvider {
            handshakes: std::sync::atomic::AtomicUsize::new(0),
            deltas: std::sync::atomic::AtomicUsize::new(0),
            mode: CountingDeltaMode::Pending,
            fidelity: faktor_semantic::SemanticFidelity::CompilerExact,
            completeness: faktor_semantic::SemanticCompleteness::ConservativeComplete,
        });
        let fixture = build_fixture(counting_registry(&provider));
        set_semantic_required(&fixture, true);
        SEMANTIC_MERGE_TIMEOUT_OVERRIDE_MS.store(150, std::sync::atomic::Ordering::SeqCst);
        let err = fixture
            .orch
            .approve_and_merge(
                "child-0",
                &fixture.cs.id(),
                &[PathBuf::from("src/a.rs")],
                &[],
            )
            .await
            .expect_err("a pending required provider must block at the bound");
        SEMANTIC_MERGE_TIMEOUT_OVERRIDE_MS.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(err, ExecError::SemanticRequired(_)), "{err:?}");
        assert_eq!(
            provider.deltas.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the provider was actually called and awaited"
        );
    }
}
