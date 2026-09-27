use crate::api::tests::*;
use crate::api::*;

/// Audit 5/6 additive surface: `/native/index/coverage?session=<id>` is
/// auth-gated, strict (unknown query fields are 400), malformed session
/// ids are 400, and an index service that was never hosted answers the
/// typed `index_coverage: null` — never a fabricated "complete". The
/// hosted partial/complete snapshots are covered by the index and
/// runtime suites.
#[tokio::test]
async fn native_index_coverage_auth_strictness_and_unhosted_null() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let ws = deps.session.create_workspace("/tmp").unwrap();
    let sid = deps
        .session
        .create_session(ws, "t-cov", "fake", "m")
        .unwrap()
        .id()
        .to_string();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let resp = client
        .get(format!("{base}/native/index/coverage?session={sid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "coverage diagnostics are auth-gated");

    let resp = client
        .get(format!(
            "{base}/native/index/coverage?session={sid}&bogus=1"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "strict query DTO");

    let resp = client
        .get(format!(
            "{base}/native/index/coverage?session=not-a-session"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "malformed session id");

    let resp = client
        .get(format!("{base}/native/index/coverage?session={sid}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessionId"], sid);
    assert!(
        body["index_coverage"].is_null(),
        "an unhosted service is reported honestly: {body}"
    );
    drop(handle);
}

#[tokio::test]
async fn native_health_and_ready_semantics() {
    // health answers 200 whenever the process responds; ready answers
    // 200 ONLY after serve() setup completed (recovery ran, migrations
    // applied, components in place) — and 503 before that moment, which
    // the simulate_not_ready knob keeps observable in tests.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // Both are auth-gated like every daemon route.
    let resp = client
        .get(format!("{base}/native/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .get(format!("{base}/native/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Post-serve: health = liveness {ok, version}, ready = 200
    // {ready:true} (the flag flips at the very end of serve() setup, so
    // a test can only observe true after serve returns).
    let resp = client
        .get(format!("{base}/native/health"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(body["version"].is_string());
    let resp = client
        .get(format!("{base}/native/ready"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ready"], true);
    let _ = handle.request_shutdown();

    // The not-ready window (deterministic test knob): with
    // simulate_not_ready the flag never flips, so ready is 503
    // {ready:false} even after serve returned — health stays 200.
    let deps = {
        let mut d = test_deps(dir.path());
        d.simulate_not_ready = true;
        d
    };
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base2 = format!("http://{}", handle.addr);
    let resp = client
        .get(format!("{base2}/native/ready"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ready"], false);
    let resp = client
        .get(format!("{base2}/native/health"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = handle.request_shutdown();
}

/// The additive `/native/health` worker-plane entry: `disabled` by
/// default, `serving` + bind while the owned second listener is alive,
/// and `unavailable` + the typed code/message once that socket died
/// unexpectedly — an operator/placement decision can never mistake a
/// dead remote-worker socket for a live one. Existing keys stay
/// unchanged (additive key only) and the configured transport bearer
/// never enters the payload.
#[tokio::test]
async fn native_health_reports_the_worker_plane_typed_status() {
    use crate::worker_plane::{
        serve_worker_plane, WorkerPlaneAuth, WorkerPlaneBindConfig, WorkerPlaneHandle,
        WorkerPlaneTransport,
    };

    fn loopback_config(bearer: Option<&str>) -> WorkerPlaneBindConfig {
        WorkerPlaneBindConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            transport: WorkerPlaneTransport::Plaintext,
            trusted_gateway: false,
            auth: WorkerPlaneAuth::WorkerTokens,
            bearer: bearer.map(faktor_security::secret::SecretValue::new),
        }
    }

    /// Wait (bounded) until the injected serve task recorded its death.
    async fn wait_dead(handle: &WorkerPlaneHandle) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while handle.is_alive() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the injected serve death must land within the bound"
            );
            tokio::task::yield_now().await;
        }
    }

    // (1) Disabled (the `[worker_plane]` default, no slot wired): an
    // explicit disabled state; ok/version unchanged.
    {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let (body, _raw) = health_json(Arc::new(deps), &token).await;
        assert_eq!(body["ok"], true);
        assert!(body["version"].is_string());
        assert_eq!(body["worker_plane"]["state"], "disabled");
        assert!(body["worker_plane"].get("bind").is_none());
        assert!(body["worker_plane"].get("code").is_none());
        assert!(body["worker_plane"].get("message").is_none());
    }

    // (2) Serving: a REAL dedicated listener over the same deps. The
    // bound address is surfaced, the bearer is not, and the daemon
    // shutdown handoff (take -> join -> record) keeps the entry
    // truthful (`stopped`, never `disabled`/`serving`).
    {
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let listener = WorkerPlaneListener::default();
        deps = deps.with_worker_plane_listener(listener.clone());
        let deps = Arc::new(deps);
        let worker = serve_worker_plane(deps.clone(), loopback_config(Some("gateway-secret")))
            .await
            .unwrap();
        let worker_addr = worker.addr;
        listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .install(worker);
        let (body, raw) = health_json(deps.clone(), &token).await;
        assert_eq!(body["ok"], true);
        assert!(body["version"].is_string());
        assert_eq!(body["worker_plane"]["state"], "serving");
        assert_eq!(body["worker_plane"]["bind"], worker_addr.to_string());
        assert!(body["worker_plane"].get("code").is_none());
        assert!(
            !raw.contains("gateway-secret"),
            "the transport bearer must never enter health: {raw}"
        );
        let taken = listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_for_shutdown();
        let (handle, health) = taken.expect("the slot owns the installed handle");
        assert!(health.is_alive(), "{health:?}");
        handle.shutdown().await.unwrap();
        listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_terminal(WorkerPlaneStatus::Stopped);
        let health = deps.worker_plane_health();
        assert_eq!(health.state(), "stopped");
        assert_eq!(health.to_json()["bind"], worker_addr.to_string());
    }

    // (3) Injected serve failure: the entry reports `unavailable` with
    // the typed stable code and the cause; a hostile oversized error
    // message is bounded, never ballooning the payload.
    {
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let listener = WorkerPlaneListener::default();
        deps = deps.with_worker_plane_listener(listener.clone());
        let addr: std::net::SocketAddr = "127.0.0.1:8790".parse().unwrap();
        let exposure = loopback_config(None).validate().unwrap();
        let dead = WorkerPlaneHandle::failing_serve_for_test(
            addr,
            exposure,
            std::io::Error::other("injected accept failure"),
        );
        wait_dead(&dead).await;
        listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .install(dead);
        let (body, _raw) = health_json(Arc::new(deps), &token).await;
        assert_eq!(body["worker_plane"]["state"], "unavailable");
        assert_eq!(body["worker_plane"]["bind"], addr.to_string());
        assert_eq!(body["worker_plane"]["code"], "worker_plane_serve_failed");
        let message = body["worker_plane"]["message"].as_str().unwrap();
        assert!(message.contains("injected accept failure"), "{message}");

        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let listener = WorkerPlaneListener::default();
        deps = deps.with_worker_plane_listener(listener.clone());
        let exposure = loopback_config(None).validate().unwrap();
        let dead = WorkerPlaneHandle::failing_serve_for_test(
            addr,
            exposure,
            std::io::Error::other("x".repeat(4096)),
        );
        wait_dead(&dead).await;
        listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .install(dead);
        let (body, _raw) = health_json(Arc::new(deps), &token).await;
        let message = body["worker_plane"]["message"].as_str().unwrap();
        assert!(
            message.len() <= MAX_WORKER_PLANE_HEALTH_MESSAGE_BYTES + 4,
            "the health message must stay bounded: {} bytes",
            message.len()
        );
        assert!(message.ends_with('…'), "{message}");
    }
}

// ------------------------------------------ native server lifecycle/health

#[tokio::test]
async fn native_providers_registry_and_health_snapshot() {
    // P0-64c: the registry view carries provider identity, models with
    // real capabilities, source provenance and the honest health
    // snapshot; secrets never leak.
    let dir = tempfile::tempdir().unwrap();
    let mut caps = std::collections::HashMap::new();
    caps.insert(
        "gpt-x".to_string(),
        ModelCapabilities {
            context: 128_000,
            max_output: 16_384,
            tools: true,
            ..Default::default()
        },
    );
    let openai = faktor_openai::OpenAiProvider::build(
        faktor_openai::OpenAiConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: Some("sk-super-secret".into()),
            family: faktor_openai::OpenAiFamily::Chat,
            models: caps,
        },
        permissive_transport(),
    );
    let deps = test_deps_with(dir.path(), vec![openai]);
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let resp = native_get(&client, &base, &token, "/native/providers").await;
    assert_eq!(resp.status(), 200);
    let list: serde_json::Value = resp.json().await.unwrap();
    let entries = list.as_array().unwrap();
    assert!(entries.len() >= 2, "fake + openai registered: {list}");
    // Deterministic order by instance id.
    let ids: Vec<&str> = entries
        .iter()
        .map(|e| e["instanceId"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted);
    let fake = entries.iter().find(|e| e["instanceId"] == "fake").unwrap();
    assert_eq!(fake["family"], "fake");
    assert_eq!(fake["health"]["status"], "registered");
    assert!(fake["health"]["note"]
        .as_str()
        .unwrap()
        .contains("adapter-private"));
    assert!(!fake["models"].as_array().unwrap().is_empty());
    let oai = entries
        .iter()
        .find(|e| e["instanceId"] == "openai")
        .unwrap();
    let models = oai["models"].as_array().unwrap();
    let gpt_x = models.iter().find(|m| m["model"] == "gpt-x").unwrap();
    assert_eq!(gpt_x["context"], 128_000);
    assert_eq!(gpt_x["source"], "providerCatalog");
    assert_eq!(oai["runtimeContextLimitSupported"], false);
    // Secrets never reach this surface.
    let raw = list.to_string().to_lowercase();
    assert!(!raw.contains("sk-super-secret"), "api keys never leak");
    assert!(!raw.contains("api_key"));

    let resp = client
        .get(format!("{base}/native/providers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_semantic_status_falls_back_without_provider() {
    // No registry configured is a 200 fallback shape, never a 500.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let resp = client
        .get(format!("{base}/native/semantic/status"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["configured"], false);
    assert!(body["providers"].as_array().unwrap().is_empty());
    assert!(!body["fallback"]["id"].as_str().unwrap().is_empty());
    assert!(body["fallback"]["version"].is_u64());
    assert!(body["snapshotState"]["fallback"].is_boolean());
    // Capabilities mirror the same fallback-only registry.
    let resp = client
        .get(format!("{base}/native/semantic/capabilities"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["providers"].as_array().unwrap().is_empty());
    assert!(body["union"]["operations"]["snapshot"].is_boolean());
    assert!(!body["fallback"]["capabilities"]
        .as_object()
        .unwrap()
        .is_empty());
    let _ = handle.request_shutdown();
}
