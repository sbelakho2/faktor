//! Adversarial control-plane / tenant-isolation tests (task category 9):
//! cross-org ids, forged/stale tokens, pagination isolation and hostile
//! principal encodings.

use super::tests::{test_deps, wait_until_server_dead};
use super::*;
use faktor_cloud::{ControlPlane, ManualClock, MemoryControlPlaneStore};
use faktor_scm::{MemoryScmStore, RepositoryRow, ScmStore};

const NOW_MS: i64 = 1_700_000_000_000;

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

async fn bootstrap_org(
    client: &reqwest::Client,
    base: &str,
    daemon_token: &str,
    name: &str,
    email: &str,
    key: &str,
) -> (String, String) {
    let resp = client
        .post(format!("{base}/native/orgs"))
        .bearer_auth(daemon_token)
        .header("idempotency-key", key)
        .json(&serde_json::json!({
            "name": name, "owner_email": email, "display_name": "Owner",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "bootstrap {name}");
    let body: serde_json::Value = resp.json().await.unwrap();
    (
        body["organization"]["id"].as_str().unwrap().to_string(),
        body["token"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn forged_and_stale_principals_are_401_without_tenant_leak() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org_a, token_a) =
        bootstrap_org(&client, &base, &daemon, "Alpha", "a@alpha.test", "k-a").await;

    // A token from a DIFFERENT control plane (its own store) never crosses.
    let other_cp = ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    );
    let other = other_cp
        .bootstrap_organization("Foreign", "f@foreign.test", "Owner", "k-foreign")
        .expect("foreign bootstrap");
    let foreign_token = other.token.expect("foreign token").expose().to_string();

    let forged: Vec<(String, &str)> = vec![
        (String::new(), "empty token"),
        ("x".into(), "one char"),
        ("e".repeat(64), "64 hex chars"),
        ("not-a-token".into(), "free text"),
        (" ".into(), "space"),
        (format!("Bearer {token_a}"), "scheme prefix in x-header"),
        (format!("{token_a}a"), "suffixed"),
        (token_a[..token_a.len() - 1].to_string(), "truncated"),
        (token_a.to_uppercase(), "case-changed"),
        (foreign_token.clone(), "foreign control plane token"),
    ];
    for (token, label) in forged {
        let response = client
            .get(format!("{base}/native/identity"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            401,
            "forged principal ({label}) must be 401"
        );
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let rendered = body.to_string();
        assert!(
            !rendered.contains("Alpha"),
            "forged principal ({label}) leaks tenant names: {rendered}"
        );
    }

    // A control-plane token in the Authorization slot is NOT the daemon
    // credential: the password gate rejects it.
    let response = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "the control token is not a daemon credential"
    );

    // The daemon credential alone is not a control-plane principal.
    let response = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&daemon)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "daemon auth alone is not a control principal"
    );

    // Oversized principals are refused before lookup.
    for size in [5000, 8 * 1024] {
        let response = client
            .get(format!("{base}/native/identity"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", "x".repeat(size))
            .send()
            .await
            .unwrap();
        assert!(
            response.status() == 401 || response.status() == 431,
            "oversized principal ({size} bytes) is refused, got {}",
            response.status()
        );
    }

    // Duplicate principal headers: two wrong values never authenticate.
    let response = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", "wrong-1")
        .header("x-faktor-control-token", "wrong-2")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "duplicate wrong principals must be 401"
    );

    // The real token still works after the hostile matrix.
    let response = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "the real principal survives");
    let identity: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        identity["identity"]["organization"], org_a,
        "the real principal stays bound to its org"
    );
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn cross_org_ids_are_nonexistent_not_forbidden_oracles() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (_org_a, token_a) =
        bootstrap_org(&client, &base, &daemon, "Alpha", "a@alpha.test", "k-a").await;
    let (org_b, token_b) =
        bootstrap_org(&client, &base, &daemon, "Beta", "b@beta.test", "k-b").await;

    // Every org-scoped read under A's token against B's id is the SAME
    // not_found a random id yields (no existence oracle).
    let mut baseline: Option<String> = None;
    for foreign in [
        org_b.clone(),
        format!("{org_b}x"),
        format!("{org_b}/../{org_b}"),
        "org_00000000000000000000000000000000".to_string(),
        "org_".to_string(),
        "org".to_string(),
        org_b.to_uppercase(),
    ] {
        let response = client
            .get(format!("{base}/native/orgs/{foreign}/members"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_a)
            .send()
            .await
            .unwrap();
        assert!(
            response.status() == 404 || response.status() == 400,
            "foreign org {foreign:?} must be a typed refusal, got {}",
            response.status()
        );
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let rendered = body.to_string();
        assert!(
            !rendered.contains("Beta") && !rendered.contains("b@beta"),
            "foreign org {foreign:?} leaks tenant data: {rendered}"
        );
        if let Some(previous) = &baseline {
            assert_eq!(
                &rendered, previous,
                "foreign org {foreign:?} is byte-identical to the baseline refusal"
            );
        } else {
            baseline = Some(rendered);
        }
    }

    // An over-long org id is refused as malformed input BEFORE existence
    // (the input bound is not an oracle either), with no tenant bytes.
    let oversized = client
        .get(format!("{base}/native/orgs/{}/members", "x".repeat(200)))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), 400, "over-long org id is malformed");
    let rendered = oversized.text().await.unwrap_or_default();
    assert!(
        !rendered.contains("Beta"),
        "over-long org refusal leaks: {rendered}"
    );

    // A foreign invitation write is refused identically.
    let response = client
        .post(format!("{base}/native/orgs/{org_b}/members"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .header("idempotency-key", "k-cross")
        .json(&serde_json::json!({"email": "intruder@x.test", "role": "owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        404,
        "a foreign invitation must be the same not_found"
    );

    // Approvals are principal-scoped: B creates one; A never sees it and
    // cannot decide it.
    let approval = client
        .post(format!("{base}/native/approvals"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_b)
        .header("idempotency-key", "k-b-approval")
        .json(&serde_json::json!({
            "action": "secret_write", "resource": "secret:beta", "reason": "beta only",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(approval.status(), 201, "B creates its approval");
    let approval_body: serde_json::Value = approval.json().await.unwrap();
    let approval_id = approval_body["approval"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let listed_a: serde_json::Value = client
        .get(format!("{base}/native/approvals"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed_a["items"].as_array().unwrap().is_empty(),
        "A's approval page must not contain B's row: {listed_a}"
    );
    let decide = client
        .post(format!("{base}/native/approvals/{approval_id}/decide"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .header("idempotency-key", "k-cross-decide")
        .json(&serde_json::json!({"approved": true, "note": "cross"}))
        .send()
        .await
        .unwrap();
    assert!(
        decide.status() == 404 || decide.status() == 403,
        "A must not decide B's approval, got {}",
        decide.status()
    );

    // An SCM webhook cannot name a foreign organization through an unknown
    // query parameter.
    let response = client
        .get(format!("{base}/native/repositories?organization={org_b}"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        400,
        "repositories ignore no tenant parameter; unknown fields are strict"
    );
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn pagination_cursors_are_tenant_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org_a, token_a) =
        bootstrap_org(&client, &base, &daemon, "Alpha", "a@alpha.test", "k-a").await;
    let (org_b, token_b) =
        bootstrap_org(&client, &base, &daemon, "Beta", "b@beta.test", "k-b").await;

    for index in 0..5 {
        scm.upsert_repository(&RepositoryRow {
            id: 0,
            installation_id: 7,
            organization_id: org_a.clone(),
            owner: "acme".into(),
            name: format!("alpha-{index}"),
            full_name: format!("acme/alpha-{index}"),
            default_branch: "main".into(),
            private: true,
            archived: false,
            updated_ms: NOW_MS + index,
        })
        .unwrap();
        scm.upsert_repository(&RepositoryRow {
            id: 0,
            installation_id: 8,
            organization_id: org_b.clone(),
            owner: "beta".into(),
            name: format!("secret-{index}"),
            full_name: format!("beta/secret-{index}"),
            default_branch: "main".into(),
            private: true,
            archived: false,
            updated_ms: NOW_MS + index,
        })
        .unwrap();
    }

    // Full walk under A sees exactly A's five rows and never B's.
    let mut cursor: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    for _page in 0..10 {
        let query = match &cursor {
            Some(c) => format!("?limit=2&cursor={c}"),
            None => "?limit=2".to_string(),
        };
        let response = client
            .get(format!("{base}/native/repositories{query}"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_a)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "page under A: {query}");
        let body: serde_json::Value = response.json().await.unwrap();
        for item in body["items"].as_array().unwrap() {
            let full = item["fullName"].as_str().unwrap().to_string();
            assert!(
                full.starts_with("acme/"),
                "A's page contains a foreign row: {full}"
            );
            seen.push(full);
        }
        match body["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5, "A's full walk sees its five rows: {seen:?}");
    assert!(
        seen.iter().all(|name| name.contains("alpha-")),
        "A never sees a beta row: {seen:?}"
    );

    // A's cursor replayed under B's token yields B rows (or a typed 400),
    // never A rows.
    let page1: serde_json::Value = client
        .get(format!("{base}/native/repositories?limit=2"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cursor_a = page1["nextCursor"].as_str().unwrap().to_string();
    let crossed = client
        .get(format!(
            "{base}/native/repositories?limit=2&cursor={cursor_a}"
        ))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_b)
        .send()
        .await
        .unwrap();
    if crossed.status() == 200 {
        let body: serde_json::Value = crossed.json().await.unwrap();
        let rendered = body.to_string();
        assert!(
            !rendered.contains("alpha-"),
            "a stolen cursor must not resurface A's rows: {rendered}"
        );
    } else {
        assert_eq!(
            crossed.status(),
            400,
            "a stolen cursor is a typed 400, got {}",
            crossed.status()
        );
    }

    // Hostile cursor values are typed 400s (never a cross-tenant read).
    for cursor in [
        "not-a-number".to_string(),
        "-1".to_string(),
        "1.5".to_string(),
        "0x10".to_string(),
        "9".repeat(64),
        "%00".to_string(),
        "é".to_string(),
        "18446744073709551616".to_string(),
    ] {
        let response = client
            .get(format!("{base}/native/repositories?cursor={cursor}"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_b)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            400,
            "hostile cursor {cursor:?} must be a typed 400"
        );
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        assert!(
            !body.to_string().contains("alpha-"),
            "hostile cursor {cursor:?} must not leak: {body}"
        );
    }
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn idempotency_keys_and_hostile_reads_stay_tenant_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _scm, _cp, daemon) = cloud_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org_a, token_a) =
        bootstrap_org(&client, &base, &daemon, "Alpha", "a@alpha.test", "k-a").await;
    let (org_b, token_b) =
        bootstrap_org(&client, &base, &daemon, "Beta", "b@beta.test", "k-b").await;

    // The SAME idempotency key under two tenants must not collide.
    for (token, resource) in [(&token_a, "alpha"), (&token_b, "beta")] {
        let response = client
            .post(format!("{base}/native/approvals"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", token)
            .header("idempotency-key", "shared-key")
            .json(&serde_json::json!({
                "action": "secret_write", "resource": format!("secret:{resource}"), "reason": "shared",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            201,
            "shared key must be tenant-scoped ({resource})"
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body["approval"]["resource"],
            format!("secret:{resource}"),
            "the tenant gets its OWN approval"
        );
    }

    // Hostile organization ids in reads are refused without touching the
    // store in a distinguishable way.
    for hostile in [
        "org_1%00",
        "org_1%2f..%2f..",
        "org_1?extra=1",
        "org_1#frag",
        "org_1%20",
        "org_\u{0}",
        "😀",
    ] {
        let encoded: String = hostile.bytes().map(|b| format!("%{b:02X}")).collect();
        let mut candidates = vec![encoded];
        if hostile
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '\u{0}')
        {
            candidates.push(hostile.to_string());
        }
        for candidate in candidates {
            let response = client
                .get(format!("{base}/native/orgs/{candidate}/members"))
                .bearer_auth(&daemon)
                .header("x-faktor-control-token", &token_a)
                .send()
                .await
                .unwrap();
            assert!(
                response.status() == 404 || response.status() == 400,
                "hostile org {candidate:?} must be refused, got {}",
                response.status()
            );
        }
    }

    // Identity never accepts tenant selectors: any query string is inert
    // and the answer stays the token's own tenant.
    for query in ["?org=foreign", "?organization=foreign", "?tenant=foreign"] {
        let response = client
            .get(format!("{base}/native/identity{query}"))
            .bearer_auth(&daemon)
            .header("x-faktor-control-token", &token_a)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "identity query {query} is inert, not a tenant selector"
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body["identity"]["organization"], org_a,
            "identity query {query} cannot redirect the tenant"
        );
        assert!(
            !body.to_string().contains(&org_b),
            "identity query {query} leaks the foreign org"
        );
    }
    let identity: serde_json::Value = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&daemon)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        identity["identity"]["organization"], org_a,
        "identity is bound to the token tenant"
    );
    assert_ne!(org_a, org_b, "fixture orgs differ");
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}
