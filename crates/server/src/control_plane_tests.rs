//! Native control-plane route tests (the `[cloud]` surface): typed
//! cloud-disabled parity, bootstrap, identity, members/invitations,
//! tenant isolation (no existence leak over HTTP), repository scoping and
//! pagination, approvals, idempotency keys and strict DTO refusals.

use super::tests::test_deps;
use super::*;
use faktor_cloud::{ControlPlane, ManualClock, MemoryControlPlaneStore, SqliteControlPlaneStore};
use faktor_scm::{MemoryScmStore, RepositoryRow, ScmStore};

const NOW_MS: i64 = 1_700_000_000_000;

/// One served daemon with the control plane + an SCM store wired. The SCM
/// store handle stays alive for seeding rows.
async fn cloud_deps(
    root: &std::path::Path,
) -> (ServerHandle, Arc<dyn ScmStore>, Arc<ControlPlane>, String) {
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let scm: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let deps = test_deps(root)
        .with_control_plane(control_plane.clone())
        .with_scm_store(scm.clone());
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    (handle, scm, control_plane, token.as_str().to_string())
}

fn url(base: &str, path: &str) -> String {
    format!("{base}{path}")
}

/// Bootstrap one organization over HTTP and return its owner token + org id.
async fn bootstrap_org(
    client: &reqwest::Client,
    base: &str,
    daemon_token: &str,
    name: &str,
    email: &str,
    key: &str,
) -> (String, String) {
    let resp = client
        .post(url(base, "/native/orgs"))
        .bearer_auth(daemon_token)
        .header("idempotency-key", key)
        .json(&serde_json::json!({
            "name": name, "owner_email": email, "display_name": "Owner",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["idempotent"], false);
    let org = body["organization"]["id"].as_str().unwrap().to_string();
    let token = body["token"].as_str().unwrap().to_string();
    (org, token)
}

#[tokio::test]
async fn cloud_disabled_answers_typed_409_and_changes_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    // No control plane and no SCM store are wired: the daemon is exactly the
    // pre-cloud daemon.
    assert!(!crate::native::control_plane_enabled(&deps));
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let cases: [(&str, &str, serde_json::Value); 7] = [
        ("GET", "/native/identity", serde_json::Value::Null),
        ("GET", "/native/orgs", serde_json::Value::Null),
        (
            "POST",
            "/native/orgs",
            serde_json::json!({"name": "X", "owner_email": "x@x.test", "display_name": "X"}),
        ),
        ("GET", "/native/orgs/org_1/members", serde_json::Value::Null),
        (
            "POST",
            "/native/orgs/org_1/members",
            serde_json::json!({"email": "x@y.test", "role": "member"}),
        ),
        ("GET", "/native/repositories", serde_json::Value::Null),
        ("GET", "/native/approvals", serde_json::Value::Null),
    ];
    for (method, path, body) in cases {
        let request = match method {
            "GET" => client.get(url(&base, path)),
            _ => client.post(url(&base, path)).json(&body),
        };
        let resp = request
            .bearer_auth(token.as_str())
            .header("idempotency-key", "k-disabled")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "{method} {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "cloud_disabled", "{method} {path}");
    }
    // The rest of the daemon is untouched: health/ready keep their exact
    // shapes.
    let health: serde_json::Value = client
        .get(url(&base, "/native/health"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    let ready = client
        .get(url(&base, "/native/ready"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
}

#[tokio::test]
async fn identity_bootstrap_and_tenant_isolation_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let (org_a, token_a) =
        bootstrap_org(&client, &base, &daemon, "Alpha", "a@alpha.test", "k-a").await;
    let (org_b, token_b) =
        bootstrap_org(&client, &base, &daemon, "Beta", "b@beta.test", "k-b").await;
    assert_ne!(org_a, org_b);

    // Identity reflects the token's tenant and role.
    let identity_resp = client
        .get(url(&base, "/native/identity"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(identity_resp.status(), 200, "identity must resolve");
    let identity: serde_json::Value = identity_resp.json().await.unwrap();
    assert_eq!(identity["identity"]["organization"], org_a);
    assert_eq!(identity["identity"]["role"], "owner");
    assert_eq!(identity["identity"]["subject_kind"], "user");

    // The daemon password is NOT a control-plane credential.
    let missing = client
        .get(url(&base, "/native/identity"))
        .bearer_auth(&daemon)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);

    // Foreign organization: byte-identical to a nonexistent id (404, no
    // existence leak), both for members and invitations.
    for foreign in [org_b.as_str(), "org_00000000000000000000000000000000"] {
        let response = client
            .get(url(&base, &format!("/native/orgs/{foreign}/members")))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_a)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "not_found");
        let rendered = body.to_string();
        assert!(!rendered.contains("Beta"), "no name leak: {rendered}");

        let invite = client
            .post(url(&base, &format!("/native/orgs/{foreign}/members")))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_a)
            .header("idempotency-key", "k-foreign")
            .json(&serde_json::json!({"email": "x@y.test", "role": "member"}))
            .send()
            .await
            .unwrap();
        assert_eq!(invite.status(), 404);
    }

    // The foreign org's own listing still works (isolation, not breakage).
    let own: serde_json::Value = client
        .get(url(&base, &format!("/native/orgs/{org_b}/members")))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(own["items"].as_array().unwrap().len(), 1);
    assert_eq!(own["items"][0]["role"], "owner");

    // Organization creation is a daemon bootstrap operation: a control-plane
    // principal cannot mint tenants.
    let denied = client
        .post(url(&base, "/native/orgs"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .header("idempotency-key", "k-mint")
        .json(&serde_json::json!({"name": "X", "owner_email": "x@x.test", "display_name": "X"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
}

#[tokio::test]
async fn members_invitations_and_approvals_flow_with_idempotency_keys() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, token) = bootstrap_org(&client, &base, &daemon, "Acme", "owner@acme.test", "k").await;
    let members_path = format!("/native/orgs/{org}/members");

    // Idempotency key is REQUIRED on mutations.
    let missing = client
        .post(url(&base, &members_path))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .json(&serde_json::json!({"email": "new@acme.test", "role": "member"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);

    let invite = client
        .post(url(&base, &members_path))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "inv-1")
        .json(&serde_json::json!({"email": "new@acme.test", "role": "member"}))
        .send()
        .await
        .unwrap();
    assert_eq!(invite.status(), 200);
    let invite_body: serde_json::Value = invite.json().await.unwrap();
    assert!(
        invite_body["token"].is_string(),
        "the token is presented once"
    );
    let invitation_id = invite_body["invitationId"].as_str().unwrap().to_string();
    assert_eq!(invite_body["status"], "pending");

    // The idempotent replay returns the same invitation and NO token.
    let replay = client
        .post(url(&base, &members_path))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "inv-1")
        .json(&serde_json::json!({"email": "new@acme.test", "role": "member"}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    let replay_body: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(replay_body["idempotent"], true);
    assert_eq!(
        replay_body["invitationId"].as_str(),
        Some(invitation_id.as_str())
    );
    assert!(
        replay_body["token"].is_null(),
        "replays never re-present secrets"
    );

    // The same key with a DIFFERENT request is a 409.
    let conflict = client
        .post(url(&base, &members_path))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "inv-1")
        .json(&serde_json::json!({"email": "other@acme.test", "role": "admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);

    // Approvals: request (idempotency-keyed), list, decide once.
    let approval = client
        .post(url(&base, "/native/approvals"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "apr-1")
        .json(&serde_json::json!({
            "action": "secret_write", "resource": "secret:prod", "reason": "rotate",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(approval.status(), 201);
    let approval_body: serde_json::Value = approval.json().await.unwrap();
    let approval_id = approval_body["approval"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(approval_body["approval"]["status"], "open");

    let listed: serde_json::Value = client
        .get(url(&base, "/native/approvals?status=open&limit=10"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert!(listed["nextCursor"].is_null());

    // Idempotency key is REQUIRED on decisions too.
    let missing_decide_key = client
        .post(url(
            &base,
            &format!("/native/approvals/{approval_id}/decide"),
        ))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .json(&serde_json::json!({"approved": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_decide_key.status(), 400);

    let decide = client
        .post(url(
            &base,
            &format!("/native/approvals/{approval_id}/decide"),
        ))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "dec-1")
        .json(&serde_json::json!({"approved": true, "note": "ok"}))
        .send()
        .await
        .unwrap();
    assert_eq!(decide.status(), 200);
    let decided: serde_json::Value = decide.json().await.unwrap();
    assert_eq!(decided["approval"]["status"], "approved");
    // A same-key retry replays the recorded decision.
    let replay_decide = client
        .post(url(
            &base,
            &format!("/native/approvals/{approval_id}/decide"),
        ))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "dec-1")
        .json(&serde_json::json!({"approved": true, "note": "ok"}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay_decide.status(), 200);
    // A second decision under a NEW key is a typed conflict.
    let again = client
        .post(url(
            &base,
            &format!("/native/approvals/{approval_id}/decide"),
        ))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .header("idempotency-key", "dec-2")
        .json(&serde_json::json!({"approved": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
}

#[tokio::test]
async fn repositories_are_tenant_scoped_and_cursor_paginated() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org_a, token_a) =
        bootstrap_org(&client, &base, &daemon, "Alpha", "a@alpha.test", "k-a").await;
    let (org_b, token_b) =
        bootstrap_org(&client, &base, &daemon, "Beta", "b@beta.test", "k-b").await;

    for index in 0..3 {
        scm.upsert_repository(&RepositoryRow {
            id: 0,
            installation_id: 7,
            organization_id: org_a.clone(),
            owner: "acme".into(),
            name: format!("widgets-{index}"),
            full_name: format!("acme/widgets-{index}"),
            default_branch: "main".into(),
            private: true,
            archived: false,
            updated_ms: NOW_MS,
        })
        .unwrap();
    }
    scm.upsert_repository(&RepositoryRow {
        id: 0,
        installation_id: 8,
        organization_id: org_b.clone(),
        owner: "beta".into(),
        name: "secret-repo".into(),
        full_name: "beta/secret-repo".into(),
        default_branch: "main".into(),
        private: true,
        archived: false,
        updated_ms: NOW_MS,
    })
    .unwrap();

    // Page 1 (limit 2) plus a cursor; page 2 returns the remainder.
    let page1: serde_json::Value = client
        .get(url(&base, "/native/repositories?limit=2"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    let cursor = page1["nextCursor"].as_str().unwrap().to_string();
    let page2: serde_json::Value = client
        .get(url(
            &base,
            &format!("/native/repositories?limit=2&cursor={cursor}"),
        ))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert!(page2["nextCursor"].is_null());

    // No row of another tenant, ever.
    let rendered = format!("{page1}{page2}");
    assert!(!rendered.contains("secret-repo"), "tenant leak: {rendered}");
    let other: serde_json::Value = client
        .get(url(&base, "/native/repositories"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(other["items"].as_array().unwrap().len(), 1);
    assert_eq!(other["items"][0]["fullName"], "beta/secret-repo");

    // Hostile cursors/limits are 400s.
    for query in ["limit=0", "limit=201", "cursor=not-a-number", "cursor=-1"] {
        let resp = client
            .get(url(&base, &format!("/native/repositories?{query}")))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_a)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{query}");
    }
}

#[tokio::test]
async fn strict_dtos_reject_unknown_fields_and_malformed_values() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, token) = bootstrap_org(&client, &base, &daemon, "Acme", "owner@acme.test", "k").await;

    // Unknown JSON fields are refused everywhere.
    for (path, body) in [
        (
            "/native/orgs",
            serde_json::json!({"name": "X", "owner_email": "x@x.test", "display_name": "X", "smuggled": 1}),
        ),
        (
            &format!("/native/orgs/{org}/members"),
            serde_json::json!({"email": "x@y.test", "role": "member", "smuggled": 1}),
        ),
        (
            "/native/approvals",
            serde_json::json!({"action": "run_create", "resource": "r", "smuggled": 1}),
        ),
    ] {
        let resp = client
            .post(url(&base, path))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token)
            .header("idempotency-key", "k-strict")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{path} {body}");
    }

    // Malformed values are typed 400s, never coerced.
    let members_path = format!("/native/orgs/{org}/members");
    let malformed_cases: [(&str, serde_json::Value); 3] = [
        (
            members_path.as_str(),
            serde_json::json!({"email": "not-an-email", "role": "member"}),
        ),
        (
            members_path.as_str(),
            serde_json::json!({"email": "x@y.test", "role": "superuser"}),
        ),
        (
            "/native/approvals",
            serde_json::json!({"action": "burn_it_all", "resource": "r"}),
        ),
    ];
    for (path, body) in malformed_cases {
        let resp = client
            .post(url(&base, path))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token)
            .header("idempotency-key", "k-malformed")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{path} {body}");
        let rendered: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(rendered["error"]["code"], "malformed");
    }

    // An unknown approval status filter is a 400.
    let resp = client
        .get(url(&base, "/native/approvals?status=bogus"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Oversized control-plane tokens are refused before any lookup.
    let resp = client
        .get(url(&base, "/native/identity"))
        .header("x-faktor-control-token", "x".repeat(5000))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn sqlite_backed_control_plane_survives_a_route_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path())
        .with_control_plane(control_plane.clone())
        .with_scm_store(Arc::new(MemoryScmStore::new()));
    let daemon = deps.auth_token.as_str().to_string();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, token) = bootstrap_org(&client, &base, &daemon, "Acme", "owner@acme.test", "k").await;

    // Restart the daemon surface over the SAME database: the token still
    // authenticates and the organization is intact. The first listener is
    // JOINED (bounded graceful shutdown) before the store is reopened.
    handle.shutdown().await.unwrap();
    let reopened = Arc::new(ControlPlane::new(
        Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path())
        .with_control_plane(reopened)
        .with_scm_store(Arc::new(MemoryScmStore::new()));
    // The per-start daemon token is regenerated per daemon process: read it
    // from the reopened deps (that is exactly the restart semantics).
    let daemon = deps.auth_token.as_str().to_string();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let identity_resp = client
        .get(url(&base, "/native/identity"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .send()
        .await
        .unwrap();
    let status = identity_resp.status();
    let identity: serde_json::Value = identity_resp.json().await.unwrap();
    assert_eq!(status, 200, "reopened identity: {identity}");
    assert_eq!(identity["identity"]["organization"], org);
    let members: serde_json::Value = client
        .get(url(&base, &format!("/native/orgs/{org}/members")))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(members["items"].as_array().unwrap().len(), 1);
}
