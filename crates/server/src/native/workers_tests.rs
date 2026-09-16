//! Adversarial route tests of the remote/VPC worker plane: disabled parity,
//! token mint/register/revoke over HTTP, protocol skew, strict DTOs, typed
//! superseded-lease refusals, heartbeat renewal + expiry, org isolation and
//! the worker-token authentication boundary.

use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};
use faktor_cloud::{ControlPlane, ManualClock, MemoryControlPlaneStore};
use faktor_worker::{
    JobGeneration, JobKey, JobRequirements, RequeuePolicy, WorkerPlane, WORKER_PROTOCOL_VERSION,
};

const NOW_MS: i64 = 1_700_000_000_000;

struct Harness {
    handle: ServerHandle,
    plane: Arc<WorkerPlane>,
    control_plane: Arc<ControlPlane>,
    daemon_token: String,
    clock: Arc<ManualClock>,
}

async fn harness(root: &std::path::Path, workers: bool) -> Harness {
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let plane = WorkerPlane::new(
        Arc::new(faktor_worker::MemoryWorkerStore::new()),
        clock.clone(),
    );
    let mut deps = test_deps(root).with_control_plane(control_plane.clone());
    if workers {
        deps = deps.with_workers(plane.clone());
    } else {
        assert!(deps.workers.is_none(), "workers are disabled by default");
    }
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    Harness {
        handle,
        plane,
        control_plane,
        daemon_token: token.as_str().to_string(),
        clock,
    }
}

fn bootstrap(control_plane: &ControlPlane, name: &str, email: &str) -> (String, String) {
    let result = control_plane
        .bootstrap_organization(name, email, "Owner", &format!("boot-{email}"))
        .unwrap();
    let org = result.organization.id.as_str().to_string();
    let token = result
        .token
        .map(|t| t.expose().to_string())
        .expect("first issuance presents the token");
    (org, token)
}

fn capabilities_json(org: &str, toolchains: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "os": "linux",
        "arch": "x86_64",
        "toolchains": toolchains,
        "sandbox": ["seccomp"],
        "network": "egress_restricted",
        "cpu_cores": 4,
        "memory_mb": 8192,
        "gpu": null,
        "region": "eu-west",
        "trust_domain": org,
        "protocol_version": WORKER_PROTOCOL_VERSION,
    })
}

