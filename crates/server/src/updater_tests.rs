//! Native updater route tests: disabled parity, control-plane role gating
//! (viewer read, member stage, ADMIN apply), typed manifest refusals over
//! HTTP, strict DTOs, idempotency requirements, and the apply→rollback round
//! trip through the real route handlers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use faktor_cloud::{Action, ControlPlane, ManualClock, MemoryControlPlaneStore, Role};
use faktor_updater::{
    Artifact, Channel, Compatibility, MemoryUpdaterStore, SchemaRange, TrustedKey, TrustedKeys,
    UpdateError, UpdateManifest, UpdateOpKind, UpdateOpStatus, Updater, UpdaterConfig,
    VersionRange,
};

use super::tests::test_deps;
use super::*;

/// The control-plane's manual clock (a fixed instant for cloud rows).
const NOW_MS: i64 = 1_700_000_000_000;

/// The UPDATER checks manifests against the real clock (the route stamps
/// `SystemTime::now()`), so signed test manifests must live in a window
/// around the real now — a frozen constant would expire.
fn real_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
const OS: &str = "darwin";
const ARCH: &str = "arm64";

struct FakeFetcher {
    payloads: Mutex<HashMap<String, Vec<u8>>>,
}

impl FakeFetcher {
    fn new() -> Self {
        FakeFetcher {
            payloads: Mutex::new(HashMap::new()),
        }
    }

    fn serve(&self, url: &str, payload: &[u8]) {
        self.payloads
            .lock()
            .unwrap()
            .insert(url.to_string(), payload.to_vec());
    }
}

#[async_trait::async_trait]
impl faktor_updater::ArtifactFetcher for FakeFetcher {
    async fn fetch_to(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut (dyn tokio::io::AsyncWrite + Send + Unpin),
    ) -> Result<u64, UpdateError> {
        let payload = self
            .payloads
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| UpdateError::Transport {
                artifact: url.to_string(),
                detail: "no fake payload".into(),
            })?;
        if payload.len() as u64 > max_bytes {
            return Err(UpdateError::ArtifactTooLarge {
                artifact: url.to_string(),
                max_bytes,
            });
        }
        use tokio::io::AsyncWriteExt as _;
        sink.write_all(&payload).await.unwrap();
        Ok(payload.len() as u64)
    }
}

fn keypair() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn trusted_keys(key: &SigningKey) -> TrustedKeys {
    TrustedKeys::new(vec![TrustedKey::from_base64(
        "operator",
        &BASE64.encode(key.verifying_key().to_bytes()),
    )
    .unwrap()])
    .unwrap()
}

fn range(min: &str, max: &str) -> VersionRange {
    VersionRange::from_parts(min, max).unwrap()
}

/// The signed anti-rollback generation of a test release: monotonic in the
/// version (major*1e6 + minor*1e3 + patch), mirroring the lifecycle tests.
fn generation_of(version: &str) -> u64 {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    major * 1_000_000 + minor * 1_000 + patch
}

/// One signed manifest + the URL it points at (already served by the
/// fetcher) and its digest.
fn signed_manifest(
    key: &SigningKey,
    fetcher: &FakeFetcher,
    version: &str,
    payload: &[u8],
    overrides: impl FnOnce(&mut UpdateManifest),
) -> (serde_json::Value, String) {
    let digest = faktor_updater::manifest::sha256_hex(payload);
    let name = format!("faktor-cli-{version}-{OS}-{ARCH}.tar.gz");
    let url = format!("https://mirror.test/{version}/{name}");
    fetcher.serve(&url, payload);
    let mut manifest = UpdateManifest {
        schema: faktor_updater::UPDATE_MANIFEST_SCHEMA.to_string(),
        channel: Channel::Stable,
        version: version.to_string(),
        commit: "a".repeat(40),
        release_generation: Some(generation_of(version)),
        artifacts: vec![Artifact {
            name,
            os: OS.into(),
            arch: ARCH.into(),
            sha256: digest.clone(),
            url,
            size: Some(payload.len() as u64),
        }],
        compatibility: Compatibility {
            cli: range("*", "*"),
            daemon: range("*", "*"),
            vscode: range("*", "*"),
            jetbrains: range("*", "*"),
            schema: SchemaRange { min: 1, max: 1 },
        },
        issued_at: real_now_ms() - 1_000,
        expires_at: real_now_ms() + 600_000,
        certification: None,
        signature: None,
    };
    overrides(&mut manifest);
    let signature = key.sign(&manifest.signing_payload().unwrap());
    manifest.signature = Some(faktor_updater::manifest::ManifestSignature {
        algorithm: "ed25519".into(),
        identity: "operator".into(),
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        value: BASE64.encode(signature.to_bytes()),
    });
    (serde_json::to_value(&manifest).unwrap(), digest)
}

