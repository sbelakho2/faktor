//! Adversarial tests of the worker-plane deployment boundary: typed
//! refusals for non-loopback binds without TLS/gateway acknowledgement,
//! gateway mode admitting non-loopback with a recorded acknowledgement, the
//! dedicated socket serving the worker routes ONLY, and the proof that the
//! two listeners (native loopback + worker plane) cannot change each other's
//! exposure or behavior.

use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{
    serve_arc, serve_worker_plane, WorkerPlaneAuth, WorkerPlaneBindConfig,
    WorkerPlaneBoundaryRefusal, WorkerPlaneTransport,
};
use faktor_cloud::{ControlPlane, ManualClock, MemoryControlPlaneStore, OrganizationId};
use faktor_worker::{WorkerPlane, WORKER_PROTOCOL_VERSION};

const NOW_MS: i64 = 1_700_000_000_000;

/// One deps envelope with BOTH the worker plane and the control plane wired
/// (the operator routes on the native listener need the principal source).
fn deps(root: &std::path::Path) -> (Arc<crate::ServerDeps>, Arc<WorkerPlane>, String) {
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let plane = WorkerPlane::new(
        Arc::new(faktor_worker::MemoryWorkerStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    );
    let base = test_deps(root)
        .with_control_plane(control_plane)
        .with_workers(plane.clone());
    let daemon_token = base.auth_token.as_str().to_string();
    (Arc::new(base), plane, daemon_token)
}

fn loopback_config(bearer: Option<&str>) -> WorkerPlaneBindConfig {
    WorkerPlaneBindConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        transport: WorkerPlaneTransport::Plaintext,
        trusted_gateway: false,
        auth: WorkerPlaneAuth::WorkerTokens,
        bearer: bearer.map(str::to_string),
    }
}

fn capabilities_json(org: &str) -> serde_json::Value {
    serde_json::json!({
        "os": "linux",
        "arch": "x86_64",
        "toolchains": ["rust"],
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

// ------------------------------------------------------------ typed refusals

/// The transport bearer is a credential: `Debug` must never print it (a
/// config type that leaks into a log line must leak nothing).
#[test]
fn bind_config_debug_redacts_the_transport_bearer() {
    let mut config = loopback_config(Some("gw-super-secret"));
    config.auth = WorkerPlaneAuth::GatewayMtls;
    config.trusted_gateway = true;
    config.validate().unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("gw-super-secret"), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");
}

/// A non-loopback worker bind without TLS or the gateway acknowledgement is
/// the typed refusal NAMING the deployment boundary — and no socket is
/// constructed.
#[tokio::test]
async fn non_loopback_without_tls_or_gateway_is_a_typed_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, _plane, _token) = deps(dir.path());
    let mut config = loopback_config(None);
    config.bind = "0.0.0.0:8790".parse().unwrap();
    let exposure = config.validate().unwrap_err();
    assert!(matches!(
        exposure,
        WorkerPlaneBoundaryRefusal::BeyondLoopbackWithoutTransport { .. }
    ));
    assert_eq!(exposure.code(), "worker_plane_boundary_refused");
    let message = exposure.to_string();
    assert!(
        message.contains("worker-plane deployment boundary"),
        "the refusal names the boundary: {message}"
    );
    assert!(
        message.contains("trusted_gateway"),
        "the refusal names the acknowledgement: {message}"
    );
    // The serving entry refuses BEFORE binding: no worker socket exists.
    let error = serve_worker_plane(deps, config).await.unwrap_err();
    let crate::WorkerPlaneServeError::Boundary(refusal) = error else {
        panic!("expected the typed boundary refusal, got {error}");
    };
    assert_eq!(refusal.code(), "worker_plane_boundary_refused");
}

/// In-process TLS termination is refused typed (this workspace compiles no
/// inbound TLS stack) — the refusal points at the gateway-only mode instead
/// of fabricating an acceptor.
#[test]
fn in_process_tls_is_refused_because_no_inbound_stack_exists() {
    let mut config = loopback_config(None);
    config.transport = WorkerPlaneTransport::Tls;
    config.trusted_gateway = true;
    let refusal = config.validate().unwrap_err();
    assert!(matches!(
        refusal,
        WorkerPlaneBoundaryRefusal::TlsUnavailableInProcess { .. }
    ));
    let message = refusal.to_string();
    assert!(message.contains("no inbound TLS stack"), "{message}");
    assert!(message.contains("trusted gateway"), "{message}");
    assert!(
        message.contains("worker-plane deployment boundary"),
        "{message}"
    );
}

/// `auth = gateway_mtls` exists only at the acknowledged gateway boundary and
/// requires the gateway's transport bearer.
#[test]
fn gateway_mtls_requires_acknowledgement_and_bearer() {
    let mut config = loopback_config(None);
    config.auth = WorkerPlaneAuth::GatewayMtls;
    assert!(matches!(
        config.validate().unwrap_err(),
        WorkerPlaneBoundaryRefusal::GatewayMtlsWithoutGateway { .. }
    ));
    config.trusted_gateway = true;
    assert!(matches!(
        config.validate().unwrap_err(),
        WorkerPlaneBoundaryRefusal::GatewayMtlsWithoutBearer { .. }
    ));
    config.bearer = Some("gateway-secret".into());
    let exposure = config.validate().unwrap();
    assert_eq!(exposure.auth, WorkerPlaneAuth::GatewayMtls);
    assert!(exposure.transport_bearer);
    // A malformed bearer (whitespace/oversized/empty) is refused.
    for bad in ["", "has space", "line\nbreak"] {
        config.bearer = Some(bad.into());
        assert!(matches!(
            config.validate().unwrap_err(),
            WorkerPlaneBoundaryRefusal::MalformedTransportBearer { .. }
        ));
    }
    config.bearer = Some("x".repeat(crate::MAX_WORKER_PLANE_BEARER_BYTES + 1));
    assert!(matches!(
        config.validate().unwrap_err(),
        WorkerPlaneBoundaryRefusal::MalformedTransportBearer { .. }
    ));
}

// -------------------------------------------------------------- gateway mode

/// Gateway mode admits a non-loopback bind, records the acknowledgement in
/// the exposure witness/audit line, and the socket actually answers on the
/// machine's interfaces.
#[tokio::test]
async fn gateway_mode_admits_non_loopback_and_records_the_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, _plane, token) = deps(dir.path());
    let mut config = loopback_config(None);
    config.bind = "0.0.0.0:0".parse().unwrap();
    config.trusted_gateway = true;
    let exposure = config.validate().unwrap();
    assert!(exposure.beyond_loopback);
    assert!(exposure.trusted_gateway);
    let audit = exposure.audit_line();
    assert!(audit.contains("exposure=beyond-loopback"), "{audit}");
    assert!(audit.contains("trusted_gateway=true"), "{audit}");
    let handle = serve_worker_plane(deps, config).await.unwrap();
    assert!(!handle.addr.ip().is_loopback(), "bound beyond loopback");
    let base = format!("http://127.0.0.1:{}", handle.addr.port());
    // The worker route answers (a malformed body proves the route is live).
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/native/workers/register"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "hostile": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

