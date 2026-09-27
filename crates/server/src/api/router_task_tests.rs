use crate::api::tests::*;
use crate::api::*;

#[tokio::test]
async fn native_tasks_verification_agents_and_terminal_reflect_durable_rows() {
    // The listings are row-backed: an injected durable ledger, an
    // injected verification fact, no turn records, no PTYs. Reading
    // them back must be exact; hostile ids are loud.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/tmp").unwrap();
    let session = manager.create_session(ws, "t-rows", "fake", "m").unwrap();
    let sid = session.id().to_string();
    let h = manager.get_session(session.id()).unwrap().unwrap();
    h.put_task_ledger(serde_json::json!({
        "goal": "implement the native surface",
        "constraints": ["rust"],
        "completed_steps": ["mount routes"],
        "open_steps": ["wire abort", "aggregate usage"],
        "decisions": ["strict DTOs"],
        "known_failures": [],
        "changed_files": ["crates/server/src/api.rs"],
        "tests_run": ["cargo check"],
        "tests_failed": [],
        "user_preferences": [],
    }))
    .unwrap();
    h.upsert_memory_fact("verification", "fmt", "failed:make fmt")
        .unwrap();

    // tasks: exactly one entry, typed from the ledger + the fact.
    let resp = client
        .get(format!("{base}/native/session/{sid}/tasks"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let tasks = body.as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{body}");
    assert_eq!(tasks[0]["goal"], "implement the native surface");
    assert_eq!(tasks[0]["state"], "in_progress");
    assert_eq!(
        tasks[0]["milestones"]["open"],
        serde_json::json!(["wire abort", "aggregate usage"])
    );
    assert_eq!(
        tasks[0]["milestones"]["completed"],
        serde_json::json!(["mount routes"])
    );
    assert_eq!(
        tasks[0]["changedFiles"],
        serde_json::json!(["crates/server/src/api.rs"])
    );
    assert_eq!(
        tasks[0]["verification"],
        serde_json::json!([{"id": "fmt", "detail": "failed:make fmt", "status": "failed"}])
    );

    // verification: the durable fact is owed-failed; no pending runs.
    let resp = client
        .get(format!("{base}/native/session/{sid}/verification"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["owed"], serde_json::json!([]));
    assert_eq!(body["failedChecks"][0]["id"], "fmt");
    assert_eq!(body["failedChecks"][0]["detail"], "failed:make fmt");

    // agents: no background agents yet (orchestration not landed).
    let resp = client
        .get(format!("{base}/native/session/{sid}/agents"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );

    // terminal: no PTYs exist on this daemon → the empty view.
    let resp = client
        .get(format!("{base}/native/session/{sid}/terminal"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );

    // turns: no turn ever ran → empty. checkpoints: no service wired
    // in test_deps → empty.
    let resp = client
        .get(format!("{base}/native/session/{sid}/turns"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );
    let resp = client
        .get(format!("{base}/native/session/{sid}/checkpoints"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );

    // The same hostile/unknown treatment applies to every listing.
    for path in [
        "/native/session/0/tasks".to_string(),
        "/native/session/abc/verification".to_string(),
        "/native/session/0/agents".to_string(),
        "/native/session/abc/terminal".to_string(),
    ] {
        let resp = client
            .get(format!("{base}{path}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{path}");
    }
    for path in [
        "/native/session/999999/tasks".to_string(),
        "/native/session/999999/checkpoints".to_string(),
        "/native/session/999999/verification".to_string(),
        "/native/session/999999/agents".to_string(),
        "/native/session/999999/terminal".to_string(),
    ] {
        let resp = client
            .get(format!("{base}{path}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_presentation_continuity_is_durable_scoped_and_typed() {
    // Foreground -> background -> foreground continuity: the durable
    // presentation fold of the child session, surfaced in the agent
    // listing + typed graph, never a scheduling or lineage change.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    // child-0 = Done row, child-1 = Running row.
    let (parent, child_a, child_b) = seed_orchestration_graph(&manager);
    let presentation_path = |session: &str, child: &str| {
        format!("/native/session/{session}/agents/{child}/presentation")
    };

    // Unauthenticated is 401 before anything else.
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-1")
        ))
        .json(&serde_json::json!({"state": "background"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Strict DTO: every hostile body is a plain 400, never a 422 and
    // never a silent default.
    for body in [
        serde_json::json!({}),
        serde_json::json!({"state": "paused"}),
        serde_json::json!({"state": "BACKGROUND"}),
        serde_json::json!({"state": 1}),
        serde_json::json!({"state": null}),
        serde_json::json!({"state": "background", "extra": true}),
        serde_json::json!("background"),
    ] {
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-1")
            ))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "hostile body must 400: {body}");
    }
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-1")
        ))
        .bearer_auth(token.as_str())
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Hostile/unknown/foreign ids are typed 404s scoped to the path
    // session (a child id of another session is never resolved).
    let other = manager
        .create_session(
            manager.create_workspace("/other").unwrap(),
            "other",
            "fake",
            "m",
        )
        .unwrap()
        .id();
    for (session, child) in [
        (parent.to_string(), "child-9".to_string()),
        (parent.to_string(), "..".to_string()),
        ("999999".to_string(), "child-1".to_string()),
        (other.to_string(), "child-1".to_string()),
    ] {
        let resp = client
            .post(format!("{base}{}", presentation_path(&session, &child)))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "background"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "session {session} child {child}");
    }
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path("not-a-number", "child-1")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "background"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // A running child flips to background durably; the same-state set is
    // an idempotent no-op that writes nothing.
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-1")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "background"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["child_id"], "child-1");
    assert_eq!(ack["presentation"], "background");
    assert_eq!(ack["changed"], true);
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-1")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "background"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["changed"], false);
    // Durable truth: the child session's ledger fold holds Background.
    let ca = manager.get_session(child_a).unwrap().unwrap();
    assert_eq!(
        ca.child_presentation("child-1").unwrap(),
        faktor_session::child::PresentationState::Background
    );

    // The native agent listing and the typed graph both carry the field;
    // an untouched child stays foreground.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/agents?session={parent}"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let listing: serde_json::Value = resp.json().await.unwrap();
    let c1 = listing
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["agent_id"] == "child-1")
        .expect("child-1 listed");
    assert_eq!(c1["presentation"], "background");
    let c0 = listing
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["agent_id"] == "child-0")
        .expect("child-0 listed");
    assert_eq!(c0["presentation"], "foreground");
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/orchestrator/graph?session={parent}"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let graph: serde_json::Value = resp.json().await.unwrap();
    let g1 = graph["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["child_id"] == "child-1")
        .expect("child-1 graphed");
    assert_eq!(g1["presentation"], "background");

    // Background -> foreground: the SAME ChildId/session continues and
    // the state folds back.
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-1")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "foreground"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["presentation"], "foreground");
    assert_eq!(ack["changed"], true);
    assert_eq!(c1["session_id"], g1["session_id"]);

    // Terminal children are Background-only: child-0's durable row is
    // Done. The currently-foreground no-op stays legal, the background
    // flip is accepted, and the foreground revival is a typed 409 that
    // writes nothing.
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-0")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "foreground"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["changed"],
        false
    );
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-0")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "background"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .post(format!(
            "{base}{}",
            presentation_path(&parent.to_string(), "child-0")
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"state": "foreground"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        409,
        "terminal child refuses foreground revival"
    );
    let cb = manager.get_session(child_b).unwrap().unwrap();
    assert_eq!(
        cb.child_presentation("child-0").unwrap(),
        faktor_session::child::PresentationState::Background,
        "the refused revival wrote nothing"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_orchestrator_graph_projects_the_durable_run() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (parent, _ca, cb) = seed_orchestration_graph(&manager);
    // Unauthenticated is 401 like every /native handler.
    let resp = client
        .get(format!(
            "{base}/native/orchestrator/graph?session={}",
            parent
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // Authenticated: the full graph JSON.
    let resp = client
        .get(format!(
            "{base}/native/orchestrator/graph?session={}",
            parent
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let g: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(g["plan_id"], "run-1");
    assert_eq!(g["goal"], "Ship the graph");
    // b is Done (durable child row), a is still Pending behind... no:
    // a has a durable Running child, so the step is Running; root is
    // Running (not all Done).
    let steps: Vec<&str> = g["work_items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["item_id"].as_str().unwrap())
        .collect();
    assert_eq!(steps, vec!["b", "a"]);
    assert_eq!(g["work_items"][0]["state"], "Done");
    assert_eq!(g["work_items"][1]["state"], "Running");
    assert_eq!(g["state"], "Running");
    // Children ordered by plan step (b child first even though its
    // created_ms is smaller anyway; verify the linkage values).
    let children = g["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["child_id"], "child-0");
    assert_eq!(children[0]["plan_step_index"], 0);
    assert_eq!(children[0]["session_id"], cb.raw());
    assert_eq!(children[0]["worktree_id"], cb.raw());
    assert_eq!(children[0]["ownership"], "read_only_shared");
    assert_eq!(children[0]["state"], "Done");
    assert_eq!(children[0]["budget"], 1000);
    assert!(children[0]["capabilities"].is_array());
    // The merge record of the Done child.
    let m = &children[0]["merge"];
    assert_eq!(m["change_set_id"], "base-child-0-cs");
    assert_eq!(m["merged"], serde_json::json!(["keep.rs"]));
    assert_eq!(m["rejected"], serde_json::json!([]));
    assert_eq!(m["conflicts"][0][0], "src/a.rs");
    assert_eq!(children[1]["child_id"], "child-1");
    assert_eq!(children[1]["plan_step_index"], 1);
    assert_eq!(children[1]["state"], "Running");
    assert!(children[1]["merge"].is_null());
    // Steering history of the a-child: applied pause first, pending
    // steer second, seq order preserved.
    let events = children[1]["steer_events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"]["kind"], "pause");
    assert!(!events[0]["applied_ms"].is_null());
    assert_eq!(events[1]["kind"]["kind"], "steer");
    assert_eq!(events[1]["kind"]["note"], "focus the api");
    assert!(events[1]["applied_ms"].is_null());
    assert!(events[0]["seq"].as_u64().unwrap() < events[1]["seq"].as_u64().unwrap());
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_orchestrator_graph_hostile_ids_404_and_corrupt_rows_are_loud() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (parent, _ca, _cb) = seed_orchestration_graph(&manager);
    // Hostile session ids: non-numeric, zero, negative, absurd, and a
    // session that exists but holds no orchestration run — all 404
    // (never a phantom graph, never a 200).
    for hostile in [
        "abc".to_string(),
        "0".to_string(),
        "-1".to_string(),
        "999999999".to_string(),
        "1;drop".to_string(),
        "%2e%2e".to_string(),
    ] {
        let resp = client
            .get(format!(
                "{base}/native/orchestrator/graph?session={hostile}"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "hostile session {hostile:?} must 404");
    }
    // A real session with no orchestration rows: 404.
    let ws = manager.create_workspace("/plain").unwrap();
    let plain = manager.create_session(ws, "plain", "fake", "m").unwrap();
    let resp = client
        .get(format!(
            "{base}/native/orchestrator/graph?session={}",
            plain.id()
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // A second run under the PARENT: the graph is ambiguous — loud 409
    // naming both runs.
    let plan_row = serde_json::json!({
        "plan": {"goal": "second", "non_goals": [], "constraints": [],
                 "work_items": []},
        "created_ms": 1,
    });
    manager
        .get_session(parent)
        .unwrap()
        .unwrap()
        .upsert_memory_fact(ORCH_PLAN_KIND, "run-2", &plan_row.to_string())
        .unwrap();
    let resp = client
        .get(format!(
            "{base}/native/orchestrator/graph?session={}",
            parent
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("run-1"));
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("run-2"));
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_orchestrator_graph_tampered_registry_row_is_a_loud_500() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (parent, _ca, _cb) = seed_orchestration_graph(&manager);
    // Corrupt one registry row: the projection refuses loudly (500)
    // instead of serving a silently partial graph.
    let parent_handle = manager.get_session(parent).unwrap().unwrap();
    parent_handle
        .upsert_memory_fact(ORCH_REGISTRY_KIND, "run-1/child-1", "{corrupt")
        .unwrap();
    let resp = client
        .get(format!(
            "{base}/native/orchestrator/graph?session={}",
            parent
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("run-1/child-1"),
        "{body}"
    );
    let _ = handle.request_shutdown();
}

// ------------------------------------------------- native agents + control

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_agents_lists_and_controls_real_children_mid_flight() {
    let dir = tempfile::tempdir().unwrap();
    // Real children over the server's runtime: paced two-iteration
    // roundtrips keep the drives mid-flight long enough for HTTP
    // controls to land deterministically.
    let paced = PacedScriptedProvider::new(
        ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        vec![
            vec![
                faktor_provider::ScriptedResponse::Text("analyzing".into()),
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text": "hello"}),
                },
                faktor_provider::ScriptedResponse::End,
            ],
            vec![
                faktor_provider::ScriptedResponse::Text("done".into()),
                faktor_provider::ScriptedResponse::End,
            ],
            vec![faktor_provider::ScriptedResponse::End],
        ],
        4000,
    );
    let deps = paced_test_deps(dir.path(), paced);
    let orch = deps.orchestrator.clone();
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let (parent, owner, isolated) = orch_owner_env(&manager, dir.path());

    let config = faktor_orchestrator::runtime::ExecConfig {
        run_id: "run-http".into(),
        ceilings: faktor_orchestrator::runtime::Ceilings::default(),
        parent_caps: read_workspace_caps(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: isolated.clone(),
        crash_seam: None,
    };
    let plan = analysis_plan(&["a", "b"]);
    let specs = vec![read_child_spec("a"), read_child_spec("b")];
    let run = tokio::spawn(async move {
        orch.execute_task(plan, owner, config, &specs)
            .await
            .unwrap()
    });

    // The agent listing shows the parent's own run + both children.
    let client = reqwest::Client::new();
    let mut entries = Vec::new();
    for _ in 0..200 {
        let resp = client
            .get(format!("{base}/native/agents?session={parent}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = resp.json().await.unwrap();
        let kids: Vec<&serde_json::Value> = v
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "child")
            .collect();
        if kids.len() >= 2 {
            entries = v.as_array().unwrap().clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(entries.len(), 3, "self + two children: {entries:?}");
    assert_eq!(entries[0]["kind"], "self");
    assert_eq!(entries[0]["run_id"], "run-http");
    assert_eq!(entries[0]["ownership"], "self");
    assert_eq!(entries[0]["state"], "Running");
    assert_eq!(entries[0]["goal"], "Ship the analysis");
    assert!(entries[1]["item_id"] == "a" || entries[2]["item_id"] == "a");
    // Children carry real session ids, worktree identity, live model
    // and progress while their drives are in flight. The provider is the
    // child session's OWN durable row (the catalog join key), so it must
    // match the run's provider even when another provider serves the same
    // model id.
    for e in entries.iter().filter(|e| e["kind"] == "child") {
        assert_eq!(e["run_id"], "run-http");
        assert_ne!(e["session_id"].as_u64().unwrap_or(0), 0);
        assert_eq!(e["ownership"], "read_only_shared");
        assert_eq!(e["state"], "Running");
        assert_eq!(e["model"], "m");
        assert_eq!(e["provider"], "fake");
    }

    // Mid-flight budget change: applied synchronously and durably
    // visible on the child row through the listing.
    let resp = client
        .post(format!("{base}/native/agents/child-0/budget"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"max_tokens": 4321}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["queuedSeq"], 1);
    assert_eq!(ack["applied"], true);
    // Steer + model enqueue durably with the exact wire shape and are
    // applied at the drive's next safe reasoning boundary (the pause
    // boundary machine itself is the wave-12 harness's contract; this
    // endpoint test freezes the queue + ack wire and the durable
    // application visible on the child's drive-state row).
    for (path, seq, body) in [
        (
            "steer",
            2,
            Some(serde_json::json!({"text": "focus the api surface"})),
        ),
        ("model", 3, Some(serde_json::json!({"model": "default"}))),
    ] {
        let mut rb = client
            .post(format!("{base}/native/agents/child-0/{path}"))
            .bearer_auth(token.as_str());
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], seq, "{path}");
        assert!(ack["applied"].is_null(), "{path}");
    }

    // The run completes naturally; both children are Done. The queue
    // wire for steer/model is frozen above; their exactly-once
    // application at a reasoning boundary is the wave-12 runtime
    // harness's contract (pause/steer/cancel/budget drives), exercised
    // in this crate's own suite.
    let outcome = tokio::time::timeout(Duration::from_secs(180), run)
        .await
        .expect("run must settle")
        .expect("executor drive panicked");
    assert!(outcome.complete, "{outcome:?}");

    // The terminal listing reflects the durable rows: the budget patch
    // sits on the child row and both children are Done.
    let resp = client
        .get(format!("{base}/native/agents?session={parent}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    let v: serde_json::Value = resp.json().await.unwrap();
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 3);
    let c0 = entries
        .iter()
        .find(|e| e["agent_id"] == "child-0")
        .expect("done child listed");
    assert_eq!(c0["state"], "Done");
    assert_eq!(c0["budget"], 4321, "budget change visible on the child row");
    let c1 = entries
        .iter()
        .find(|e| e["agent_id"] == "child-1")
        .expect("done child listed");
    assert_eq!(c1["state"], "Done");
    let root = entries
        .iter()
        .find(|e| e["kind"] == "self")
        .expect("self entry");
    assert_eq!(root["state"], "Done");
    // Pause after the run is a typed terminal refusal once the mirror
    // settled (409, never a silent no-op).
    let resp = client
        .post(format!("{base}/native/agents/child-0/pause"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "terminal children refuse pause");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_agents_lists_insession_task_runs_and_empty_only_without_runs() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let tasks = deps.tasks.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let sid = manager
        .create_session(ws, "plain", "fake", "m")
        .unwrap()
        .id();

    // Genuinely no task run -> the empty array (never a phantom).
    let v = get_agents(
        &base,
        token.as_str(),
        &format!("/native/session/{sid}/agents"),
    )
    .await;
    assert_eq!(v, serde_json::json!([]));

    // A TaskExecutor single-item task (the one-work-item case of the
    // SAME executor that spawns orchestrated children) drives this
    // session through the daemon's own prompt path and shows up as the
    // parent's own task run.
    let req = faktor_orchestrator::runtime::task_executor::TaskRunRequest {
        goal: "analyze the module boundaries".into(),
        work_items: vec![faktor_orchestrator::WorkItem::new(
            "a1",
            "analyze the module boundaries",
            faktor_orchestrator::WorkKind::Analysis,
        )],
        ..Default::default()
    };
    let receipt = tasks.start_task(sid, req).expect("single-item start");
    assert_eq!(
        receipt.mode,
        faktor_orchestrator::runtime::task_executor::TaskRunMode::InSession
    );
    assert!(receipt.run_id.starts_with("tx-"));

    // The listing polls to the run's terminal state and shows the
    // session's own run with the session's worktree identity.
    let mut seen = None;
    for _ in 0..200 {
        let v = get_agents(
            &base,
            token.as_str(),
            &format!("/native/agents?session={sid}"),
        )
        .await;
        let entries = v.as_array().unwrap();
        if entries.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        seen = Some(entries.clone());
        if entries[0]["state"] == "Done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entries = seen.expect("the in-session run must appear");
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e["kind"], "self");
    assert_eq!(e["run_id"], receipt.run_id);
    assert_eq!(e["session_id"], sid.raw());
    assert_eq!(e["state"], "Done");
    assert_eq!(e["goal"], "analyze the module boundaries");
    assert_eq!(e["item_ids"], serde_json::json!(["a1"]));
    assert_eq!(e["ownership"], "self");
    assert!(e["budget"].is_null());
    let session_row = manager.get_session(sid).unwrap().unwrap().row().unwrap();
    assert_eq!(e["worktree_id"], session_row.worktree_id.raw());
    // The path-id form lists the same truth (progress ticks between
    // polls, so the live fields are compared individually).
    let v2 = get_agents(
        &base,
        token.as_str(),
        &format!("/native/session/{sid}/agents"),
    )
    .await;
    let e2 = &v2.as_array().unwrap()[0];
    for key in [
        "agent_id",
        "kind",
        "run_id",
        "session_id",
        "worktree_id",
        "goal",
        "state",
        "model",
        "budget",
        "ownership",
    ] {
        assert_eq!(e2.get(key), e.get(key), "{key}");
    }
    assert_eq!(e2["item_ids"], e["item_ids"]);
    // Hostile session ids are typed 404 on the query endpoint.
    for hostile in ["abc", "0", "-1", "999999999", "1;drop"] {
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/native/agents?session={hostile}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "hostile {hostile:?}");
    }
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_agent_control_guards_and_hostile_inputs_are_typed() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let orch = deps.orchestrator.clone();
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let (parent, owner, isolated) = orch_owner_env(&manager, dir.path());

    // A run whose executor crashed right after the child was created
    // (BeforeDrive seam): the durable child row is live (Running) and
    // the mirror is parked — a deterministic control window without a
    // racing drive.
    let config = faktor_orchestrator::runtime::ExecConfig {
        run_id: "run-seam".into(),
        ceilings: faktor_orchestrator::runtime::Ceilings::default(),
        parent_caps: read_workspace_caps(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: isolated.clone(),
        crash_seam: Some(faktor_orchestrator::runtime::CrashSeam::BeforeDrive),
    };
    let plan = analysis_plan(&["a"]);
    let specs = vec![read_child_spec("a")];
    let res = orch
        .execute_task(plan, owner, config, &specs)
        .await
        .expect_err("the seam must fire");
    assert!(
        matches!(
            res,
            faktor_orchestrator::runtime::ExecError::InjectedCrashSeam(_)
        ),
        "{res:?}"
    );

    // Hostile child ids and bodies: typed 404/400, never a panic.
    let client = reqwest::Client::new();
    let post = |path: &str, body: Option<serde_json::Value>| {
        let mut rb = client
            .post(format!("{base}{path}"))
            .bearer_auth(token.as_str());
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        rb.send()
    };
    for path in [
        "/native/agents/child-9/pause",
        "/native/agents/nope/cancel",
        "/native/agents/child-0/../../pause",
    ] {
        let resp = post(path, None).await.unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    let resp = post("/native/agents/child-0/budget", Some(serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // A cost budget change is a synchronous durable effect (audit 9/H):
    // the child's task-row `max_cost_micro` is patched and its budget
    // scope is enrolled under the run root. No queue row exists for it
    // (queuedSeq null) and the token axis is untouched.
    let resp = post(
        "/native/agents/child-0/budget",
        Some(serde_json::json!({"max_cost_micro": 500})),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert!(ack["queuedSeq"].is_null(), "{ack}");
    assert_eq!(ack["applied"], true);
    let v = get_agents(
        &base,
        token.as_str(),
        &format!("/native/agents?session={parent}"),
    )
    .await;
    let child_session = v
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["agent_id"] == "child-0")
        .and_then(|e| e["session_id"].as_u64())
        .expect("child-0 listed with its session");
    let ledger = faktor_session::DurableBudgetLedger::new(manager.clone());
    let cost_view = ledger
        .session_budget_view(
            SessionId::new(child_session),
            faktor_core::id::TaskId::new(1),
        )
        .expect("durable budget view");
    assert_eq!(
        cost_view.max_cost_micro,
        Some(500),
        "the cost change landed on the child's durable task-row cap"
    );
    assert_eq!(
        ledger
            .scope_of(SessionId::new(child_session))
            .unwrap()
            .map(|s| s.child_id),
        Some("child-0".to_string()),
        "the child is enrolled under its run root"
    );
    // Zero on either axis is ambiguous (the store reads 0 as unlimited)
    // and refuses typed on both axes.
    let resp = post(
        "/native/agents/child-0/budget",
        Some(serde_json::json!({"max_cost_micro": 0})),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = post(
        "/native/agents/child-0/budget",
        Some(serde_json::json!({"max_tokens": 0})),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = post(
        "/native/agents/child-0/budget",
        Some(serde_json::json!({"max_tokens": 5, "max_cost_micro": 5})),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = post(
        "/native/agents/child-0/steer",
        Some(serde_json::json!({"text": "x".repeat(501)})),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 400, "steering notes are bounded");
    let resp = post(
        "/native/agents/child-0/model",
        Some(serde_json::json!({"model": "gpt-99"})),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 404, "model not in the provider registry");
    let resp = post("/native/agents/child-0/retry", None).await.unwrap();
    assert_eq!(resp.status(), 409, "only Failed children retry");
    let resp = post("/native/agents/child-0/resume", None).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Valid controls enqueue durably with the exactly-once ack shape.
    let resp = post("/native/agents/child-0/pause", None).await.unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["queuedSeq"], 2, "the earlier resume took seq 1");
    assert!(ack["applied"].is_null());
    let resp = post(
        "/native/agents/child-0/steer",
        Some(serde_json::json!({"text": "look at the seam"})),
    )
    .await
    .unwrap();
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["queuedSeq"], 3);
    assert!(ack["applied"].is_null());
    let resp = post(
        "/native/agents/child-0/model",
        Some(serde_json::json!({"model": "default"})),
    )
    .await
    .unwrap();
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["queuedSeq"], 4);
    assert!(ack["applied"].is_null());
    let resp = post(
        "/native/agents/child-0/budget",
        Some(serde_json::json!({"max_tokens": 99})),
    )
    .await
    .unwrap();
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["queuedSeq"], 5);
    assert_eq!(ack["applied"], true);
    let resp = post("/native/agents/child-0/cancel", None).await.unwrap();
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["queuedSeq"], 6);
    assert_eq!(ack["applied"], true);

    // The listing reflects the durable rows (budget patch on the child
    // row; the run's own entry derives from the children).
    let v = get_agents(
        &base,
        token.as_str(),
        &format!("/native/agents?session={}", parent),
    )
    .await;
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let child = entries
        .iter()
        .find(|e| e["agent_id"] == "child-0")
        .expect("child listed");
    assert_eq!(
        child["budget"], 99,
        "budget change visible on the child row"
    );
    assert_eq!(child["run_id"], "run-seam");
    let root = entries
        .iter()
        .find(|e| e["kind"] == "self")
        .expect("self entry");
    assert_eq!(root["run_id"], "run-seam");
    assert_eq!(root["state"], "Running");
    let _ = handle.request_shutdown();
}

// ------------------------------------------------- audits P0-62/63/64

#[tokio::test]
async fn native_verification_evidence_scoped_to_session_and_task() {
    // P0-64e: /native/session/{id}/tasks/{task_id}/verification returns
    // the durable VerificationRecord rows (checks/criteria/changed
    // files) of the session's OWN task only. Another session querying
    // the same numeric task id gets a typed 404 or an empty list when a
    // different workspace holds records under that id — evidence never
    // crosses sessions.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws_a = manager.create_workspace("/ver-a").unwrap();
    let ws_b = manager.create_workspace("/ver-b").unwrap();
    let a = manager
        .create_session(ws_a, "t-ver-a", "fake", "m")
        .unwrap();
    let b = manager
        .create_session(ws_b, "t-ver-b", "fake", "m")
        .unwrap();
    // C shares B's workspace and (for the cross-workspace guard) adopts
    // the SAME numeric task id A owns — yet C must never see A's
    // evidence rows, which certify A's workspace.
    let c = manager
        .create_session(ws_b, "t-ver-c", "fake", "m")
        .unwrap();
    manager
        .adopt_identity(
            a.id(),
            faktor_core::WorktreeId::new(3),
            faktor_core::id::TaskId::new(7),
        )
        .unwrap();
    manager
        .adopt_identity(
            b.id(),
            faktor_core::WorktreeId::new(4),
            faktor_core::id::TaskId::new(9),
        )
        .unwrap();
    manager
        .adopt_identity(
            c.id(),
            faktor_core::WorktreeId::new(5),
            faktor_core::id::TaskId::new(7),
        )
        .unwrap();
    let store = manager.store();
    let row_a = a.row().unwrap();
    let row_b = b.row().unwrap();
    let rev = faktor_core::id::TaskRevision::new(1);
    // A's record carries the full v20 candidate-proof evidence; B's is a
    // legacy NULL-evidence row (both must project honestly).
    let candidate = faktor_core::state::CandidateProofRef {
        task_revision: rev,
        base_manifest_hash: "11".repeat(32),
        candidate_manifest_hash: "22".repeat(32),
        source_diff_evidence: None,
        risk_report_evidence: None,
        accounting_snapshot_digest: "accounting:v1:feedfacefeedface".into(),
        run_id: Some("run-ver-a".into()),
        run_base_snapshot: Some("33".repeat(32)),
        candidate_snapshot: Some("44".repeat(32)),
        sources_digest: Some("55".repeat(32)),
        changed_files_digest: Some("66".repeat(32)),
    };
    let candidate_json = serde_json::to_string(&candidate).unwrap();
    let put =
        |row: &faktor_store::VerificationRecordRow| store.verification_record_put(row).unwrap();
    let put_with_evidence = |row: &faktor_store::VerificationRecordRow| {
        store
            .verification_record_put_with_evidence(row, None, Some(&candidate_json))
            .unwrap()
    };
    let rec_for = |session_row: &faktor_store::SessionRow, task_id: u64, check: &str| {
        faktor_store::VerificationRecordRow {
            id: faktor_core::id::VerificationRecordId::new(1),
            task_id: faktor_core::id::TaskId::new(task_id),
            revision: rev,
            workspace_id: session_row.workspace_id,
            worktree_id: session_row.worktree_id,
            tree_hash: Some("ab".repeat(32)),
            criteria: vec![faktor_core::state::CriterionVerification {
                criterion_key: "tests pass".into(),
                passed: true,
                evidence: Some("ran".into()),
                binding: None,
            }],
            checks: vec![faktor_core::state::CheckExecution {
                check: check.into(),
                program: "cargo".into(),
                args: vec!["test".into()],
                category: "required".into(),
                required: true,
                status: faktor_core::state::VerificationStatus::Passed,
                started_ms: 1,
                finished_ms: Some(2),
                exit: Some(0),
                summary: Some("ok".into()),
            }],
            changed_files: vec![faktor_core::state::FileStateEvidence {
                path: "crates/server/src/api.rs".into(),
                digest_hex: "cd".repeat(32),
                size: 42,
            }],
            unrelated_changes: vec!["README.md".into()],
            reviewer: None,
            status: faktor_core::state::VerificationStatus::Passed,
            started_ms: 1,
            completed_ms: Some(2),
        }
    };
    let ra = put_with_evidence(&rec_for(&row_a, 7, "cargo test -p faktor-session"));
    let rb = put(&rec_for(&row_b, 9, "cargo test -p faktor-server"));
    // A's integration record: 3 aggregate sources landing the candidate
    // snapshot (the proof payload's source count + landed snapshot).
    a.ledger_integration_record_set(&faktor_session::ledger::IntegrationRecordRow {
        run_id: "run-ver-a".into(),
        task_id: 7,
        base_revision: None,
        base_snapshot: Some("33".repeat(32)),
        run_base_snapshot: Some("33".repeat(32)),
        candidate_snapshot: Some("44".repeat(32)),
        landed_snapshot: Some("44".repeat(32)),
        proof_basis_digest: None,
        integration_txn_id: None,
        final_root: "/ver-a".into(),
        final_snapshot_hash: "44".repeat(32),
        integrated_files: vec!["src/lib.rs".into()],
        integrated_file_count: 1,
        integrated_files_digest: "77".repeat(32),
        conflicts: Vec::new(),
        conflict_count: 0,
        sources: Vec::new(),
        source_count: 3,
        sources_digest: "55".repeat(32),
        at_ms: 3,
    })
    .unwrap();

    // A's task-7 evidence: checks/criteria/changed files all present.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/tasks/7/verification", a.id()),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessionId"], a.id().to_string());
    assert_eq!(body["taskId"], "7");
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{body}");
    let rec = &records[0];
    assert_eq!(rec["recordId"], ra.to_string());
    assert_eq!(rec["revision"], "1");
    assert_eq!(rec["status"], "passed");
    assert_eq!(rec["criteria"][0]["criterionKey"], "tests pass");
    assert_eq!(rec["criteria"][0]["passed"], true);
    assert_eq!(rec["checks"][0]["check"], "cargo test -p faktor-session");
    assert_eq!(rec["checks"][0]["program"], "cargo");
    assert_eq!(rec["checks"][0]["args"], serde_json::json!(["test"]));
    assert_eq!(rec["checks"][0]["status"], "passed");
    assert_eq!(rec["checks"][0]["exit"], 0);
    assert_eq!(rec["changedFiles"][0]["path"], "crates/server/src/api.rs");
    assert_eq!(rec["changedFiles"][0]["size"], 42);
    assert_eq!(rec["unrelatedChanges"], serde_json::json!(["README.md"]));
    assert_eq!(rec["completedMs"], 2);
    // P0 proof payload (additive strict fields): the CandidateProofRef,
    // the verified candidate snapshot, the run base it was based on, the
    // integration source count and the landed final snapshot.
    assert_eq!(rec["candidateProof"]["taskRevision"], "1");
    assert_eq!(rec["candidateProof"]["runId"], "run-ver-a");
    assert_eq!(rec["candidateProof"]["candidateSnapshot"], "44".repeat(32));
    assert_eq!(rec["candidateProof"]["runBaseSnapshot"], "33".repeat(32));
    assert_eq!(rec["candidateProof"]["sourcesDigest"], "55".repeat(32));
    assert_eq!(rec["candidateProof"]["changedFilesDigest"], "66".repeat(32));
    assert_eq!(rec["verifiedSnapshot"], "44".repeat(32));
    assert_eq!(rec["basedOnSnapshot"], "33".repeat(32));
    assert_eq!(rec["sourceCount"], 3);
    assert_eq!(rec["landedSnapshot"], "44".repeat(32));

    // B never sees A's task-7 evidence: 7 is not B's task → typed 404.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/tasks/7/verification", b.id()),
    )
    .await;
    assert_eq!(resp.status(), 404);
    let err: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], "not_found");
    // A cannot read B's task-9 records either.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/tasks/9/verification", a.id()),
    )
    .await;
    assert_eq!(resp.status(), 404);
    // B's OWN task 9 serves its record.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/tasks/9/verification", b.id()),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["recordId"], rb.to_string());
    assert_eq!(
        records[0]["checks"][0]["check"],
        "cargo test -p faktor-server"
    );
    // Legacy NULL-evidence row: the P0 fields are honest absences (the
    // verified snapshot falls back to the stored tree hash; no
    // integration exists for task 9).
    assert_eq!(records[0]["candidateProof"], serde_json::Value::Null);
    assert_eq!(records[0]["verifiedSnapshot"], "ab".repeat(32));
    assert_eq!(records[0]["basedOnSnapshot"], serde_json::Value::Null);
    assert_eq!(records[0]["sourceCount"], serde_json::Value::Null);
    assert_eq!(records[0]["landedSnapshot"], serde_json::Value::Null);
    // C (another workspace, SAME numeric task id 7) cannot reach A's
    // record: records certify A's workspace, so C's view is the honest
    // empty list — evidence never crosses sessions or workspaces.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/tasks/7/verification", c.id()),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["records"],
        serde_json::json!([]),
        "records of another workspace never surface: {body}"
    );

    // Hostile ids, unknown sessions, auth.
    for path in [
        format!("/native/session/{}/tasks/0/verification", a.id()),
        format!("/native/session/{}/tasks/abc/verification", a.id()),
        "/native/session/abc/tasks/7/verification".to_string(),
        "/native/session/0/tasks/7/verification".to_string(),
    ] {
        let resp = native_get(&client, &base, &token, &path).await;
        assert_eq!(resp.status(), 400, "{path}");
    }
    let resp = native_get(
        &client,
        &base,
        &token,
        "/native/session/999999/tasks/7/verification",
    )
    .await;
    assert_eq!(resp.status(), 404);
    let resp = client
        .get(format!(
            "{base}/native/session/{}/tasks/7/verification",
            a.id()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_verification_serves_criterion_binding_origin_requirement_and_verdict() {
    // P0 UI proof blockers: every criterion row of the task-verification
    // projection additively carries the typed binding (kind + members),
    // the flat binding kind, the three-way verdict
    // (pass|fail|unavailable) alongside the recorded `passed` boolean,
    // and — when the session's typed task row holds the criterion — its
    // origin and requirement. All SEVEN binding kinds render; an
    // unbound (legacy) row is `unavailable`, which is distinct from
    // both `fail` and `pass`.
    use faktor_core::state::{CriterionBinding, CriterionOrigin, CriterionRequirement, TaskState};
    use faktor_session::task::{encode_criteria, Criterion};

    struct Spec {
        key: &'static str,
        binding: CriterionBinding,
        passed: bool,
        origin: CriterionOrigin,
        requirement: CriterionRequirement,
    }

    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/ver-proof").unwrap();
    let s = manager
        .create_session(ws, "t-ver-proof", "fake", "m")
        .unwrap();
    let task_id = s.task_id().unwrap();

    let specs = [
        Spec {
            key: "required check proof",
            binding: CriterionBinding::RequiredCheck {
                check_id: "rust_check".into(),
                command_digest: "digest-check".into(),
            },
            passed: true,
            origin: CriterionOrigin::User,
            requirement: CriterionRequirement::Required,
        },
        Spec {
            key: "integration coverage proof",
            binding: CriterionBinding::IntegrationCoverage {
                required_work_items: vec!["impl-a".into(), "impl-b".into()],
            },
            passed: true,
            origin: CriterionOrigin::ProjectPolicy,
            requirement: CriterionRequirement::Required,
        },
        Spec {
            key: "file state proof",
            binding: CriterionBinding::FileState {
                path: "src/a.rs".into(),
                expected_digest: "digest-file".into(),
            },
            passed: true,
            origin: CriterionOrigin::VerificationPolicy,
            requirement: CriterionRequirement::Required,
        },
        Spec {
            key: "evidence proof",
            binding: CriterionBinding::Evidence {
                evidence_id: "41".into(),
                evidence_digest: "digest-evidence".into(),
            },
            passed: true,
            origin: CriterionOrigin::SemanticProvider,
            requirement: CriterionRequirement::Preferred,
        },
        Spec {
            key: "independent review proof",
            binding: CriterionBinding::IndependentReview {
                reviewer_id: "reviewer-1".into(),
            },
            passed: true,
            origin: CriterionOrigin::ProjectPolicy,
            requirement: CriterionRequirement::Required,
        },
        Spec {
            key: "aggregate goal proof",
            binding: CriterionBinding::AggregateGoal,
            passed: false,
            origin: CriterionOrigin::VerificationPolicy,
            requirement: CriterionRequirement::Preferred,
        },
        Spec {
            key: "explicitly unavailable proof",
            binding: CriterionBinding::Unavailable {
                reason: "no objective mechanism".into(),
            },
            passed: true,
            origin: CriterionOrigin::User,
            requirement: CriterionRequirement::Required,
        },
    ];
    let typed: Vec<Criterion> = specs
        .iter()
        .map(|spec| {
            Criterion::derived(spec.key, spec.origin, spec.requirement, None)
                .with_binding(spec.binding.clone())
        })
        .collect();
    let now = s.now_ms();
    s.create_task(faktor_session::Task {
        task_id,
        session_id: s.id(),
        goal: "prove every binding kind".into(),
        acceptance_criteria: encode_criteria(&typed),
        plan: Vec::new(),
        attachments: Vec::new(),
        budget: Default::default(),
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();

    let row = s.row().unwrap();
    let mut criteria: Vec<faktor_core::state::CriterionVerification> = specs
        .iter()
        .map(|spec| faktor_core::state::CriterionVerification {
            criterion_key: spec.key.into(),
            passed: spec.passed,
            evidence: Some(format!("evidence for {}", spec.key)),
            binding: Some(spec.binding.clone()),
        })
        .collect();
    // A legacy unbound row: the recorded boolean contract survives, the
    // new three-way verdict must honestly say `unavailable`.
    criteria.push(faktor_core::state::CriterionVerification {
        criterion_key: "legacy unbound proof".into(),
        passed: true,
        evidence: None,
        binding: None,
    });
    let record = faktor_store::VerificationRecordRow {
        id: faktor_core::id::VerificationRecordId::new(1),
        task_id,
        revision: faktor_core::id::TaskRevision::new(1),
        workspace_id: row.workspace_id,
        worktree_id: row.worktree_id,
        tree_hash: Some("ab".repeat(32)),
        criteria,
        checks: Vec::new(),
        changed_files: Vec::new(),
        unrelated_changes: Vec::new(),
        reviewer: None,
        status: faktor_core::state::VerificationStatus::Passed,
        started_ms: 11,
        completed_ms: Some(22),
    };
    manager.store().verification_record_put(&record).unwrap();

    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/tasks/{}/verification", s.id(), task_id),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{body}");
    let rec = &records[0];
    assert_eq!(rec["startedMs"], 11);
    assert_eq!(rec["completedMs"], 22);
    let rows = rec["criteria"].as_array().unwrap();
    assert_eq!(rows.len(), specs.len() + 1);
    let find = |key: &str| -> serde_json::Value {
        rows.iter()
            .find(|r| r["criterionKey"] == key)
            .unwrap_or_else(|| panic!("criterion {key} must be served"))
            .clone()
    };

    // All seven binding kinds with their served origin/requirement and
    // the three-way verdict.
    let expected: [(&str, &str, &str, &str, &str); 7] = [
        (
            "required check proof",
            "required_check",
            "user",
            "required",
            "pass",
        ),
        (
            "integration coverage proof",
            "integration_coverage",
            "project_policy",
            "required",
            "pass",
        ),
        (
            "file state proof",
            "file_state",
            "verification_policy",
            "required",
            "pass",
        ),
        (
            "evidence proof",
            "evidence",
            "semantic_provider",
            "preferred",
            "pass",
        ),
        (
            "independent review proof",
            "independent_review",
            "project_policy",
            "required",
            "pass",
        ),
        (
            "aggregate goal proof",
            "aggregate_goal",
            "verification_policy",
            "preferred",
            "fail",
        ),
        (
            "explicitly unavailable proof",
            "unavailable",
            "user",
            "required",
            "unavailable",
        ),
    ];
    for (key, kind, origin, requirement, verdict) in expected {
        let row = find(key);
        assert_eq!(row["bindingKind"], kind, "{row}");
        assert_eq!(row["origin"], origin, "{row}");
        assert_eq!(row["requirement"], requirement, "{row}");
        assert_eq!(row["verdict"], verdict, "{row}");
        assert_eq!(row["binding"]["kind"], kind, "{row}");
    }
    // The typed binding members ride the wire in the serde shape.
    assert_eq!(
        find("required check proof")["binding"]["check_id"],
        "rust_check"
    );
    assert_eq!(
        find("required check proof")["binding"]["command_digest"],
        "digest-check"
    );
    assert_eq!(
        find("integration coverage proof")["binding"]["required_work_items"],
        serde_json::json!(["impl-a", "impl-b"])
    );
    assert_eq!(find("file state proof")["binding"]["path"], "src/a.rs");
    assert_eq!(find("evidence proof")["binding"]["evidence_id"], "41");
    assert_eq!(
        find("independent review proof")["binding"]["reviewer_id"],
        "reviewer-1"
    );
    assert_eq!(
        find("explicitly unavailable proof")["binding"]["reason"],
        "no objective mechanism"
    );
    // A verdict without a binding (legacy row) is unavailable, never a
    // pass — the recorded `passed` boolean stays alongside it.
    let legacy = find("legacy unbound proof");
    assert_eq!(legacy["passed"], true);
    assert_eq!(legacy["binding"], serde_json::Value::Null);
    assert_eq!(legacy["bindingKind"], "unavailable");
    assert_eq!(legacy["verdict"], "unavailable");
    assert_ne!(legacy["verdict"], "pass");
    // The three-way verdict distinguishes fail from unavailable AND
    // pass from unavailable.
    assert_ne!(find("explicitly unavailable proof")["verdict"], "fail");
    assert_ne!(find("explicitly unavailable proof")["verdict"], "pass");
    assert_ne!(find("aggregate goal proof")["verdict"], "unavailable");
    assert_ne!(find("aggregate goal proof")["verdict"], "pass");
    // No typed task criterion matches the legacy row: honest nulls.
    assert_eq!(legacy["origin"], serde_json::Value::Null);
    assert_eq!(legacy["requirement"], serde_json::Value::Null);

    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_tasks_carry_progress_and_durable_budget() {
    // P0-64d: the /native/session/{id}/tasks entry additively carries
    // `progress` (the live bounded progress record, null when nothing
    // ran) and `budget` — the DURABLE budget envelope of the typed task
    // row (token + cost-ledger columns and the open reservation sum).
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/task-budget").unwrap();
    let s = manager
        .create_session(ws, "t-task-budget", "fake", "m")
        .unwrap();
    let sid = s.id().to_string();
    let h = manager.get_session(s.id()).unwrap().unwrap();
    // Durable task ledger (the tasks endpoint's base) + a typed row with
    // a budget + one open and one settled reservation.
    h.put_task_ledger(serde_json::json!({
        "goal": "wire durable budgets",
        "completed_steps": ["mount endpoints"],
        "open_steps": ["ship"],
        "changed_files": ["crates/server/src/api.rs"],
    }))
    .unwrap();
    seed_typed_task(&h, 1, Some(8000), Some(4), "wire durable budgets");
    let store = manager.store();
    store
        .cost_task_cap_set(s.id(), faktor_core::id::TaskId::new(1), Some(250_000))
        .unwrap();
    let now = manager.now_ms();
    store
        .cost_reserve(
            s.id(),
            faktor_core::id::TaskId::new(1),
            manager.try_next_op_id().unwrap(),
            60,
            now,
        )
        .unwrap();
    let faktor_store::CostReserveOutcome::Granted(settled_id) = store
        .cost_reserve(
            s.id(),
            faktor_core::id::TaskId::new(1),
            manager.try_next_op_id().unwrap(),
            500,
            now,
        )
        .unwrap()
    else {
        panic!("settled reserve granted");
    };
    store
        .cost_settle(settled_id, 100, Some(100), None, None, now + 1)
        .unwrap();

    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tasks"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let tasks = body.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    let entry = &tasks[0];
    assert!(
        entry.get("progress").is_some(),
        "progress key present: {entry}"
    );
    assert_eq!(entry["budget"]["maxTokens"], 8000);
    assert_eq!(entry["budget"]["maxTurns"], 4);
    assert_eq!(entry["budget"]["spentTokens"], 0);
    assert_eq!(entry["budget"]["spentTurns"], 0);
    assert_eq!(money(&entry["budget"]["maxCostMicro"]), 250_000);
    assert_eq!(money(&entry["budget"]["spentCostMicro"]), 100);
    assert_eq!(money(&entry["budget"]["openReservedMicro"]), 60);
    let _ = handle.request_shutdown();
}

// ---------------------------------------- max_cost_micro task control E2E
// (audit 9/H: TaskRunRequest.max_cost_micro flows to the task row cap and a
// REAL drive whose first model-call reserve exceeds the cap fails with the
// typed budget refusal — nothing is reserved, nothing is spent.)

#[tokio::test]
async fn native_single_item_task_max_cost_micro_caps_the_real_drive() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps_full(dir.path(), vec![Arc::new(CacheUsageProvider)]);
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let tasks = deps.tasks.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/cap-e2e").unwrap();
    let s = manager
        .create_session(ws, "t-cap-e2e", "fake", "m")
        .unwrap();
    let sid = s.id();
    // A 1-micro cap: every real reserve of the drive (the route estimate is
    // far larger) is refused at admission — the refusal writes NOTHING.
    let req = faktor_orchestrator::runtime::task_executor::TaskRunRequest {
        goal: "spend against the cost cap".into(),
        work_items: vec![faktor_orchestrator::WorkItem::new(
            "a1",
            "spend against the cost cap",
            faktor_orchestrator::WorkKind::Analysis,
        )],
        max_cost_micro: Some(1),
        ..Default::default()
    };
    let receipt = tasks.start_task(sid, req).expect("single-item start");
    assert_eq!(
        receipt.mode,
        faktor_orchestrator::runtime::task_executor::TaskRunMode::InSession
    );
    // The cap was durable before the detached drive ran its first call...
    let h = manager.get_session(sid).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let ledger = faktor_session::DurableBudgetLedger::new(manager.clone());
    assert_eq!(
        ledger
            .session_budget_view(sid, task_id)
            .expect("durable budget view")
            .max_cost_micro,
        Some(1),
        "the request cap landed on the task row"
    );
    // ...and the drive ends FailedRecoverable (budget exceeded): the spy
    // provider was never billed — no reservation row, zero spent.
    let mut seen = None;
    for _ in 0..240 {
        let state = h.state().unwrap();
        if state == faktor_core::state::AgentState::FailedRecoverable {
            seen = Some(state);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        seen,
        Some(faktor_core::state::AgentState::FailedRecoverable),
        "the capped drive must fail recoverable, never silently spend"
    );
    let view = ledger
        .session_budget_view(sid, task_id)
        .expect("durable budget view");
    assert_eq!(view.max_cost_micro, Some(1));
    assert_eq!(view.spent_cost_micro, 0, "a refused reserve spends nothing");
    assert_eq!(view.open_reservations, 0, "a refused reserve writes no row");
    assert_eq!(view.uncertain_reservations, 0);
    // The native agent listing reflects the failed run.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/agents?session={sid}"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let entries: serde_json::Value = resp.json().await.unwrap();
    let e = &entries.as_array().unwrap()[0];
    assert_eq!(e["kind"], "self");
    assert_eq!(e["state"], "Failed");
    let _ = handle.request_shutdown();
}

// =========================================== native task runs (wave-24)
// The native task-start surface: POST /native/session/{id}/task-runs is
// the ONE HTTP edge into TaskExecutor::start_task (shadow mutation is
// the production default; DirectCompat keeps the byte-identical direct
// behavior), GET list/state read the durable runs, and the task-level
// cancel is the executor's single cancel authority. Tests below drive
// REAL shadowed worktrees through the HTTP layer, attack the strict
// DTO, freeze the parity of DirectCompat, and source-scan the crate for
// any second task-start edge.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_task_run_start_shadowed_drive_keeps_checkout_then_integrates() {
    // The wave-24 E2E through the HTTP layer: POST starts a mutating
    // single-item task (production default: shadowed) that really
    // drives; the run's write lands in the SHADOW while the owner
    // checkout stays byte-untouched MID-drive; the run listing/state
    // endpoints reflect it; a verified completion integrates the owner
    // checkout through the executor's own settle paths (never a manual
    // finalize call from the test).
    let dir = tempfile::tempdir().unwrap();
    let rig = native_task_rig_verified(
        dir.path(),
        vec![
            vec![
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": NATIVE_IMPL_LIB_RS,
                    }),
                },
                faktor_provider::ScriptedResponse::Text("done".into()),
                faktor_provider::ScriptedResponse::End,
            ],
            vec![faktor_provider::ScriptedResponse::End],
        ],
        true,
        true,
    );
    seed_native_owner(&rig.owner_root);
    let NativeTaskRig {
        deps,
        manager,
        parent: sid,
        owner_root,
        gate,
        fired,
    } = rig;
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // POST the strict native start (goal only: absent work_items = one
    // MUTATING main item; absent mutation_mode = the daemon default
    // Shadow).
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"goal": "implement the change"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let start: serde_json::Value = resp.json().await.unwrap();
    let run_id = start["run_id"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("tx-"), "{start}");
    assert_eq!(start["task_id"], 1);
    let row = manager
        .shadow_row(sid)
        .unwrap()
        .expect("shadow row at begin");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    assert_eq!(row.state, faktor_session::ShadowRowState::Active);

    // Mid-drive: the first write landed inside the SHADOW and parked
    // the drive; the user checkout is byte-untouched.
    for _ in 0..3000 {
        if fired.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(shadow_dir.join("src/lib.rs")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "the write landed in the SHADOW"
    );
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_OWNER_LIB_RS.as_bytes(),
        "the owner checkout is byte-untouched MID-drive"
    );
    assert_eq!(
        manager.active_root(sid).unwrap(),
        Some(shadow_dir.clone()),
        "the live shadow re-points the session"
    );
    // The task-runs list reflects the live run with its state.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/task-runs"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let list: serde_json::Value = resp.json().await.unwrap();
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["run_id"] == run_id)
        .expect("the live run is listed");
    assert_eq!(entry["task_id"], 1);
    assert_eq!(entry["mode"], "in_session");
    assert_eq!(entry["goal"], "implement the change");
    assert_eq!(entry["item_ids"], serde_json::json!(["main"]));
    // The per-run state read matches the list entry.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/task-runs/{run_id}"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let one: serde_json::Value = resp.json().await.unwrap();
    for key in ["task_id", "run_id", "mode", "goal", "item_ids"] {
        assert_eq!(one.get(key), entry.get(key), "{key}");
    }

    // Release the drive; the executor's own isolation pipeline
    // (prepare the candidate from the shadow run base → verify the
    // CANDIDATE with the configured verifier → land the owner
    // transactionally → complete the manifest-bound task → retire the
    // shadow) runs through its settle paths — no finalize endpoint, no
    // manual certification.
    gate.notify_waiters();
    for _ in 0..600 {
        let ok = std::fs::read(owner_root.join("src/lib.rs"))
            .map(|b| b == NATIVE_IMPL_LIB_RS.as_bytes())
            .unwrap_or(false)
            && !shadow_dir.exists();
        if ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "VerifiedComplete integration lands the changed file"
    );
    assert!(!shadow_dir.exists(), "clean integration removes the shadow");
    assert_eq!(
        manager.shadow_row(sid).unwrap().unwrap().state,
        faktor_session::ShadowRowState::Integrated
    );
    // The run's terminal state reads Done on the task-runs surface.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/task-runs/{run_id}"),
    )
    .await;
    let done: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(done["state"], "Done", "{done}");
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_shadow_run_via_extension_client_session_survives_worktree_id_shift() {
    // Shadow 409 root cause, end to end through the EXACT extension
    // client path: `POST /session/create` (workspace root) followed by
    // `POST /native/session/{id}/task-runs` with the shadow default.
    // The regression: a session created over HTTP carries the
    // standalone default worktree 1. When its workspace ALREADY holds
    // an owner worktree row with another id (any worktree row from an
    // earlier project on the same daemon), the executor's adoption
    // early-returned on "workspace has worktrees" and the shadowed run
    // refused with a typed 409 "no registered worktree row". The daemon
    // must register the session's own workspace/worktree at creation
    // and self-heal older sessions at task-run start.
    let dir = tempfile::tempdir().unwrap();
    let rig = native_task_rig(
        dir.path(),
        vec![
            vec![
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": NATIVE_IMPL_LIB_RS,
                    }),
                },
                faktor_provider::ScriptedResponse::Text("done".into()),
                faktor_provider::ScriptedResponse::End,
            ],
            vec![faktor_provider::ScriptedResponse::End],
        ],
        false,
        true,
    );
    seed_native_owner(&rig.owner_root);
    let NativeTaskRig {
        deps,
        manager,
        owner_root,
        ..
    } = rig;
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // Force the regression's precondition: the owner workspace's
    // worktree row is NOT id 1 (a decoy workspace registered first,
    // then the owner row recreated), so the standalone default 1 names
    // no row of this workspace. The rig's own session is irrelevant.
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    manager
        .remove_worktree(owner_root.to_str().unwrap())
        .unwrap();
    let decoy = manager
        .create_workspace(dir.path().join("decoy").to_str().unwrap())
        .unwrap();
    let decoy_wt = manager
        .put_worktree(decoy, dir.path().join("decoy").to_str().unwrap(), "main")
        .unwrap();
    assert_eq!(
        decoy_wt, 1,
        "the standalone default id is taken by the decoy"
    );
    let owner_wt = manager
        .put_worktree(ws, owner_root.to_str().unwrap(), "main")
        .unwrap();
    assert_ne!(
        owner_wt, 1,
        "the owner row id shifted away from the default"
    );

    // The native session creation path leaves the standalone default
    // worktree; the self-heal below forces the regression precondition
    // (a session naming no row of its workspace) before the task start.
    let sid = manager
        .create_session(ws, "extension session", "fake", "m")
        .unwrap()
        .id();

    // Adversarial self-heal probe: an OLDER session (or one created
    // before creation-time registration existed) still holds the
    // standalone default. The task-run start must re-register it
    // instead of refusing the shadowed run with a 409.
    manager
        .adopt_identity(
            sid,
            faktor_core::id::WorktreeId::new(1),
            faktor_core::id::TaskId::new(1),
        )
        .unwrap();

    // The shadowed mutating run via the extension client path: goal
    // only, mutation_mode omitted = daemon default (Shadow).
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"goal": "implement the change"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "an unregistered session must be adopted at task-run start, never 409ed: {:?}",
        resp.text().await
    );
    native_wait_session_state(
        &manager,
        sid,
        faktor_core::state::AgentState::ReadyForNextTurn,
    )
    .await;
    // The run really worked in a daemon-owned shadow; the owner
    // checkout stayed byte-untouched.
    let shadow = manager
        .shadow_row(sid)
        .unwrap()
        .expect("an ordinary native mutating prompt must begin a shadow");
    assert_eq!(shadow.state, faktor_session::ShadowRowState::Active);
    assert_eq!(
        std::fs::read(std::path::Path::new(&shadow.root).join("src/lib.rs")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "the edit landed in the shadow"
    );
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_OWNER_LIB_RS.as_bytes(),
        "the owner checkout is byte-untouched"
    );
    assert_eq!(
        manager
            .get_session(sid)
            .unwrap()
            .unwrap()
            .row()
            .unwrap()
            .worktree_id,
        faktor_core::id::WorktreeId::new(owner_wt as u64),
        "the self-heal re-adopted the workspace owner row"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_mutating_multi_agent_task_isolated_then_explicit_integration_and_restart() {
    // The audit E2E: POST one 3-stage analysis -> implementation ->
    // review plan. Ownership is EXPLICIT per item (read-only items hold
    // NoWrites, the mutating item owns an IsolatedWorktree); the DAEMON
    // allocates the candidate root itself (the DTO carries no path).
    // Asserts: accepted, durable assignments, real child sessions,
    // implementation inside the daemon-owned candidate root, the owner
    // checkout untouched, an EXPLICIT integration that makes the
    // candidate visible to review, and a restart that preserves child
    // ids + run.
    use faktor_orchestrator::runtime::OrchestratorRuntime;

    let dir = tempfile::tempdir().unwrap();
    let rig = native_task_rig(
        dir.path(),
        vec![
            // child-0 analysis (read-only).
            vec![
                faktor_provider::ScriptedResponse::Text("analysis done".into()),
                faktor_provider::ScriptedResponse::End,
            ],
            // child-1 implementation: a REAL write inside its isolated
            // worktree. (The candidate starts empty; the write creates
            // `candidate.txt` at its root.)
            vec![
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "w1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "candidate.txt",
                        "content": NATIVE_IMPL_LIB_RS,
                    }),
                },
                faktor_provider::ScriptedResponse::Text("implemented".into()),
                faktor_provider::ScriptedResponse::End,
            ],
            // child-2 review (read-only).
            vec![
                faktor_provider::ScriptedResponse::Text("review ok".into()),
                faktor_provider::ScriptedResponse::End,
            ],
        ],
        false,
        false,
    );
    seed_native_owner(&rig.owner_root);
    let NativeTaskRig {
        deps,
        manager,
        parent: sid,
        owner_root,
        ..
    } = rig;
    let orchestrator = deps.orchestrator.clone();
    let tasks = deps.tasks.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "goal": "3-stage change",
            "work_items": [
                {"id": "analyze", "kind": "Analysis", "ownership": "no_writes"},
                {
                    "id": "implement",
                    "kind": "Implementation",
                    "depends_on": ["analyze"],
                    "ownership": "isolated_worktree",
                },
                {
                    "id": "review",
                    "kind": "Review",
                    "depends_on": ["implement"],
                    "ownership": "no_writes",
                },
            ],
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let start: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{start}");
    assert_eq!(start["task_id"], 1);
    let run_id = start["run_id"].as_str().unwrap().to_string();
    // The list surface reports the orchestrated mode (the POST receipt
    // predates the detached plan row).
    let mut modes = Vec::new();
    // Environment-independent wait (the fixed 200x25ms loop was
    // load-sensitive); the assertions and break condition are unchanged.
    let wait_deadline = std::time::Instant::now() + Duration::from_secs(240);
    while std::time::Instant::now() < wait_deadline {
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let list: serde_json::Value = resp.json().await.unwrap();
        modes = list
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["run_id"] == run_id.as_str())
            .map(|e| e["mode"].clone())
            .collect();
        if !modes.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(modes, vec![serde_json::json!("orchestrated")], "{start}");

    // Durable assignments exist BEFORE/while the children drive.
    let assignments = OrchestratorRuntime::assignment_rows(manager.clone(), sid, &run_id).unwrap();
    assert_eq!(assignments.len(), 3, "one durable assignment per item");
    let a_of = |id: &str| assignments.iter().find(|a| a.item_id == id).unwrap();
    assert_eq!(
        a_of("analyze").ownership,
        faktor_core::state::OwnershipSpec::NoWrites
    );
    assert_eq!(
        a_of("implement").ownership,
        faktor_core::state::OwnershipSpec::IsolatedWorktree
    );
    assert_eq!(
        a_of("review").ownership,
        faktor_core::state::OwnershipSpec::NoWrites
    );

    // Wait for the whole run to reach its terminal item states.
    for _ in 0..600 {
        let rows = OrchestratorRuntime::registry_rows(manager.clone(), sid, &run_id).unwrap();
        if rows.len() == 3 && rows.iter().all(|c| c.state.is_terminal()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rows = OrchestratorRuntime::registry_rows(manager.clone(), sid, &run_id).unwrap();
    assert_eq!(rows.len(), 3, "three real child rows");
    let row_of = |id: &str| rows.iter().find(|r| r.item_id == id).unwrap();
    for id in ["analyze", "implement", "review"] {
        assert_ne!(row_of(id).session_id, 0, "child {id} has a real session");
        assert_ne!(row_of(id).operation_id, 0, "child {id} was really driven");
        assert_eq!(row_of(id).state, faktor_orchestrator::ChildState::Done);
    }
    // Child sessions are visible on the native agents surface.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/agents?session={sid}"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let entries: serde_json::Value = resp.json().await.unwrap();
    let children: Vec<&serde_json::Value> = entries
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "child")
        .collect();
    assert_eq!(children.len(), 3, "children visible: {entries}");

    // The implementation ran in the DAEMON-allocated candidate root
    // (never a client path): its workspace lives under
    // `<run_roots.root()>/s<sid>/<run_id>`.
    let impl_row = row_of("implement");
    assert_eq!(
        impl_row.ownership,
        faktor_session::child::ChildOwnership::IsolatedWorktree
    );
    let impl_root = manager
        .workspace_root(WorkspaceId::new(impl_row.workspace_id))
        .unwrap()
        .expect("implementation workspace root");
    let expected = tasks
        .run_roots()
        .root()
        .join(format!("s{}", sid.raw()))
        .join(&run_id);
    assert!(
        std::path::Path::new(&impl_root).starts_with(&expected),
        "the implementation lives under the daemon-allocated candidate root \
             ({impl_root:?} vs {expected:?})"
    );
    assert!(
        std::path::Path::new(&impl_root).ends_with("child-1"),
        "the implementation child owns its own isolated child root"
    );
    assert_eq!(
        std::fs::read(std::path::Path::new(&impl_root).join("candidate.txt")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "the implementation wrote inside its candidate"
    );
    // Owner checkout unchanged until an EXPLICIT integration.
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_OWNER_LIB_RS.as_bytes(),
        "the owner checkout is byte-untouched before integration"
    );
    assert!(
        !owner_root.join("candidate.txt").exists(),
        "nothing of the candidate leaked into the owner checkout"
    );

    // Explicit integration of the candidate into the owner checkout.
    let cs = orchestrator
        .stage_child_changes(&impl_row.child_id)
        .unwrap();
    assert!(
        cs.files.iter().any(|f| f.path.ends_with("candidate.txt")),
        "the staged change set holds the candidate write: {:?}",
        cs.files
    );
    let approved: Vec<std::path::PathBuf> = cs
        .files
        .iter()
        .filter(|f| f.child_hash.is_some())
        .map(|f| f.path.clone())
        .collect();
    let outcome = orchestrator
        .approve_and_merge(&impl_row.child_id, &cs.id(), &approved, &[])
        .unwrap();
    assert!(
        outcome.merged.iter().any(|p| p.ends_with("candidate.txt")),
        "the candidate merged explicitly: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(owner_root.join("candidate.txt")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "integration lands the candidate in the owner checkout"
    );
    // Review now SEES the candidate: a reviewer tree copied from the
    // current parent state contains the integrated bytes.
    let reviewer = orchestrator.spawn_reviewer(&impl_row.child_id).unwrap();
    let reviewer_root = manager
        .workspace_root(WorkspaceId::new(reviewer.workspace_id))
        .unwrap()
        .expect("reviewer workspace root");
    assert_eq!(
        std::fs::read(std::path::Path::new(&reviewer_root).join("candidate.txt")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "review sees the integrated candidate"
    );
    let _ = handle.request_shutdown();
    drop(client);
    drop(orchestrator);
    drop(tasks);

    // Restart on the same data dir: the run + child ids survive.
    drop(manager);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let assignments2 =
        OrchestratorRuntime::assignment_rows(reopened.clone(), sid, &run_id).unwrap();
    assert_eq!(assignments2, assignments, "assignments survive a restart");
    let rows2 = OrchestratorRuntime::registry_rows(reopened, sid, &run_id).unwrap();
    assert_eq!(
        rows2.len(),
        4,
        "the three plan children + the reviewer survive"
    );
    let ids: Vec<&str> = rows2.iter().map(|r| r.child_id.as_str()).collect();
    for id in ["child-0", "child-1", "child-2"] {
        assert!(ids.contains(&id), "child id {id} survives: {ids:?}");
    }
    for r in &rows2 {
        assert_ne!(r.session_id, 0, "child session ids survive a restart");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_ordinary_prompt_uses_the_shadow_executor_and_keeps_the_owner_untouched() {
    // The NATIVE ordinary prompt (no explicit work items) keeps the
    // daemon default shadow mutation: its write lands in the daemon
    // shadow and the owner checkout stays byte-untouched until a
    // verified integration. (The moved coverage of the pre-regression
    // the native surface is where shadowing is the promise; the
    // retired compatibility surfaces were direct.)
    let dir = tempfile::tempdir().unwrap();
    let rig = native_task_rig(
        dir.path(),
        vec![vec![
            faktor_provider::ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/lib.rs",
                    "content": NATIVE_IMPL_LIB_RS,
                }),
            },
            faktor_provider::ScriptedResponse::Text("done".into()),
            faktor_provider::ScriptedResponse::End,
        ]],
        false,
        true,
    );
    seed_native_owner(&rig.owner_root);
    let NativeTaskRig {
        deps,
        manager,
        parent: sid,
        owner_root,
        ..
    } = rig;
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"goal": "implement the change"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    native_wait_session_state(
        &manager,
        sid,
        faktor_core::state::AgentState::ReadyForNextTurn,
    )
    .await;
    // The write stayed in the shadow; the owner checkout is untouched
    // until a verified integration.
    let shadow = manager
        .shadow_row(sid)
        .unwrap()
        .expect("an ordinary native mutating prompt must begin a shadow");
    assert_eq!(shadow.state, faktor_session::ShadowRowState::Active);
    assert_eq!(
        std::fs::read(std::path::Path::new(&shadow.root).join("src/lib.rs")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "the edit landed in the shadow"
    );
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_OWNER_LIB_RS.as_bytes(),
        "the owner checkout is byte-untouched"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_task_run_start_always_isolates_and_direct_compat_is_a_400() {
    // P0 isolation over the HTTP edge: (a) the removed `direct_compat`
    // value is a strict DTO rejection (400) before any drive or durable
    // run row; (b) the same start under the (wire-only) shadow policy
    // begins the isolated candidate — the owner checkout stays
    // byte-untouched MID-drive, and the write lands in the candidate.
    let dir = tempfile::tempdir().unwrap();
    let rig = native_task_rig(
        dir.path(),
        vec![
            vec![
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": NATIVE_IMPL_LIB_RS,
                    }),
                },
                faktor_provider::ScriptedResponse::Text("done".into()),
                faktor_provider::ScriptedResponse::End,
            ],
            vec![faktor_provider::ScriptedResponse::End],
        ],
        true,
        true,
    );
    seed_native_owner(&rig.owner_root);
    let NativeTaskRig {
        deps,
        manager,
        parent: sid,
        owner_root,
        gate,
        fired,
        ..
    } = rig;
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // (a) the removed escape hatch is a strict 400 and never drives.
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "goal": "implement the change",
            "mutation_mode": "direct_compat",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "direct_compat must stay a strict DTO 400"
    );
    assert_eq!(
        manager
            .get_session(sid)
            .unwrap()
            .unwrap()
            .message_count()
            .unwrap(),
        0,
        "a refused DTO never drives"
    );
    assert!(
        manager.shadow_row(sid).unwrap().is_none(),
        "a refused DTO never begins a shadow"
    );

    // (b) the same start under the only decodable policy isolates.
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "goal": "implement the change",
            "mutation_mode": "shadow",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let row = manager
        .shadow_row(sid)
        .unwrap()
        .expect("a mutating task-run must isolate");
    assert_eq!(row.state, faktor_session::ShadowRowState::Active);
    // The parked write fired inside the candidate and parked the drive;
    // the owner checkout is STILL byte-untouched mid-drive.
    for _ in 0..3000 {
        if fired.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(std::path::PathBuf::from(&row.root).join("src/lib.rs")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "the write landed in the isolated candidate"
    );
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_OWNER_LIB_RS.as_bytes(),
        "the owner checkout is byte-untouched mid-drive"
    );
    gate.notify_waiters();
    native_wait_session_state(
        &manager,
        sid,
        faktor_core::state::AgentState::ReadyForNextTurn,
    )
    .await;
    assert_eq!(
        std::fs::read(std::path::PathBuf::from(&row.root).join("src/lib.rs")).unwrap(),
        NATIVE_IMPL_LIB_RS.as_bytes(),
        "the write stays in the isolated candidate"
    );
    assert_eq!(
        std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
        NATIVE_OWNER_LIB_RS.as_bytes(),
        "the owner checkout stayed byte-untouched"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_task_run_start_hostile_dtos_are_typed_400s() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let sid = manager
        .create_session(ws, "hostile", "fake", "m")
        .unwrap()
        .id();

    // Unauthenticated is 401 before anything else.
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .json(&serde_json::json!({"goal": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let oversized_goal = "x".repeat(2001);
    let hostile_bodies: Vec<serde_json::Value> = vec![
        serde_json::json!({}),
        serde_json::json!({"goal": ""}),
        serde_json::json!({"goal": "x", "bogus": 1}),
        serde_json::json!({"goal": "x", "mutation_mode": "nonsense"}),
        // The removed direct-owner mode stays a strict DTO 400.
        serde_json::json!({"goal": "x", "mutation_mode": "direct_compat"}),
        serde_json::json!({"goal": "x", "mutation_mode": "Shadow"}),
        serde_json::json!({"goal": "x", "routing_mode": "economy"}),
        serde_json::json!({"goal": oversized_goal}),
        serde_json::json!({"goal": "x", "max_tokens": "many"}),
        serde_json::json!({"goal": "x", "criteria": (0..=faktor_session::MAX_TASK_CRITERIA).map(|i| format!("criterion {i}")).collect::<Vec<_>>()}),
        serde_json::json!({"goal": "x", "criteria": vec!["c".repeat(faktor_session::MAX_TASK_CRITERION_BYTES + 1)]}),
        serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "Implementation"}, {"id": "b", "kind": "Implementation"}]}),
        serde_json::json!({"goal": "x", "work_items": [{"id": "a a/..", "kind": "Analysis"}]}),
        serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "Analysis"}, {"id": "a", "kind": "Analysis"}]}),
        serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "NoSuchKind"}]}),
        serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "Analysis", "extra": 1}]}),
        serde_json::json!({"goal": "x", "work_items": "not-an-array"}),
    ];
    for body in hostile_bodies {
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "hostile body must 400: {body}");
    }
    // Non-JSON bodies are plain 400s; unknown sessions are 404s.
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("{base}/native/session/999999/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"goal": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // No provider call ever happened on the hostile attempts.
    let h = manager.get_session(sid).unwrap().unwrap();
    assert_eq!(h.message_count().unwrap(), 0, "hostile starts never drive");
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_tournament_start_hostile_dtos_are_typed_400s() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let sid = manager
        .create_session(ws, "hostile-tournament", "fake", "m")
        .unwrap()
        .id();

    // Unauthenticated is 401 before anything else.
    let resp = client
        .post(format!("{base}/native/session/{sid}/tournament"))
        .json(&serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let oversized_goal = "x".repeat(513);
    let hostile_bodies: Vec<serde_json::Value> = vec![
        serde_json::json!({}),
        serde_json::json!({"goal": "x", "criteria": ["c"]}),
        serde_json::json!({"goal": "", "criteria": ["c"], "n": 2}),
        serde_json::json!({"goal": "x", "criteria": [], "n": 2}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": 0}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": 1}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": 5}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": "two"}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2, "bogus": 1}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2, "mutation_mode": "nonsense"}),
        serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2, "mutation_mode": "direct_compat"}),
        serde_json::json!({"goal": oversized_goal, "criteria": ["c"], "n": 2}),
        serde_json::json!({"goal": "x", "criteria": ["c".repeat(600)], "n": 2}),
    ];
    for body in hostile_bodies {
        let resp = client
            .post(format!("{base}/native/session/{sid}/tournament"))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "hostile tournament body must 400: {body}"
        );
    }
    // Non-JSON bodies are plain 400s; unknown sessions are 404s; an
    // unknown tournament id is a 404 (never a phantom tournament).
    let resp = client
        .post(format!("{base}/native/session/{sid}/tournament"))
        .bearer_auth(token.as_str())
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("{base}/native/session/999999/tournament"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tournament/does-not-exist"),
    )
    .await;
    assert_eq!(resp.status(), 404);
    // No provider call ever happened on the hostile attempts.
    let h = manager.get_session(sid).unwrap().unwrap();
    assert_eq!(h.message_count().unwrap(), 0, "hostile starts never drive");
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_tournaments_list_summarizes_the_durable_fold() {
    // The additive listing folds the pinned tournament lifecycle rows:
    // id, state, candidate count, winner and the decision timestamp.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let s = manager
        .create_session(ws, "tour-list", "fake", "m")
        .unwrap();
    let sid = s.id();

    // Unauthenticated is 401.
    let resp = client
        .get(format!("{base}/native/session/{sid}/tournaments"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // A session without tournaments is an empty list, never a phantom.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tournaments"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );
    // Hostile session ids are typed (malformed 400 / unknown 404).
    let resp = native_get(&client, &base, &token, "/native/session/nope/tournaments").await;
    assert_eq!(resp.status(), 400);
    let resp = native_get(&client, &base, &token, "/native/session/999999/tournaments").await;
    assert_eq!(resp.status(), 404);

    // Seed one OPEN and one DECIDED tournament through the typed ledger.
    let criteria = vec![faktor_session::ledger::TournamentCriterionRow {
        id: "c1".into(),
        spec: "cargo test".into(),
    }];
    let candidates: Vec<faktor_session::ledger::TournamentCandidateRow> = (0..2)
        .map(|i| faktor_session::ledger::TournamentCandidateRow {
            child_id: format!("child-{i}"),
            worktree: String::new(),
            base_revision: String::new(),
        })
        .collect();
    s.ledger_tournament_started("tour-open", "run-open", "open goal", &criteria, &candidates)
        .unwrap();
    s.ledger_tournament_started("tour-done", "run-done", "done goal", &criteria, &candidates)
        .unwrap();
    s.ledger_tournament_decided(
        "tour-done",
        Some("child-1"),
        faktor_session::TOURNAMENT_OUTCOME_DECIDED,
        "winner child-1",
    )
    .unwrap();

    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tournaments"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let list: serde_json::Value = resp.json().await.unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    let open = list.iter().find(|e| e["id"] == "tour-open").unwrap();
    assert_eq!(open["state"], "open");
    assert_eq!(open["candidate_count"], 2);
    assert!(open["winner"].is_null());
    assert!(open["decided_ms"].is_null());
    let done = list.iter().find(|e| e["id"] == "tour-done").unwrap();
    assert_eq!(done["state"], "decided");
    assert_eq!(done["candidate_count"], 2);
    assert_eq!(done["winner"], "child-1");
    assert!(done["decided_ms"].as_i64().unwrap_or(0) > 0);
    // The listing is a durable fold: it survives a ledger compaction
    // (tournament rows are pinned).
    s.compact_typed_ledger().unwrap();
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tournaments"),
    )
    .await;
    let list: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_tournament_decide_and_abort_are_strict_and_engine_gated() {
    // The additive decide/abort routes over the typed ledger: happy
    // decide (deterministic winner + losers discarded), happy abort
    // (terminal row with the reason), and every refusal boundary —
    // unknown ids 404, non-open/no-eligible-winner 409, hostile bodies
    // and oversized reasons 400, missing auth 401. The engine stays the
    // ONE authority: the routes never mutate the fold directly.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let s = manager
        .create_session(ws, "tour-control", "fake", "m")
        .unwrap();
    let sid = s.id();

    let criteria = vec![faktor_session::ledger::TournamentCriterionRow {
        id: "c1".into(),
        spec: "cargo test".into(),
    }];
    let candidates: Vec<faktor_session::ledger::TournamentCandidateRow> = (0..2)
        .map(|i| faktor_session::ledger::TournamentCandidateRow {
            child_id: format!("child-{i}"),
            worktree: String::new(),
            base_revision: String::new(),
        })
        .collect();
    let derived = faktor_orchestrator::tournament::derive_check_specs(&[
        faktor_orchestrator::tournament::Criterion {
            id: "c1".into(),
            spec: "cargo test".into(),
        },
    ]);
    let settlement =
        |child_id: &str, rank: &str, cost: u64| faktor_session::ledger::TournamentSettlementRow {
            child_id: child_id.into(),
            worktree: String::new(),
            base_revision: String::new(),
            state: "done".into(),
            verification: Some(7),
            verification_pass: Some(true),
            checks: derived.clone(),
            review: Some(rank.into()),
            reviewer: Some("review-0".into()),
            cost_micro: cost,
            wall_ms: 100,
            reason: "settled".into(),
        };

    // Unauthenticated is 401 before anything else.
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-x/decide"
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // A tournament with no eligible candidate refuses decide as a typed
    // 409 (nothing was persisted; a second decide reads the same state).
    s.ledger_tournament_started("tour-bare", "run-bare", "bare goal", &criteria, &candidates)
        .unwrap();
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-bare/decide"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tournament/tour-bare"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["state"],
        "open"
    );

    // Happy decide: clean review outranks the cheaper concern reviewer.
    s.ledger_tournament_started(
        "tour-happy",
        "run-happy",
        "happy goal",
        &criteria,
        &candidates,
    )
    .unwrap();
    s.ledger_candidate_settled("tour-happy", &settlement("child-0", "clean", 500))
        .unwrap();
    s.ledger_candidate_settled("tour-happy", &settlement("child-1", "concern", 1))
        .unwrap();
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-happy/decide"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let decided: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(decided["tournament_id"], "tour-happy");
    assert_eq!(decided["winner"], "child-0");
    assert!(decided["rationale"]
        .as_str()
        .unwrap_or("")
        .contains("child-0"));
    let discarded = decided["discarded"].as_array().unwrap();
    assert_eq!(discarded.len(), 1);
    assert_eq!(discarded[0]["child_id"], "child-1");
    // Wrong state: deciding or aborting the decided tournament is 409.
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-happy/decide"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-happy/abort"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"reason": "too late"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/tournament/tour-happy"),
    )
    .await;
    let state: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(state["state"], "decided");
    assert_eq!(state["winner"], "child-0");
    assert_eq!(state["candidates"][1]["state"], "discarded");

    // Happy abort: the terminal row carries the reason and every
    // candidate is discarded; a second abort is a typed 409.
    s.ledger_tournament_started(
        "tour-abort",
        "run-abort",
        "abort goal",
        &criteria,
        &candidates,
    )
    .unwrap();
    s.ledger_candidate_settled("tour-abort", &settlement("child-0", "clean", 10))
        .unwrap();
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-abort/abort"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"reason": "operator stopped it"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let aborted: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(aborted["id"], "tour-abort");
    assert_eq!(aborted["state"], "aborted");
    assert!(aborted["winner"].is_null());
    for candidate in aborted["candidates"].as_array().unwrap() {
        assert_eq!(candidate["state"], "discarded");
    }
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-abort/abort"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // Hostile boundaries: unknown tournament 404, unknown session 404,
    // strict bodies 400 (unknown member / non-JSON / missing body /
    // oversized reason).
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/nope/decide"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/nope/abort"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .post(format!(
            "{base}/native/session/999999/tournaments/nope/decide"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    for body in [
        serde_json::json!({"bogus": 1}),
        serde_json::json!({"reason": "decide takes no reason"}),
    ] {
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-bare/decide"
            ))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "hostile decide body: {body}");
    }
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-bare/decide"
        ))
        .bearer_auth(token.as_str())
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-bare/decide"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/tournaments/tour-bare/abort"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"reason": "x".repeat(600)}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_task_run_list_state_and_cancel_reflect_the_durable_run() {
    // List/state/cancel over HTTP on a real session: a completed
    // read-only run reads Done on both surfaces and refuses cancel; a
    // mid-flight run is cancelled at the task level (durable row
    // Cancelled, drive aborted) and stays Cancelled; hostile ids stay
    // typed.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let sid = manager
        .create_session(ws, "list-cancel", "fake", "m")
        .unwrap()
        .id();

    // Fresh session: an empty task-run list and typed 404 per-run reads.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/task-runs"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );
    for hostile in ["tx-1", "run-x", "..", "a/b"] {
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs/{hostile}"),
        )
        .await;
        assert_eq!(resp.status(), 404, "hostile run id {hostile:?}");
    }

    // A completed read-only run (explicit Analysis item + criteria):
    // the list/state reflect the terminal run; cancelling it is a 409.
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "goal": "analyze the module boundaries",
            "criteria": ["the analysis names the seams"],
            "work_items": [{"id": "a1", "kind": "Analysis"}],
            "max_tokens": 100_000,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let start: serde_json::Value = resp.json().await.unwrap();
    let run_id = start["run_id"].as_str().unwrap().to_string();
    assert_eq!(start["task_id"], 1);
    // Criteria rode the durable task row.
    let h = manager.get_session(sid).unwrap().unwrap();
    assert_eq!(
        h.get_task(faktor_core::id::TaskId::new(1))
            .unwrap()
            .unwrap()
            .acceptance_criteria,
        vec!["the analysis names the seams".to_string()]
    );
    let mut state = None;
    for _ in 0..300 {
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs/{run_id}"),
        )
        .await;
        let v: serde_json::Value = resp.json().await.unwrap();
        state = Some(v.clone());
        if v["state"] == "Done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entry = state.expect("the run must settle");
    assert_eq!(entry["state"], "Done");
    assert_eq!(entry["run_id"], run_id);
    assert_eq!(entry["mode"], "in_session");
    assert_eq!(entry["goal"], "analyze the module boundaries");
    assert_eq!(entry["item_ids"], serde_json::json!(["a1"]));
    // The list carries the same projection.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/task-runs"),
    )
    .await;
    let list: serde_json::Value = resp.json().await.unwrap();
    let list_entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["run_id"] == run_id)
        .expect("settled run listed");
    assert_eq!(list_entry["state"], "Done");
    // A run whose task row is durably TERMINAL (verified complete)
    // refuses cancel — a typed 409, never a silent no-op.
    certify_native_task(&manager, sid);
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/task-runs/{run_id}/cancel"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "terminal runs refuse cancel");
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs/nope/cancel"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "unknown runs are typed 404s");
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_task_run_cancel_aborts_a_mid_flight_in_session_run() {
    // A mid-flight in-session drive (paced text-only provider, no
    // tools) is cancelled at the TASK level: the drive is aborted
    // durably, the task row turns Cancelled, and the task-runs surface
    // reads Cancelled.
    let dir = tempfile::tempdir().unwrap();
    let paced = PacedScriptedProvider::new(
        faktor_core::model::ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        vec![
            (0..400)
                .map(|i| faktor_provider::ScriptedResponse::Text(format!("tick {i}")))
                .chain(std::iter::once(faktor_provider::ScriptedResponse::End))
                .collect(),
            vec![faktor_provider::ScriptedResponse::End],
        ],
        10,
    );
    let deps = paced_test_deps(dir.path(), paced);
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/plain").unwrap();
    let sid = manager
        .create_session(ws, "cancel-mid", "fake", "m")
        .unwrap()
        .id();
    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "goal": "long-running analysis",
            "work_items": [{"id": "a1", "kind": "Analysis"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let start: serde_json::Value = resp.json().await.unwrap();
    let run_id = start["run_id"].as_str().unwrap().to_string();
    // Wait for the drive to be mid-flight (the session is actively
    // working — anything but parked/terminal), then cancel at the task
    // level.
    let wait_deadline = std::time::Instant::now() + Duration::from_secs(240);
    while std::time::Instant::now() < wait_deadline {
        let st = manager.get_session(sid).unwrap().unwrap().state().unwrap();
        if !st.is_terminal()
            && !matches!(
                st,
                faktor_core::state::AgentState::ReadyForNextTurn
                    | faktor_core::state::AgentState::Idle
                    | faktor_core::state::AgentState::Suspended
            )
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/task-runs/{run_id}/cancel"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["run_id"], run_id);
    assert_eq!(ack["cancelled"], true);
    // The durable outcome: task row Cancelled, session parked, task-runs
    // state Cancelled; a second cancel is a typed 409.
    let wait_deadline = std::time::Instant::now() + Duration::from_secs(240);
    while std::time::Instant::now() < wait_deadline {
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs/{run_id}"),
        )
        .await;
        let v: serde_json::Value = resp.json().await.unwrap();
        if v["state"] == "Cancelled" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let h = manager.get_session(sid).unwrap().unwrap();
    let task = h
        .get_task(faktor_core::id::TaskId::new(1))
        .unwrap()
        .unwrap();
    assert_eq!(
        task.state,
        faktor_core::state::TaskState::Cancelled,
        "the task row is durably Cancelled"
    );
    let resp = client
        .post(format!(
            "{base}/native/session/{sid}/task-runs/{run_id}/cancel"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        409,
        "a cancelled run is never cancelled twice"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_board_endpoints_are_scoped_strict_and_hostile_inputs_are_typed() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let ws = manager.create_workspace("/tmp").unwrap();
    let root = manager
        .create_session(ws, "board-root", "fake", "m")
        .unwrap();
    let child = manager
        .create_child_session(
            root.id(),
            ws,
            faktor_core::id::WorktreeId::new(1),
            faktor_core::id::TaskId::new(1),
            "fake",
            "m",
            "board-child",
            faktor_session::child::ChildOwnership::ReadOnlyShared,
        )
        .unwrap();
    // A second family: its board must never leak into the first's.
    let foreign = manager
        .create_session(ws, "foreign-root", "fake", "m")
        .unwrap();
    let root_post = root.board_post("root post", "root body", &[]).unwrap();
    let child_post = child.board_post("child post", "child body", &[]).unwrap();
    foreign
        .board_post("foreign post", "foreign body", &[])
        .unwrap();
    let pw = deps.server_password.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let get = |path: &str, auth: Option<&str>| {
        let mut rb = client.get(format!("{base}{path}"));
        if let Some(pw) = auth {
            rb = rb.header("x-faktor-server-password", pw);
        }
        rb.send()
    };
    let post = |path: &str, body: serde_json::Value| {
        client
            .post(format!("{base}{path}"))
            .header("x-faktor-server-password", pw.as_str())
            .json(&body)
            .send()
    };

    // Auth is required for both verbs.
    assert_eq!(
        get(&format!("/native/session/{}/board", root.id().raw()), None)
            .await
            .unwrap()
            .status(),
        401
    );

    // The root reads its family board: both posts, newest first, and the
    // board identity is the family root — there is no board-id input to
    // forge.
    let resp = get(
        &format!("/native/session/{}/board", root.id().raw()),
        Some(pw.as_str()),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let page: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(page["board_id"], root.id().raw());
    assert_eq!(page["revision"], child_post.revision);
    assert_eq!(page["posts"].as_array().unwrap().len(), 2);
    assert_eq!(page["posts"][0]["id"], child_post.id.raw());
    assert_eq!(page["posts"][1]["id"], root_post.id.raw());
    assert_eq!(page["posts"][0]["author_session"], child.id().raw());
    assert_eq!(page["has_more"], false);
    assert!(page["next_before_revision"].is_null());

    // The child reads the SAME family board (its own member view).
    let resp = get(
        &format!("/native/session/{}/board", child.id().raw()),
        Some(pw.as_str()),
    )
    .await
    .unwrap();
    let child_page: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(child_page["board_id"], root.id().raw());
    assert_eq!(child_page["posts"].as_array().unwrap().len(), 2);

    // The foreign family sees ONLY its own single-post board: knowing the
    // first family's session ids grants no board access.
    let resp = get(
        &format!("/native/session/{}/board", foreign.id().raw()),
        Some(pw.as_str()),
    )
    .await
    .unwrap();
    let foreign_page: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(foreign_page["board_id"], foreign.id().raw());
    assert_eq!(foreign_page["posts"].as_array().unwrap().len(), 1);
    assert_eq!(foreign_page["posts"][0]["subject"], "foreign post");

    // Cursor paging: limit=1 yields the newest post + the exclusive
    // older cursor; `since` then returns the older page.
    let resp = get(
        &format!("/native/session/{}/board?limit=1", root.id().raw()),
        Some(pw.as_str()),
    )
    .await
    .unwrap();
    let first: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(first["posts"].as_array().unwrap().len(), 1);
    assert_eq!(first["posts"][0]["id"], child_post.id.raw());
    assert_eq!(first["has_more"], true);
    let cursor = first["next_before_revision"].as_u64().unwrap();
    let resp = get(
        &format!(
            "/native/session/{}/board?since={cursor}&limit=1",
            root.id().raw()
        ),
        Some(pw.as_str()),
    )
    .await
    .unwrap();
    let second: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(second["posts"].as_array().unwrap().len(), 1);
    assert_eq!(second["posts"][0]["id"], root_post.id.raw());

    // Hostile/malformed queries are typed 400s, never a silent default.
    for query in [
        "?since=0",
        "?limit=0",
        "?limit=101",
        "?limit=abc",
        "?since=-1",
        "?bogus=1",
        "?since=1&limit=1&bogus=1",
    ] {
        let resp = get(
            &format!("/native/session/{}/board{query}", root.id().raw()),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 400, "{query}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "malformed", "{query}");
    }

    // Session path hostility: malformed id 400, unknown id 404 (no
    // phantom empty board for a session that does not exist).
    for (path, status) in [
        ("/native/session/nope/board", 400),
        ("/native/session/0/board", 400),
        ("/native/session/999999/board", 404),
    ] {
        assert_eq!(
            get(path, Some(pw.as_str())).await.unwrap().status(),
            status,
            "{path}"
        );
    }

    // Hostile bodies: strict DTOs and the session bounds both hold.
    for (body, status) in [
        (
            serde_json::json!({"subject": "s", "body": "b", "extra": 1}),
            400,
        ),
        (serde_json::json!({"subject": "s"}), 400),
        (serde_json::json!({"subject": "", "body": "b"}), 400),
        (
            serde_json::json!({"subject": "x".repeat(513), "body": "b"}),
            413,
        ),
        (
            serde_json::json!({"subject": "s", "body": "x".repeat(16 * 1024 + 1)}),
            413,
        ),
        (
            serde_json::json!({"subject": "s", "body": "b", "refs": "nope"}),
            400,
        ),
    ] {
        let resp = post(
            &format!("/native/session/{}/board", root.id().raw()),
            body.clone(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), status, "{body}");
    }

    // A valid post is durable and immediately visible as the newest row.
    let resp = post(
            &format!("/native/session/{}/board", child.id().raw()),
            serde_json::json!({"subject": "native post", "body": "from the child", "refs": ["evidence://1"]}),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let created: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(created["author_session"], child.id().raw());
    assert_eq!(created["subject"], "native post");
    let resp = get(
        &format!("/native/session/{}/board?limit=1", root.id().raw()),
        Some(pw.as_str()),
    )
    .await
    .unwrap();
    let newest: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        newest["posts"][0]["id"].as_u64().unwrap(),
        created["id"].as_u64().unwrap()
    );

    // A terminal child cannot post: the session-layer lifecycle rule
    // refuses BEFORE any durable write (typed 403).
    child
        .orchestrator_child_runtime_put(&faktor_session::child::ChildRuntimeBlockerRow {
            child_id: "board-child".into(),
            state: "cancelled".into(),
            blocker: None,
            updated_ms: 1,
        })
        .unwrap();
    let before = child.board_read_posts(None, None, 10, false).unwrap();
    let resp = post(
        &format!("/native/session/{}/board", child.id().raw()),
        serde_json::json!({"subject": "late", "body": "should refuse"}),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 403);
    let after = child.board_read_posts(None, None, 10, false).unwrap();
    assert_eq!(
        before.posts.len(),
        after.posts.len(),
        "a refused terminal post writes nothing"
    );
    let _ = handle.request_shutdown();
}