/// Mint one token through the admin route and return its plaintext.
async fn mint_token(
    client: &reqwest::Client,
    base: &str,
    daemon_token: &str,
    control_token: &str,
) -> String {
    let resp = client
        .post(format!("{base}/native/workers/tokens"))
        .bearer_auth(daemon_token)
        .header("x-faktor-control-token", control_token)
        .json(&serde_json::json!({ "label": "test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "mint must succeed");
    let body: serde_json::Value = resp.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

async fn register(
    client: &reqwest::Client,
    base: &str,
    org: &str,
    worker_id: &str,
    token: &str,
    toolchains: &[&str],
) -> reqwest::Response {
    client
        .post(format!("{base}/native/workers/register"))
        .json(&serde_json::json!({
            "token": token,
            "worker_id": worker_id,
            "display_name": worker_id,
            "capabilities": capabilities_json(org, toolchains),
        }))
        .send()
        .await
        .unwrap()
}

fn requirements_json(org: &str) -> serde_json::Value {
    serde_json::json!({
        "os": "linux",
        "arch": "x86_64",
        "toolchains": ["rust"],
        "sandbox": ["seccomp"],
        "network": "egress_restricted",
        "min_cpu_cores": 1,
        "min_memory_mb": 1024,
        "gpu": false,
        "region": "eu-west",
        "trust_domain": org,
    })
}

/// Schedule one job directly through the plane (the daemon's scheduler does
/// this in production; the route surface only serves the worker/operator
/// halves).
fn schedule(plane: &WorkerPlane, org: &str, key: &str) -> faktor_worker::ScheduledJob {
    let requirements: JobRequirements = serde_json::from_value(requirements_json(org)).unwrap();
    plane
        .schedule_job(
            &faktor_cloud::OrganizationId::try_new(org).unwrap(),
            org,
            &JobKey::try_new(key).unwrap(),
            requirements,
            &"a".repeat(64),
            RequeuePolicy { max_attempts: 3 },
            None,
        )
        .unwrap()
}

#[tokio::test]
async fn worker_routes_are_typed_409_when_the_plane_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), false).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let (org, control_token) = bootstrap(&h.control_plane, "Acme", "owner@acme.test");
    let bodies = [
        (
            "register",
            client
                .post(format!("{base}/native/workers/register"))
                .json(&serde_json::json!({
                    "token": "wkr_x",
                    "worker_id": "wrk_1",
                    "display_name": "w",
                    "capabilities": capabilities_json(&org, &["rust"]),
                })),
        ),
        (
            "heartbeat",
            client
                .post(format!("{base}/native/workers/wrk_1/heartbeat"))
                .json(&serde_json::json!({
                    "token": "wkr_x",
                    "protocol_version": WORKER_PROTOCOL_VERSION,
                    "lease_id": "lease_1",
                    "generation": 1,
                })),
        ),
        (
            "result",
            client
                .post(format!("{base}/native/jobs/job_1/result"))
                .json(&serde_json::json!({
                    "token": "wkr_x",
                    "protocol_version": WORKER_PROTOCOL_VERSION,
                    "lease_id": "lease_1",
                    "generation": 1,
                    "digest": "a".repeat(64),
                    "outcome": "succeeded",
                })),
        ),
    ];
    for (label, req) in bodies {
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), 409, "{label}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "workers_disabled", "{label}");
    }
    // Operator routes stay behind the daemon password AND the principal.
    let resp = client
        .get(format!("{base}/native/workers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "no daemon password");
    let resp = client
        .get(format!("{base}/native/workers"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "workers_disabled");
    let resp = client
        .post(format!("{base}/native/workers/wrk_1/revoke"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let resp = client
        .get(format!("{base}/native/jobs/job_1"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let resp = client
        .post(format!("{base}/native/workers/tokens"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn register_heartbeat_result_roundtrip_and_token_reuse_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let (org, control_token) = bootstrap(&h.control_plane, "Acme", "owner@acme.test");
    let token = mint_token(&client, &base, &h.daemon_token, &control_token).await;

    let resp = register(&client, &base, &org, "wrk_1", &token, &["rust"]).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["worker"]["capabilities_version"], 1);
    assert_eq!(body["capabilitiesReconciled"], false);

    // Reusing the SAME token for a DIFFERENT worker is refused typed.
    let resp = register(&client, &base, &org, "wrk_2", &token, &["rust"]).await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "worker_token_reused");

    // Protocol skew is refused typed before any state changes.
    let mut skewed = capabilities_json(&org, &["rust"]);
    skewed["protocol_version"] = serde_json::json!(WORKER_PROTOCOL_VERSION + 1);
    let resp = client
        .post(format!("{base}/native/workers/register"))
        .json(&serde_json::json!({
            "token": token,
            "worker_id": "wrk_1",
            "display_name": "wrk_1",
            "capabilities": skewed,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "protocol_skew");

    // Capability reconciliation over HTTP: a changed toolchain bumps the
    // version exactly once.
    let resp = register(&client, &base, &org, "wrk_1", &token, &["rust", "node"]).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["worker"]["capabilities_version"], 2);
    assert_eq!(body["capabilitiesReconciled"], true);

    // One page lists the worker for the operator (WorkerRead).
    let resp = client
        .get(format!("{base}/native/workers"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(
        body["items"][0].get("token_hash").is_none(),
        "no token material leaks"
    );

    // Schedule + heartbeat + result over the wire.
    let job = schedule(&h.plane, &org, "run-1");
    let lease = job.lease.clone().expect("auto-assigned");
    let resp = client
        .post(format!("{base}/native/workers/wrk_1/heartbeat"))
        .json(&serde_json::json!({
            "token": token,
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": lease.lease_id,
            "generation": job.generation.as_u64(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["expiresAtMs"].as_i64().unwrap() > NOW_MS);

    let resp = client
        .post(format!("{base}/native/jobs/{}/result", job.job.job_id))
        .json(&serde_json::json!({
            "token": token,
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": lease.lease_id,
            "generation": job.generation.as_u64(),
            "digest": "a".repeat(64),
            "outcome": "succeeded",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["outcome"], "landed");

    // `GET /native/jobs/{id}` shows terminal state/attempts.
    let resp = client
        .get(format!("{base}/native/jobs/{}", job.job.job_id))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["job"]["state"], "completed");
    assert_eq!(body["result"]["outcome"], "succeeded");
    assert_eq!(body["generations"].as_array().unwrap().len(), 1);

    // An identical replay is a duplicate, never a second landing.
    let resp = client
        .post(format!("{base}/native/jobs/{}/result", job.job.job_id))
        .json(&serde_json::json!({
            "token": token,
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": lease.lease_id,
            "generation": job.generation.as_u64(),
            "digest": "a".repeat(64),
            "outcome": "succeeded",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["outcome"], "duplicate");
}

#[tokio::test]
async fn stale_generation_result_is_typed_409_superseded_lease() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let (org, control_token) = bootstrap(&h.control_plane, "Acme", "owner@acme.test");
    let token = mint_token(&client, &base, &h.daemon_token, &control_token).await;
    let resp = register(&client, &base, &org, "wrk_1", &token, &["rust"]).await;
    assert_eq!(resp.status(), 200);
    let job = schedule(&h.plane, &org, "stale-run");
    let lease = job.lease.clone().unwrap();
    // Advance past the heartbeat window: the lease expires and the job
    // requeues into a new generation.
    h.clock.advance(60_000);
    let report = h
        .plane
        .recover(&faktor_cloud::OrganizationId::try_new(&org).unwrap())
        .unwrap();
    assert_eq!(report.requeued.len(), 1);
    let resp = client
        .post(format!("{base}/native/jobs/{}/result", job.job.job_id))
        .json(&serde_json::json!({
            "token": token,
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": lease.lease_id,
            "generation": JobGeneration::FIRST.as_u64(),
            "digest": "a".repeat(64),
            "outcome": "succeeded",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["code"], "superseded_lease",
        "a stale generation is a TYPED refusal"
    );
    // Nothing landed and the rejection is journaled.
    let status = h
        .plane
        .job_status(
            &faktor_cloud::OrganizationId::try_new(&org).unwrap(),
            &job.job.job_id,
        )
        .unwrap();
    assert!(status.result.is_none());
    let journal = h
        .plane
        .journal_page(
            &faktor_cloud::OrganizationId::try_new(&org).unwrap(),
            None,
            100,
        )
        .unwrap();
    assert!(journal.iter().any(|e| e.kind == "stale_result_rejected"));
}

#[tokio::test]
async fn revocation_and_org_isolation_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let (org_a, control_a) = bootstrap(&h.control_plane, "Acme", "owner@acme.test");
    let (org_b, control_b) = bootstrap(&h.control_plane, "Beta", "owner@beta.test");
    let token_a = mint_token(&client, &base, &h.daemon_token, &control_a).await;
    let token_b = mint_token(&client, &base, &h.daemon_token, &control_b).await;
    assert_eq!(
        register(&client, &base, &org_a, "wrk_a", &token_a, &["rust"])
            .await
            .status(),
        200
    );
    assert_eq!(
        register(&client, &base, &org_b, "wrk_b", &token_b, &["rust"])
            .await
            .status(),
        200
    );

    // Org B sees ONLY its own worker, and org A's job is a 404 for B.
    let resp = client
        .get(format!("{base}/native/workers"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_b)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["worker_id"], "wrk_b");
    let job = schedule(&h.plane, &org_a, "iso-run");
    let resp = client
        .get(format!("{base}/native/jobs/{}", job.job.job_id))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_b)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "a foreign org job is a byte-identical 404"
    );

    // Org B cannot revoke org A's worker (404, no existence leak).
    let resp = client
        .post(format!("{base}/native/workers/wrk_a/revoke"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_b)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // The owner revokes its worker: the lease dies and the token is dead.
    let lease = job.lease.clone().unwrap();
    let resp = client
        .post(format!("{base}/native/workers/wrk_a/revoke"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &control_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["expiredLeases"].as_array().unwrap().len(), 1);
    let resp = client
        .post(format!("{base}/native/workers/wrk_a/heartbeat"))
        .json(&serde_json::json!({
            "token": token_a,
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": lease.lease_id,
            "generation": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "worker_revoked");
    // Re-registering with the dead token is refused typed.
    let resp = register(&client, &base, &org_a, "wrk_a", &token_a, &["rust"]).await;
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "worker_token_revoked");
}

#[tokio::test]
async fn strict_dtos_and_worker_token_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let (org, control_token) = bootstrap(&h.control_plane, "Acme", "owner@acme.test");
    // Unknown fields are 400s (strict DTOs).
    let resp = client
        .post(format!("{base}/native/workers/register"))
        .json(&serde_json::json!({
            "token": "wkr_x",
            "worker_id": "wrk_1",
            "display_name": "w",
            "capabilities": capabilities_json(&org, &["rust"]),
            "hostile_extra": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "malformed");
    // An unknown token never registers and never heartbeats.
    let resp = register(&client, &base, &org, "wrk_1", "wkr_deadbeef", &["rust"]).await;
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unknown_worker_token");
    // A worker token is NOT a control-plane credential: it cannot list.
    let token = mint_token(&client, &base, &h.daemon_token, &control_token).await;
    let resp = client
        .get(format!("{base}/native/workers"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "worker tokens are not principals");
    // Register, then prove the daemon password is not a worker credential.
    assert_eq!(
        register(&client, &base, &org, "wrk_1", &token, &["rust"])
            .await
            .status(),
        200
    );
    let resp = client
        .post(format!("{base}/native/workers/wrk_1/heartbeat"))
        .json(&serde_json::json!({
            "token": h.daemon_token,
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": "lease_1",
            "generation": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn operator_routes_require_a_principal_and_ids_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let (org, control_token) = bootstrap(&h.control_plane, "Acme", "owner@acme.test");
    let _ = org;
    // No control-plane principal: the operator surface answers the typed
    // unauthorized (the daemon password alone is not a principal).
    let resp = client
        .post(format!("{base}/native/workers/tokens"))
        .bearer_auth(&h.daemon_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");
    // An unknown control token is the same typed unauthorized.
    let resp = client
        .post(format!("{base}/native/workers/tokens"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", "not-a-real-token")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // The owner mints successfully and the plaintext is presented once.
    let token = mint_token(&client, &base, &h.daemon_token, &control_token).await;
    assert!(token.starts_with("wkr_"));
    // A malformed worker id is a 400, never a lookup.
    let resp = client
        .post(format!(
            "{base}/native/workers/not%20a%20valid%20id/heartbeat"
        ))
        .json(&serde_json::json!({
            "token": "wkr_x",
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "lease_id": "lease_1",
            "generation": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