struct UpdaterHarness {
    handle: ServerHandle,
    daemon_token: String,
    owner_token: String,
    member_token: String,
    fetcher: Arc<FakeFetcher>,
    _dir: tempfile::TempDir,
    _updater: Arc<Updater>,
}

fn url(base: &str, path: &str) -> String {
    format!("{base}{path}")
}

/// Serve one daemon with the updater + control plane wired and two
/// principals: the owner (admin-and-above) and a member-role service account
/// with the updater read/stage scopes.
async fn updater_harness() -> UpdaterHarness {
    let dir = tempfile::tempdir().unwrap();
    let key = keypair();
    let fetcher = Arc::new(FakeFetcher::new());
    let config = UpdaterConfig {
        channel: Channel::Stable,
        install_root: dir.path().join("install"),
        keys: trusted_keys(&key),
        max_artifact_bytes: 1024 * 1024,
        clock_skew_ms: faktor_updater::DEFAULT_CLOCK_SKEW_MS,
        host_os: OS.into(),
        host_arch: ARCH.into(),
        local_version: "0.1.0".into(),
        allow_legacy_manifests_once: false,
    };
    let updater = Arc::new(
        Updater::with_default_probe(config, Arc::new(MemoryUpdaterStore::new()), fetcher.clone())
            .unwrap(),
    );
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path())
        .with_control_plane(control_plane.clone())
        .with_updater(updater.clone());
    let daemon_token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // Bootstrap an owner.
    let resp = client
        .post(url(&base, "/native/orgs"))
        .bearer_auth(daemon_token.as_str())
        .header("idempotency-key", "k-owner")
        .json(&serde_json::json!({
            "name": "Ops", "owner_email": "owner@ops.test", "display_name": "Owner",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let org = body["organization"]["id"].as_str().unwrap().to_string();
    let owner_token = body["token"].as_str().unwrap().to_string();

    // The owner mints a member-role service account with the updater
    // read/stage scopes.
    let principal = control_plane.authenticate(&owner_token).unwrap();
    let issued = control_plane
        .create_service_account(
            &principal,
            &faktor_cloud::OrganizationId::try_new(org).unwrap(),
            "ci-updater",
            Role::Member,
            vec![Action::UpdaterRead, Action::UpdaterStage],
        )
        .unwrap();
    let member_token = issued.token.unwrap().expose().to_string();

    UpdaterHarness {
        handle,
        daemon_token: daemon_token.as_str().to_string(),
        owner_token,
        member_token,
        fetcher,
        _dir: dir,
        _updater: updater,
    }
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    daemon: &str,
    control: &str,
    key: Option<&str>,
    body: serde_json::Value,
) -> reqwest::Response {
    let mut request = client
        .post(url(base, path))
        .bearer_auth(daemon)
        .header("x-faktor-control-token", control)
        .json(&body);
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn updater_disabled_answers_typed_409_and_changes_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    assert!(!crate::native::updater_enabled(&deps));
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let cases: [(&str, serde_json::Value); 4] = [
        ("GET", serde_json::Value::Null),
        ("POST", serde_json::json!({"manifest": {}})),
        ("POST", serde_json::json!({"manifest": {}})),
        ("POST", serde_json::json!({"confirm": true})),
    ];
    let paths = [
        "/native/updater/status",
        "/native/updater/check",
        "/native/updater/stage",
        "/native/updater/apply",
    ];
    for ((method, body), path) in cases.into_iter().zip(paths) {
        let request = if method == "GET" {
            client.get(url(&base, path))
        } else {
            client
                .post(url(&base, path))
                .header("idempotency-key", "k-disabled")
                .json(&body)
        };
        let resp = request
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", "tok_x")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "{method} {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "updater_disabled", "{method} {path}");
    }
    // The rest of the daemon is untouched.
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
}

#[tokio::test]
async fn updater_enabled_without_a_control_plane_refuses_with_cloud_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(FakeFetcher::new());
    let updater = Arc::new(
        Updater::with_default_probe(
            UpdaterConfig {
                channel: Channel::Stable,
                install_root: dir.path().join("install"),
                keys: TrustedKeys::empty(),
                max_artifact_bytes: 1024,
                clock_skew_ms: faktor_updater::DEFAULT_CLOCK_SKEW_MS,
                host_os: OS.into(),
                host_arch: ARCH.into(),
                local_version: "0.1.0".into(),
                allow_legacy_manifests_once: false,
            },
            Arc::new(MemoryUpdaterStore::new()),
            fetcher,
        )
        .unwrap(),
    );
    let deps = test_deps(dir.path()).with_updater(updater);
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let cases: [(&str, serde_json::Value); 4] = [
        ("GET", serde_json::Value::Null),
        ("POST", serde_json::json!({"manifest": {}})),
        ("POST", serde_json::json!({"manifest": {}})),
        ("POST", serde_json::json!({"confirm": true})),
    ];
    let paths = [
        "/native/updater/status",
        "/native/updater/check",
        "/native/updater/stage",
        "/native/updater/apply",
    ];
    for ((method, body), path) in cases.into_iter().zip(paths) {
        let request = if method == "GET" {
            client.get(url(&base, path))
        } else {
            client
                .post(url(&base, path))
                .header("idempotency-key", "k-cloud-off")
                .json(&body)
        };
        let resp = request
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", "tok_x")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "{method} {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "cloud_disabled", "{method} {path}");
    }
}

#[tokio::test]
async fn stage_needs_a_control_plane_principal_and_the_member_scope() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);
    let key = keypair();
    let (manifest, _digest) = signed_manifest(&key, &host.fetcher, "0.2.0", b"payload", |_| {});

    // No control-plane token: 401.
    let resp = client
        .get(url(&base, "/native/updater/status"))
        .bearer_auth(&host.daemon_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // The daemon password is not a control-plane credential.
    let resp = client
        .get(url(&base, "/native/updater/status"))
        .bearer_auth(&host.daemon_token)
        .header("x-faktor-control-token", &host.daemon_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // The member role may read the status (updater.read is a viewer action).
    let resp = client
        .get(url(&base, "/native/updater/status"))
        .bearer_auth(&host.daemon_token)
        .header("x-faktor-control-token", &host.member_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["status"]["channel"], "stable");
    assert!(status["status"]["installed"].is_null());

    // The member role may stage (updater.stage is a member action), but the
    // route demands an idempotency key.
    let resp = post(
        &client,
        &base,
        "/native/updater/stage",
        &host.daemon_token,
        &host.member_token,
        None,
        serde_json::json!({ "manifest": manifest }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "malformed");

    let resp = post(
        &client,
        &base,
        "/native/updater/stage",
        &host.daemon_token,
        &host.member_token,
        Some("k-stage-1"),
        serde_json::json!({ "manifest": manifest }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["stage"]["version"], "0.2.0");

    // Apply is ADMIN-gated: the member service account is refused with a
    // typed permission denial, and nothing was swapped.
    let resp = post(
        &client,
        &base,
        "/native/updater/apply",
        &host.daemon_token,
        &host.member_token,
        Some("k-apply-member"),
        serde_json::json!({ "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "permission_denied");
    let pointer_path = host._updater.layout().pointer_path();
    assert!(!pointer_path.exists(), "a refused apply never swaps");

    // The owner (owner > admin) applies it.
    let resp = post(
        &client,
        &base,
        "/native/updater/apply",
        &host.daemon_token,
        &host.owner_token,
        Some("k-apply-owner"),
        serde_json::json!({ "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["apply"]["result"], "applied");
    assert!(pointer_path.exists());

    // A second apply of the same staged artifact is refused.
    let resp = post(
        &client,
        &base,
        "/native/updater/apply",
        &host.daemon_token,
        &host.owner_token,
        Some("k-apply-owner-2"),
        serde_json::json!({ "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn apply_requires_explicit_confirmation_and_a_strict_body() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);

    let resp = post(
        &client,
        &base,
        "/native/updater/apply",
        &host.daemon_token,
        &host.owner_token,
        Some("k-confirm"),
        serde_json::json!({ "confirm": false }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "confirmation_required");

    let resp = post(
        &client,
        &base,
        "/native/updater/apply",
        &host.daemon_token,
        &host.owner_token,
        Some("k-unknown-field"),
        serde_json::json!({ "confirm": true, "surprise": 1 }),
    )
    .await;
    assert_eq!(resp.status(), 400);

    // compatibility data cannot be smuggled past `stage`.
    let resp = post(
        &client,
        &base,
        "/native/updater/apply",
        &host.daemon_token,
        &host.owner_token,
        Some("k-components"),
        serde_json::json!({ "confirm": true, "components": { "cli": "0.1.0" } }),
    )
    .await;
    assert_eq!(resp.status(), 400);

    // Rollback is admin-gated too and refuses politely with nothing to do.
    let resp = post(
        &client,
        &base,
        "/native/updater/rollback",
        &host.daemon_token,
        &host.owner_token,
        Some("k-rollback-empty"),
        serde_json::json!({ "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn manifest_refusals_are_typed_over_http_and_never_download() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);
    let key = keypair();
    let (manifest, _digest) = signed_manifest(&key, &host.fetcher, "0.2.0", b"payload", |_| {});

    // (a) unsigned.
    let mut unsigned = manifest.clone();
    unsigned.as_object_mut().unwrap().remove("signature");
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": unsigned }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "manifest_unsigned");

    // (b) unknown identity.
    let mut unknown = manifest.clone();
    unknown["signature"]["identity"] = serde_json::json!("stranger");
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": unknown }),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "manifest_unknown_key");

    // (c) tampered after signing.
    let mut tampered = manifest.clone();
    tampered["version"] = serde_json::json!("9.9.9");
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": tampered }),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "manifest_tampered");

    // (d) expired (authentic signature).
    let (expired, _) = signed_manifest(&key, &host.fetcher, "0.5.0", b"expired", |m| {
        m.issued_at = real_now_ms() - 900_000;
        m.expires_at = real_now_ms() - 800_000;
    });
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": expired }),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "manifest_expired");

    // (e) incompatible: the refusal names each refusing component.
    let (incompatible, _) = signed_manifest(&key, &host.fetcher, "0.6.0", b"incompatible", |m| {
        m.compatibility.cli = VersionRange::from_parts("9.0.0", "9.9.9").unwrap();
        m.compatibility.daemon = VersionRange::from_parts("9.0.0", "9.9.9").unwrap();
    });
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": incompatible }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "incompatible");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("cli"), "{message}");
    assert!(message.contains("daemon"), "{message}");

    // (f) channel pin: a beta manifest on the stable channel.
    let (beta, _) = signed_manifest(&key, &host.fetcher, "0.7.0", b"beta", |m| {
        m.channel = Channel::Beta;
    });
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": beta }),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "manifest_channel_mismatch");
}

#[tokio::test]
async fn a_digest_mismatch_over_http_is_refused_and_the_install_is_untouched() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);
    let key = keypair();
    let (manifest, digest) = signed_manifest(&key, &host.fetcher, "0.2.0", b"payload", |_| {});
    // Serve DIFFERENT bytes than the signed digest: overwrite the URL payload.
    host.fetcher.serve(
        &format!("https://mirror.test/0.2.0/faktor-cli-0.2.0-{OS}-{ARCH}.tar.gz"),
        b"evil",
    );
    let resp = post(
        &client,
        &base,
        "/native/updater/stage",
        &host.daemon_token,
        &host.member_token,
        Some("k-bad-digest"),
        serde_json::json!({ "manifest": manifest }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "digest_mismatch");
    assert!(!host
        ._updater
        .layout()
        .artifact_path(&format!("faktor-cli-0.2.0-{OS}-{ARCH}.tar.gz"), &digest)
        .exists());
    assert!(!host._updater.layout().pointer_path().exists());
}

#[tokio::test]
async fn an_apply_probe_failure_rolls_back_over_http_and_rollback_is_admin_gated() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);
    let key = keypair();

    // v1 applies cleanly.
    let (v1, v1_digest) = signed_manifest(&key, &host.fetcher, "0.1.0", b"v1", |_| {});
    for (path, key_id, body) in [
        (
            "/native/updater/stage",
            "k-v1-stage",
            serde_json::json!({ "manifest": v1 }),
        ),
        (
            "/native/updater/apply",
            "k-v1-apply",
            serde_json::json!({ "confirm": true }),
        ),
    ] {
        let resp = post(
            &client,
            &base,
            path,
            &host.daemon_token,
            &host.owner_token,
            Some(key_id),
            body,
        )
        .await;
        assert_eq!(resp.status(), 200, "{path}");
    }
    assert_eq!(
        host._updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v1_digest
    );

    // A member cannot roll back.
    let resp = post(
        &client,
        &base,
        "/native/updater/rollback",
        &host.daemon_token,
        &host.member_token,
        Some("k-rb-member"),
        serde_json::json!({ "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), 403);

    // v2 then an explicit owner rollback returns to v1 exactly.
    let (v2, _v2_digest) = signed_manifest(&key, &host.fetcher, "0.2.0", b"v2", |_| {});
    for (path, key_id, body) in [
        (
            "/native/updater/stage",
            "k-v2-stage",
            serde_json::json!({ "manifest": v2 }),
        ),
        (
            "/native/updater/apply",
            "k-v2-apply",
            serde_json::json!({ "confirm": true }),
        ),
    ] {
        let resp = post(
            &client,
            &base,
            path,
            &host.daemon_token,
            &host.owner_token,
            Some(key_id),
            body,
        )
        .await;
        assert_eq!(resp.status(), 200, "{path}");
    }
    let resp = post(
        &client,
        &base,
        "/native/updater/rollback",
        &host.daemon_token,
        &host.owner_token,
        Some("k-rb-owner"),
        serde_json::json!({ "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["apply"]["result"], "rolled_back");
    let pointer = host._updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, v1_digest);
    assert_eq!(pointer.version, "0.1.0");
}

/// Stage + apply one release over the real routes and return its digest.
async fn install_version(
    host: &UpdaterHarness,
    client: &reqwest::Client,
    version: &str,
    payload: &[u8],
    key: &SigningKey,
) -> String {
    let base = format!("http://{}", host.handle.addr);
    let (manifest, digest) = signed_manifest(key, &host.fetcher, version, payload, |_| {});
    for (path, key_id, body) in [
        (
            "/native/updater/stage",
            format!("k-stage-{version}"),
            serde_json::json!({ "manifest": manifest }),
        ),
        (
            "/native/updater/apply",
            format!("k-apply-{version}"),
            serde_json::json!({ "confirm": true }),
        ),
    ] {
        let resp = post(
            client,
            &base,
            path,
            &host.daemon_token,
            &host.owner_token,
            Some(&key_id),
            body,
        )
        .await;
        assert_eq!(resp.status(), 200, "{path}");
    }
    digest
}

#[tokio::test]
async fn an_older_signed_manifest_is_refused_typed_over_http_and_never_stages() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);
    let key = keypair();

    // Install generation 2000 (v2).
    let v2_digest = install_version(&host, &client, "2.0.0", b"v2", &key).await;
    let status: serde_json::Value = client
        .get(url(&base, "/native/updater/status"))
        .bearer_auth(&host.daemon_token)
        .header("x-faktor-control-token", &host.owner_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"]["high_water"]["generation"], 2_000_000);

    // Replaying the older signed generation 1000 (v1) is refused by BOTH
    // check and stage with the typed rollback refusal naming both sides.
    let (v1, v1_digest) = signed_manifest(&key, &host.fetcher, "1.0.0", b"v1", |m| {
        m.release_generation = Some(1_000_000);
    });
    for (path, key_id, body) in [
        (
            "/native/updater/check",
            None,
            serde_json::json!({ "manifest": v1 }),
        ),
        (
            "/native/updater/stage",
            Some("k-old-stage"),
            serde_json::json!({ "manifest": v1 }),
        ),
    ] {
        let resp = post(
            &client,
            &base,
            path,
            &host.daemon_token,
            &host.owner_token,
            key_id,
            body,
        )
        .await;
        assert_eq!(resp.status(), 409, "{path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "rollback_refused", "{path}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("1000000"), "{message}");
        assert!(message.contains("2000000"), "{message}");
    }
    // No v1 artifact was published and the install still points at v2.
    assert!(!host
        ._updater
        .layout()
        .artifact_path("faktor-cli-1.0.0-darwin-arm64.tar.gz", &v1_digest)
        .exists());
    assert_eq!(
        host._updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v2_digest
    );
    // The refusal is durable evidence: a failed check row exists, no stage
    // row was recorded for it.
    let operations = host._updater.store().list(64).unwrap();
    assert!(operations
        .iter()
        .any(|op| op.kind == UpdateOpKind::Check && op.status == UpdateOpStatus::Failed));
    assert!(!operations
        .iter()
        .any(|op| op.kind == UpdateOpKind::Stage && op.after_version.as_deref() == Some("1.0.0")));
}

#[tokio::test]
async fn the_authorized_downgrade_is_admin_gated_audited_and_resets_the_floor() {
    let host = updater_harness().await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", host.handle.addr);
    let key = keypair();

    // Install generation 2000 (v2).
    let v2_digest = install_version(&host, &client, "2.0.0", b"v2", &key).await;
    // The older target carries its own (lower) signed generation.
    let (v1, v1_digest) = signed_manifest(&key, &host.fetcher, "1.0.0", b"v1", |m| {
        m.release_generation = Some(1_000_000);
    });

    // A member can never bypass the floor.
    let resp = post(
        &client,
        &base,
        "/native/updater/downgrade",
        &host.daemon_token,
        &host.member_token,
        Some("k-dg-member"),
        serde_json::json!({ "confirm": true, "manifest": v1 }),
    )
    .await;
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "permission_denied");

    // Even the owner needs explicit confirmation.
    let resp = post(
        &client,
        &base,
        "/native/updater/downgrade",
        &host.daemon_token,
        &host.owner_token,
        Some("k-dg-noconfirm"),
        serde_json::json!({ "confirm": false, "manifest": v1 }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "confirmation_required");

    // The authorized downgrade succeeds and swaps the pointer back.
    let resp = post(
        &client,
        &base,
        "/native/updater/downgrade",
        &host.daemon_token,
        &host.owner_token,
        Some("k-dg-owner"),
        serde_json::json!({ "confirm": true, "manifest": v1 }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["apply"]["result"], "applied");
    assert_eq!(body["apply"]["digest"], v1_digest);
    let pointer = host._updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, v1_digest);
    assert_eq!(pointer.version, "1.0.0");

    // The durable floor is reset to the downgraded generation.
    let status: serde_json::Value = client
        .get(url(&base, "/native/updater/status"))
        .bearer_auth(&host.daemon_token)
        .header("x-faktor-control-token", &host.owner_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"]["high_water"]["generation"], 1_000_000);

    // The audit row: a durable `downgrade` operation with the authorizing
    // actor, both generations' evidence and the signed generation.
    let operations = host._updater.store().list(64).unwrap();
    let audit = operations
        .iter()
        .find(|op| op.kind == UpdateOpKind::Downgrade && op.status == UpdateOpStatus::Applied)
        .expect("the authorized downgrade must be recorded as a durable audit row");
    assert!(
        audit
            .actor
            .as_deref()
            .is_some_and(|a| a.starts_with("user:")),
        "{:?}",
        audit.actor
    );
    assert_eq!(audit.release_generation, Some(1_000_000));
    assert_eq!(audit.before_digest.as_deref(), Some(v2_digest.as_str()));
    assert_eq!(audit.after_digest.as_deref(), Some(v1_digest.as_str()));
    assert_eq!(audit.idempotency_key.as_deref(), Some("k-dg-owner"));

    // Now a still-older manifest is refused against the NEW floor.
    let (v0, _) = signed_manifest(&key, &host.fetcher, "0.9.0", b"v0", |m| {
        m.release_generation = Some(900);
    });
    let resp = post(
        &client,
        &base,
        "/native/updater/check",
        &host.daemon_token,
        &host.owner_token,
        None,
        serde_json::json!({ "manifest": v0 }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "rollback_refused");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("900"), "{message}");
    assert!(message.contains("1000000"), "{message}");

    // A legacy downgrade target (no signed generation) is refused outright:
    // an unauthenticated generation can never bypass the floor.
    let (legacy, _) = signed_manifest(&key, &host.fetcher, "0.1.0", b"legacy", |m| {
        m.release_generation = None;
    });
    let resp = post(
        &client,
        &base,
        "/native/updater/downgrade",
        &host.daemon_token,
        &host.owner_token,
        Some("k-dg-legacy"),
        serde_json::json!({ "confirm": true, "manifest": legacy }),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "legacy_manifest_refused");
    assert_eq!(
        host._updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v1_digest
    );
}