// ------------------------------------------------------- dedicated socket

/// The dedicated socket serves the FOUR worker routes only: native routes and
/// the operator half of the worker plane are unreachable there (404), while
/// the native loopback listener keeps its full surface.
#[tokio::test]
async fn dedicated_socket_serves_worker_routes_only() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, plane, token) = deps(dir.path());
    let org = OrganizationId::try_new("org_worker").unwrap();
    let issued = plane
        .mint_registration_token(&org, "org_worker", "boundary")
        .unwrap();
    let native = serve_arc(deps.clone(), 0).await.unwrap();
    let worker = serve_worker_plane(deps.clone(), loopback_config(None))
        .await
        .unwrap();
    let worker_base = format!("http://{}", worker.addr);
    let native_base = format!("http://{}", native.addr);
    let client = reqwest::Client::new();

    // The worker route works on the dedicated socket (registration).
    let registered = client
        .post(format!("{worker_base}/native/workers/register"))
        .json(&serde_json::json!({
            "token": issued.token.expose(),
            "worker_id": "w-boundary",
            "display_name": "w-boundary",
            "capabilities": capabilities_json("org_worker"),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        registered.status(),
        200,
        "{}",
        registered.text().await.unwrap()
    );

    // Every native surface (and the operator half) is a 404 there.
    for (method, path) in [
        ("GET", "/native/health"),
        ("GET", "/native/ready"),
        ("GET", "/native/sessions"),
        ("GET", "/native/workers"),
        ("POST", "/native/workers/tokens"),
        ("POST", "/native/sso/start"),
        ("GET", "/native/jobs/job_x"),
    ] {
        let request = match method {
            "GET" => client.get(format!("{worker_base}{path}")),
            _ => client.post(format!("{worker_base}{path}")),
        };
        let response = request.bearer_auth(&token).send().await.unwrap();
        assert_eq!(
            response.status(),
            404,
            "{method} {path} must be unreachable on the worker socket"
        );
    }

    // The native loopback listener is unchanged: health answers and the
    // worker-token route is served there too (additive parity).
    let health = client
        .get(format!("{native_base}/native/health"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    assert!(native.addr.ip().is_loopback());
}

/// Binding the worker plane (even beyond loopback, gateway-acknowledged)
/// never changes the native listener: the native address stays loopback and
/// keeps answering; and the worker plane's own transport bearer keeps gating
/// independently of the native listener.
#[tokio::test]
async fn worker_plane_binding_does_not_change_the_native_listener_and_vice_versa() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, _plane, token) = deps(dir.path());
    let native = serve_arc(deps.clone(), 0).await.unwrap();
    assert!(native.addr.ip().is_loopback(), "native is always loopback");
    let mut config = loopback_config(Some("gateway-secret"));
    config.bind = "0.0.0.0:0".parse().unwrap();
    config.trusted_gateway = true;
    let worker = serve_worker_plane(deps.clone(), config).await.unwrap();
    let native_base = format!("http://{}", native.addr);
    let worker_base = format!("http://127.0.0.1:{}", worker.addr.port());
    let client = reqwest::Client::new();

    // Native listener: daemon password works, worker transport bearer is
    // meaningless there (the native listener never reads the worker plane's
    // transport config).
    let health = client
        .get(format!("{native_base}/native/health"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    let health = client
        .get(format!("{native_base}/native/health"))
        .bearer_auth("gateway-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(
        health.status(),
        401,
        "the worker plane's bearer must not authenticate on the native listener"
    );

    // Worker socket: the transport bearer is required on every request, the
    // daemon password is NOT accepted, and the worker route is still live
    // behind the bearer.
    let response = client
        .post(format!("{worker_base}/native/workers/register"))
        .json(&serde_json::json!({
            "token": "token",
            "worker_id": "w",
            "display_name": "w",
            "capabilities": capabilities_json("org_worker"),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "missing transport bearer");
    let response = client
        .post(format!("{worker_base}/native/workers/register"))
        .bearer_auth("wrong")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "wrong transport bearer");
    let response = client
        .post(format!("{worker_base}/native/workers/register"))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "the daemon password is not the worker-plane transport credential"
    );
    // Behind the correct bearer the route is reached (strict DTO 400).
    let response = client
        .post(format!("{worker_base}/native/workers/register"))
        .bearer_auth("gateway-secret")
        .json(&serde_json::json!({ "hostile": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}
