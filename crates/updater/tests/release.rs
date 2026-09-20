//! Adversarial tests for the immutable-release activation state machine:
//! stage → activate → restart → digest observation → health → finalize, the
//! digest-mismatch/unsigned launch refusals, the exact previous-release
//! rollback, and the crash seams between every step.
//!
//! The harness injects a fake transport, a scripted health probe and a
//! scripted supervisor restarter; no network and no real supervisor are
//! touched. Every test that activates a release materializes real bytes
//! (the running test binary is the bootstrap launcher source).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use faktor_updater::release::{TrustFile, TRUST_FILE_SCHEMA};
use faktor_updater::{
    manifest, resolve_launch, ApplyOutcome, Artifact, Channel, Compatibility, InstallLayout,
    InstallPointer, LaunchInputs, MemoryUpdaterStore, ReleaseOutcome, ReleaseRestarter,
    RunningComponents, TrustedKey, TrustedKeys, UpdateError, UpdateOpKind, UpdateOpStatus, Updater,
    UpdaterConfig, UpdaterStore, VersionRange,
};

// ------------------------------------------------------------- test harness

const OS: &str = "darwin";
const ARCH: &str = "arm64";
const NOW: i64 = 1_700_000_000_000;

struct FakeFetcher {
    payloads: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeFetcher {
    fn new() -> Self {
        FakeFetcher {
            payloads: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
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
        self.calls.lock().unwrap().push(url.to_string());
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

#[derive(Default)]
struct ScriptedProbe {
    results: Mutex<VecDeque<Result<(), String>>>,
}

impl ScriptedProbe {
    fn scripted(results: impl IntoIterator<Item = Result<(), String>>) -> Self {
        ScriptedProbe {
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl faktor_updater::HealthProbe for ScriptedProbe {
    fn probe(&self, _layout: &InstallLayout, _pointer: &InstallPointer) -> Result<(), UpdateError> {
        // An exhausted script passes: the tests queue exactly the probes
        // each path consumes, so exhaustion means "no failure was scripted".
        match self.results.lock().unwrap().pop_front() {
            Some(Err(detail)) => Err(UpdateError::HealthFailed { detail }),
            Some(Ok(())) | None => Ok(()),
        }
    }
}

/// One scripted restart result: the digest the restarted process reports.
#[derive(Clone)]
enum RestartStep {
    Reports(String),
    Fails(String),
}

#[derive(Default)]
struct ScriptedRestarter {
    steps: Mutex<VecDeque<RestartStep>>,
    /// One entry per restart call: the digest the pointer named when the
    /// restart was requested.
    calls: Mutex<Vec<String>>,
}

impl ScriptedRestarter {
    fn scripted(steps: impl IntoIterator<Item = RestartStep>) -> Self {
        ScriptedRestarter {
            steps: Mutex::new(steps.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl ReleaseRestarter for ScriptedRestarter {
    fn restart(
        &self,
        _layout: &InstallLayout,
        active: &InstallPointer,
    ) -> Result<String, UpdateError> {
        self.calls.lock().unwrap().push(active.digest.clone());
        match self.steps.lock().unwrap().pop_front() {
            Some(RestartStep::Reports(digest)) => Ok(digest),
            Some(RestartStep::Fails(detail)) => Err(UpdateError::RestartFailed { detail }),
            // An exhausted script keeps reporting the pointer it was asked
            // to restart (the supervisor's honest happy path).
            None => Ok(active.digest.clone()),
        }
    }
}

fn keypair() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn trusted_keys(key: &SigningKey) -> TrustedKeys {
    let trusted =
        TrustedKey::from_base64("operator", &BASE64.encode(key.verifying_key().to_bytes()))
            .unwrap();
    TrustedKeys::new(vec![trusted]).unwrap()
}

fn range(min: &str, max: &str) -> VersionRange {
    VersionRange::from_parts(min, max).unwrap()
}

fn compatibility() -> Compatibility {
    Compatibility {
        cli: range("0.1.0", "9.9.9"),
        daemon: range("0.1.0", "9.9.9"),
        vscode: range("*", "*"),
        jetbrains: range("*", "*"),
        schema: faktor_updater::SchemaRange { min: 1, max: 1 },
    }
}

fn components() -> RunningComponents {
    RunningComponents::from_strs(Some("0.1.0"), Some("0.1.0"), None, None, Some(1)).unwrap()
}

/// One signed manifest for arbitrary artifact bytes.
fn signed_manifest(
    key: &SigningKey,
    version: &str,
    generation: u64,
    payload: &[u8],
) -> (Vec<u8>, String) {
    let digest = manifest::sha256_hex(payload);
    let name = format!("faktor-cli-{version}-{OS}-{ARCH}");
    let url = format!("https://mirror.test/{version}/{name}");
    let mut doc = manifest::UpdateManifest {
        schema: manifest::UPDATE_MANIFEST_SCHEMA.to_string(),
        channel: Channel::Stable,
        version: version.to_string(),
        commit: format!("{version:0>40}").replace('.', "a"),
        release_generation: Some(generation),
        artifacts: vec![Artifact {
            name,
            os: OS.into(),
            arch: ARCH.into(),
            sha256: digest.clone(),
            url,
            size: Some(payload.len() as u64),
        }],
        compatibility: compatibility(),
        issued_at: NOW - 1_000,
        expires_at: NOW + 600_000,
        certification: None,
        signature: None,
    };
    let signing_payload = doc.signing_payload().unwrap();
    let signature = key.sign(&signing_payload);
    doc.signature = Some(manifest::ManifestSignature {
        algorithm: "ed25519".into(),
        identity: "operator".into(),
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        value: BASE64.encode(signature.to_bytes()),
    });
    (serde_json::to_vec(&doc).unwrap(), digest)
}

struct Host {
    _dir: tempfile::TempDir,
    updater: Arc<Updater>,
    fetcher: Arc<FakeFetcher>,
    store: Arc<MemoryUpdaterStore>,
    root: std::path::PathBuf,
    key: SigningKey,
    /// Strictly increasing clock: staged rows are ordered by their recorded
    /// timestamps, so the helper always stages "newer" operations.
    clock: std::sync::atomic::AtomicI64,
}

fn fixture(probe_results: impl IntoIterator<Item = Result<(), String>>) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("install");
    let fetcher = Arc::new(FakeFetcher::new());
    let store = Arc::new(MemoryUpdaterStore::new());
    let probe = Arc::new(ScriptedProbe::scripted(probe_results));
    let key = keypair();
    let updater = Arc::new(
        Updater::new(
            UpdaterConfig {
                channel: Channel::Stable,
                install_root: root.clone(),
                keys: trusted_keys(&key),
                max_artifact_bytes: 1024 * 1024,
                clock_skew_ms: manifest::DEFAULT_CLOCK_SKEW_MS,
                host_os: OS.into(),
                host_arch: ARCH.into(),
                local_version: "0.1.0".into(),
                allow_legacy_manifests_once: false,
            },
            store.clone(),
            fetcher.clone(),
            probe,
        )
        .unwrap(),
    );
    Host {
        _dir: dir,
        updater,
        fetcher,
        store,
        root,
        key,
        clock: std::sync::atomic::AtomicI64::new(NOW),
    }
}

impl Host {
    /// A strictly increasing timestamp for staged/activated rows.
    fn now(&self) -> i64 {
        self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Stage + activate one release explicitly (no restarter), leaving the
    /// activation PENDING (the apply row is Running): the exact state a
    /// supervisor observes between the pointer swap and the restart.
    async fn stage_and_activate(
        &self,
        version: &str,
        generation: u64,
        bytes: &[u8],
    ) -> (String, String) {
        let (signed, digest) = signed_manifest(&self.key, version, generation, bytes);
        self.fetcher.serve(
            &format!("https://mirror.test/{version}/faktor-cli-{version}-{OS}-{ARCH}"),
            bytes,
        );
        let staged = self
            .updater
            .stage_release(&signed, &components(), None, self.now())
            .await
            .unwrap();
        assert_eq!(staged.stage.digest, digest);
        let outcome = self.updater.activate_release(self.now(), None).unwrap();
        assert!(
            matches!(outcome, ReleaseOutcome::Activated { .. }),
            "{outcome:?}"
        );
        (staged.release_id, digest)
    }

    /// One COMPLETE explicit install: stage, activate, observe the digest
    /// and finalize. What a supervisor restarting through the bootstrap and
    /// then calling finalize produces.
    async fn install_release(
        &self,
        version: &str,
        generation: u64,
        bytes: &[u8],
    ) -> (String, String) {
        let (release_id, digest) = self.stage_and_activate(version, generation, bytes).await;
        let outcome = self.updater.finalize_release(&digest, self.now()).unwrap();
        assert!(
            matches!(outcome, ReleaseOutcome::Applied { .. }),
            "{outcome:?}"
        );
        (release_id, digest)
    }

    fn pointer(&self) -> InstallPointer {
        self.updater.layout().read_pointer().unwrap().unwrap()
    }

    fn launch_inputs(&self) -> LaunchInputs {
        LaunchInputs::new(self.root.clone())
    }

    fn launch_keys(&self) -> TrustedKeys {
        TrustFile::read(&self.updater.layout().trust_path()).unwrap()
    }

    fn operations(&self, kind: UpdateOpKind) -> Vec<faktor_updater::UpdateOperation> {
        self.store
            .list(100)
            .unwrap()
            .into_iter()
            .filter(|op| op.kind == kind)
            .collect()
    }
}

// ------------------------------------------------------------------ layout

#[tokio::test]
async fn stage_release_materializes_the_immutable_layout_before_any_pointer_moves() {
    let host = fixture([]);
    let (signed, digest) = signed_manifest(&host.key, "0.9.1", 1, b"release v1 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.1/faktor-cli-0.9.1-darwin-arm64",
        b"release v1 bytes",
    );
    let staged = host
        .updater
        .stage_release(&signed, &components(), None, NOW)
        .await
        .unwrap();
    let expected_id = faktor_updater::release_id_for("0.9.1", &digest);
    assert_eq!(staged.release_id, expected_id);
    // The exact staged bytes are materialized as versions/<id>/faktor.
    assert_eq!(
        faktor_updater::install::file_digest(&host.updater.layout().release_binary(&expected_id))
            .unwrap(),
        digest
    );
    // The signed manifest is stored next to the binary, byte-exactly.
    assert_eq!(
        std::fs::read(host.updater.layout().release_manifest(&expected_id)).unwrap(),
        signed
    );
    // The trust anchor exists and carries the configured allowlist.
    let anchor = TrustFile::read(&host.updater.layout().trust_path()).unwrap();
    assert_eq!(anchor.identities(), vec!["operator"]);
    assert!(host.updater.layout().launcher_path().is_file());
    // The pointer was NOT touched by staging.
    assert!(host.updater.layout().read_pointer().unwrap().is_none());
}

#[tokio::test]
async fn activate_swaps_the_pointer_to_the_release_the_bootstrap_will_exec() {
    let host = fixture([]);
    let (release_id, digest) = host.stage_and_activate("0.9.1", 1, b"v1 bytes").await;
    let pointer = host.pointer();
    assert_eq!(pointer.release_id.as_deref(), Some(release_id.as_str()));
    assert_eq!(pointer.digest, digest);
    assert_eq!(pointer.version, "0.9.1");
    // The apply row is still Running: no restart has been observed yet.
    let applies = host.operations(UpdateOpKind::Apply);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].status, UpdateOpStatus::Running);
    // The bootstrap resolves exactly the materialized binary.
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.release_id, release_id);
    assert_eq!(target.digest, digest);
    assert_eq!(
        target.binary,
        host.updater.layout().release_binary(&release_id)
    );
}

#[tokio::test]
async fn an_unsigned_or_tampered_release_manifest_is_refused_at_launch() {
    let host = fixture([]);
    let (release_id, digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let manifest_path = host.updater.layout().release_manifest(&release_id);

    // (a) unsigned: strip the signature.
    let signed = std::fs::read(&manifest_path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&signed).unwrap();
    value.as_object_mut().unwrap().remove("signature");
    std::fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "manifest_unsigned");

    // (b) tampered signature.
    let mut value: serde_json::Value = serde_json::from_slice(&signed).unwrap();
    let sig = value["signature"]["value"].as_str().unwrap();
    let mut raw = BASE64.decode(sig).unwrap();
    raw[0] ^= 0x01;
    value["signature"]["value"] = serde_json::Value::String(BASE64.encode(raw));
    std::fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "manifest_tampered");

    // (c) the pointer's digest must appear in the SIGNED manifest: restore
    // the signature but rewrite a field it covers (the digest).
    let mut value: serde_json::Value = serde_json::from_slice(&signed).unwrap();
    value["artifacts"][0]["sha256"] = serde_json::Value::String("c".repeat(64));
    std::fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "manifest_tampered");
    // The pointer still names the original digest.
    assert_eq!(host.pointer().digest, digest);
}

#[tokio::test]
async fn a_digest_mismatch_at_launch_is_refused_and_never_executed() {
    let host = fixture([]);
    let (release_id, _digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();

    // The bytes change AFTER verification (the documented TOCTOU window):
    // `launch` re-hashes immediately before exec and refuses.
    std::fs::write(host.updater.layout().release_binary(&release_id), b"evil").unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "launch_refused");
    assert!(
        err.to_string().contains("digest mismatch"),
        "the refusal names the digest mismatch: {err}"
    );
    let err = faktor_updater::launch(&target, &host.root, &[]).unwrap_err();
    assert_eq!(err.code(), "launch_refused");
    assert!(err
        .to_string()
        .contains("changed between verification and exec"));
}

#[tokio::test]
async fn an_empty_trust_anchor_and_a_legacy_pointer_are_refused() {
    let host = fixture([]);
    let (_release_id, _digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    // Empty anchor: no implicit trust.
    std::fs::write(
        host.updater.layout().trust_path(),
        serde_json::to_vec(&serde_json::json!({
            "schema": TRUST_FILE_SCHEMA,
            "keys": []
        }))
        .unwrap(),
    )
    .unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "launch_refused");
    assert!(err.to_string().contains("trust anchor"));

    // Restore the anchor; a legacy pointer (no release id) is refused too.
    std::fs::write(
        host.updater.layout().trust_path(),
        serde_json::to_vec(&TrustFile::from_keys(&trusted_keys(&host.key))).unwrap(),
    )
    .unwrap();
    host.updater
        .layout()
        .write_pointer(
            &InstallPointer::new("bundle.tar.gz", &"a".repeat(64), "0.1.0", "stable", NOW).unwrap(),
        )
        .unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "launch_refused");
    assert!(err.to_string().contains("no release id"));
}

// --------------------------------------------------------- supervised flow

#[tokio::test]
async fn apply_release_restarts_observes_the_new_digest_and_finalizes() {
    let host = fixture([Ok(())]);
    let (signed, digest) = signed_manifest(&host.key, "0.9.1", 1, b"v1 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.1/faktor-cli-0.9.1-darwin-arm64",
        b"v1 bytes",
    );
    host.updater
        .stage_release(&signed, &components(), None, NOW)
        .await
        .unwrap();
    let restarter = ScriptedRestarter::scripted([RestartStep::Reports(digest.clone())]);
    let outcome = host.updater.apply_release(NOW + 1, &restarter).unwrap();
    match outcome {
        ReleaseOutcome::Applied {
            running_digest,
            digest: applied,
            ..
        } => {
            assert_eq!(running_digest, digest, "the observed process digest");
            assert_eq!(applied, digest);
        }
        other => panic!("expected a finalized apply, got {other:?}"),
    }
    // Exactly one restart, against the pointer that names the new release.
    assert_eq!(restarter.calls(), vec![digest.clone()]);
    let applies = host.operations(UpdateOpKind::Apply);
    assert_eq!(applies[0].status, UpdateOpStatus::Applied);
    assert!(applies[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("the restarted process reports"));
    // No rollback row was recorded.
    assert!(host.operations(UpdateOpKind::Rollback).is_empty());
    // The launch resolution still names the release.
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.digest, digest);
}

#[tokio::test]
async fn a_restart_reporting_the_wrong_digest_rolls_back_and_restarts_the_previous_binary() {
    let host = fixture([]);
    let (v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;

    let (signed_v2, v2_digest) = signed_manifest(&host.key, "0.9.2", 2, b"v2 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.2/faktor-cli-0.9.2-darwin-arm64",
        b"v2 bytes",
    );
    host.updater
        .stage_release(&signed_v2, &components(), None, NOW + 2)
        .await
        .unwrap();
    let restarter = ScriptedRestarter::scripted([
        RestartStep::Reports("f".repeat(64)), // the restarted process lies
        RestartStep::Reports(v1_digest.clone()), // the previous binary is restarted
    ]);
    let outcome = host.updater.apply_release(NOW + 3, &restarter).unwrap();
    match outcome {
        ReleaseOutcome::RolledBack {
            digest,
            restored_digest,
            running_digest,
            reason,
            ..
        } => {
            assert_eq!(digest, v2_digest);
            assert_eq!(restored_digest.as_deref(), Some(v1_digest.as_str()));
            assert_eq!(running_digest.as_deref(), Some(v1_digest.as_str()));
            assert!(reason.contains("reports release digest"), "{reason}");
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    // The restarter was asked to run v2 and then the EXACT previous v1.
    assert_eq!(
        restarter.calls(),
        vec![v2_digest.clone(), v1_digest.clone()]
    );
    // The pointer is back at the previous release with its release id.
    let pointer = host.pointer();
    assert_eq!(pointer.digest, v1_digest);
    assert_eq!(pointer.release_id.as_deref(), Some(v1_id.as_str()));
    let applies = host.operations(UpdateOpKind::Apply);
    assert_eq!(applies[0].status, UpdateOpStatus::RolledBack);
    let rollbacks = host.operations(UpdateOpKind::Rollback);
    assert_eq!(rollbacks.len(), 1);
    assert_eq!(
        rollbacks[0].after_digest.as_deref(),
        Some(v1_digest.as_str())
    );
    // The bootstrap resolves the previous release again.
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.digest, v1_digest);
    assert_eq!(target.release_id, v1_id);
}

#[tokio::test]
async fn a_restart_error_and_a_post_restart_probe_failure_roll_back() {
    // (a) the restarter refuses to start the new binary.
    let host = fixture([]);
    let (v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let (signed_v2, v2_digest) = signed_manifest(&host.key, "0.9.2", 2, b"v2 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.2/faktor-cli-0.9.2-darwin-arm64",
        b"v2 bytes",
    );
    host.updater
        .stage_release(&signed_v2, &components(), None, NOW + 2)
        .await
        .unwrap();
    let restarter = ScriptedRestarter::scripted([
        RestartStep::Fails("systemd refused".into()),
        RestartStep::Reports(v1_digest.clone()),
    ]);
    let outcome = host.updater.apply_release(NOW + 3, &restarter).unwrap();
    let ReleaseOutcome::RolledBack { digest, reason, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert_eq!(digest, v2_digest);
    assert!(reason.contains("systemd refused"), "{reason}");
    assert_eq!(host.pointer().digest, v1_digest);
    assert_eq!(host.pointer().release_id.as_deref(), Some(v1_id.as_str()));

    // (b) the restart succeeds but the post-restart health probe fails.
    // Probes: v1 activation, v1 finalize, then the v2 post-restart check.
    let host = fixture([Ok(()), Ok(()), Err("store failed to reopen".into())]);
    let (v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let (signed_v2, v2_digest) = signed_manifest(&host.key, "0.9.2", 2, b"v2 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.2/faktor-cli-0.9.2-darwin-arm64",
        b"v2 bytes",
    );
    host.updater
        .stage_release(&signed_v2, &components(), None, NOW + 2)
        .await
        .unwrap();
    let restarter = ScriptedRestarter::scripted([
        RestartStep::Reports(v2_digest.clone()),
        RestartStep::Reports(v1_digest.clone()),
    ]);
    let outcome = host.updater.apply_release(NOW + 3, &restarter).unwrap();
    let ReleaseOutcome::RolledBack { digest, reason, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert_eq!(digest, v2_digest);
    assert!(reason.contains("store failed to reopen"), "{reason}");
    assert_eq!(restarter.calls(), vec![v2_digest, v1_digest.clone()]);
    assert_eq!(host.pointer().digest, v1_digest);
    assert_eq!(host.pointer().release_id.as_deref(), Some(v1_id.as_str()));
    // No stale apply row is left Running.
    assert!(host.store.running().unwrap().is_empty());
}

// ------------------------------------------------- explicit restart & abort

#[tokio::test]
async fn finalize_requires_the_reported_running_digest_and_abort_restores_v1() {
    let host = fixture([]);
    let (v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    // v2 stays PENDING (activated, not finalized) for this test.
    let (v2_id, v2_digest) = host.stage_and_activate("0.9.2", 2, b"v2 bytes").await;
    assert_ne!(v1_id, v2_id);

    // A wrong reported digest refuses the finalize and changes nothing.
    let err = host
        .updater
        .finalize_release(&"f".repeat(64), NOW + 4)
        .unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert_eq!(host.pointer().digest, v2_digest);

    // Explicit abort restores the exact previous release.
    let outcome = host.updater.abort_release(NOW + 5).unwrap();
    match outcome {
        ReleaseOutcome::RolledBack {
            release_id,
            restored_digest,
            ..
        } => {
            assert_eq!(release_id, v2_id);
            assert_eq!(restored_digest.as_deref(), Some(v1_digest.as_str()));
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    let pointer = host.pointer();
    assert_eq!(pointer.release_id.as_deref(), Some(v1_id.as_str()));
    assert_eq!(pointer.digest, v1_digest);
    assert!(host.store.running().unwrap().is_empty());
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.release_id, v1_id);

    // Re-activate v2 (a legitimate forward move again after the abort) and
    // finalize with the observed digest.
    host.updater.activate_release(NOW + 6, None).unwrap();
    let outcome = host.updater.finalize_release(&v2_digest, NOW + 7).unwrap();
    assert!(matches!(outcome, ReleaseOutcome::Applied { .. }));
    let applies = host.operations(UpdateOpKind::Apply);
    // The v2 activation is the newest applied row (v1 stays applied too).
    let newest = applies.first().unwrap();
    assert_eq!(newest.status, UpdateOpStatus::Applied);
    assert_eq!(newest.after_digest.as_deref(), Some(v2_digest.as_str()));
    assert!(newest
        .detail
        .as_deref()
        .unwrap()
        .contains("finalized: the running process reports"));
}

// ------------------------------------------------------- explicit rollback

#[tokio::test]
async fn rollback_reactivates_the_exact_previous_release_digest_and_binary() {
    let host = fixture([]);
    let (_v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let (v2_id, _v2_digest) = host.install_release("0.9.2", 2, b"v2 bytes").await;

    let outcome = host.updater.rollback(NOW + 8).unwrap();
    let ApplyOutcome::RolledBack { digest, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert_eq!(digest, v1_digest);
    let pointer = host.pointer();
    assert_eq!(pointer.digest, v1_digest);
    assert!(pointer.release_id.is_some(), "the release identity is kept");
    assert_ne!(pointer.release_id.as_deref(), Some(v2_id.as_str()));
    // The bootstrap now resolves the previous release's binary — not just a
    // metadata pointer.
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.digest, v1_digest);
    assert_eq!(
        faktor_updater::install::file_digest(&target.binary).unwrap(),
        v1_digest
    );
    assert!(target.binary.ends_with("faktor"));
}

// ------------------------------------------------------------ crash seams

#[tokio::test]
async fn recovery_never_calls_an_unobserved_release_applied() {
    let host = fixture([]);
    // The activation is left PENDING: this is the crash residue recovery
    // must resolve.
    let (release_id, digest) = host.stage_and_activate("0.9.1", 1, b"v1 bytes").await;

    // Seam 1: the daemon restarted DIRECTLY (no bootstrap release digest) —
    // the pointer is in place but nothing proves which binary runs.
    let apply_id = host.operations(UpdateOpKind::Apply)[0].id.clone();
    let outcomes = host
        .updater
        .recover_with_running_digest(NOW + 2, None)
        .unwrap();
    assert!(matches!(
        outcomes[0],
        faktor_updater::RecoveryOutcome::NeedsVerification { .. }
    ));
    assert_eq!(host.pointer().digest, digest);
    // Force-verification resolved the row; a later restart re-runs the
    // recovery. Simulate the same crash residue again (raw status restore)
    // for the next seam.
    let reopen = |status: UpdateOpStatus| {
        let mut row = host.store.get(apply_id.as_str()).unwrap().unwrap();
        row.status = status;
        host.store.update(&row).unwrap();
    };

    // Seam 2: a stale process reports a different digest.
    reopen(UpdateOpStatus::Running);
    let outcomes = host
        .updater
        .recover_with_running_digest(NOW + 3, Some(&"f".repeat(64)))
        .unwrap();
    assert!(matches!(
        outcomes[0],
        faktor_updater::RecoveryOutcome::NeedsVerification { .. }
    ));

    // Seam 3: the restarted process reports the activated digest: resumed.
    reopen(UpdateOpStatus::Running);
    let outcomes = host
        .updater
        .recover_with_running_digest(NOW + 4, Some(&digest))
        .unwrap();
    assert!(matches!(
        outcomes[0],
        faktor_updater::RecoveryOutcome::Resumed { .. }
    ));
    let applies = host.operations(UpdateOpKind::Apply);
    assert_eq!(applies[0].status, UpdateOpStatus::Applied);
    assert_eq!(
        applies[0].detail.as_deref().unwrap(),
        "recovered: the interrupted swap is healthy"
    );
    // The release identity survived the recovery.
    assert_eq!(
        host.pointer().release_id.as_deref(),
        Some(release_id.as_str())
    );
}

#[tokio::test]
async fn a_crash_after_a_release_rollback_restart_forces_verification() {
    let host = fixture([]);
    let (v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let (signed_v2, v2_digest) = signed_manifest(&host.key, "0.9.2", 2, b"v2 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.2/faktor-cli-0.9.2-darwin-arm64",
        b"v2 bytes",
    );
    host.updater
        .stage_release(&signed_v2, &components(), None, NOW + 2)
        .await
        .unwrap();

    // Simulate the crash seam directly: a Running apply row whose pointer
    // was already restored to v1, while the process that was restarted is
    // still v2. No store API can produce this; the row is raw-inserted.
    let mut op = faktor_updater::UpdateOperation::new(UpdateOpKind::Apply, NOW + 3, None);
    op.before_version = Some("0.9.1".into());
    op.before_digest = Some(v1_digest.clone());
    op.before_artifact = Some("faktor-cli-0.9.1-darwin-arm64".into());
    op.after_version = Some("0.9.2".into());
    op.after_digest = Some(v2_digest.clone());
    op.artifact = Some("faktor-cli-0.9.2-darwin-arm64".into());
    host.store.insert(&op).unwrap();
    assert_eq!(
        host.pointer().digest,
        v1_digest,
        "the pointer is rolled back"
    );

    let outcomes = host
        .updater
        .recover_with_running_digest(NOW + 4, Some(&v2_digest))
        .unwrap();
    assert!(matches!(
        outcomes[0],
        faktor_updater::RecoveryOutcome::NeedsVerification { .. }
    ));
    // With the running process agreeing with the pointer, the crashed apply
    // is abandoned normally (the previous install is intact).
    host.store
        .update(&faktor_updater::UpdateOperation {
            status: UpdateOpStatus::Running,
            ..op.clone()
        })
        .unwrap();
    let outcomes = host
        .updater
        .recover_with_running_digest(NOW + 5, Some(&v1_digest))
        .unwrap();
    assert!(matches!(
        outcomes[0],
        faktor_updater::RecoveryOutcome::Abandoned { .. }
    ));
    assert_eq!(host.pointer().release_id.as_deref(), Some(v1_id.as_str()));
}

#[tokio::test]
async fn the_launcher_is_stable_across_releases_and_release_ids_are_plain_names() {
    let host = fixture([]);
    let (signed_v1, _) = signed_manifest(&host.key, "0.9.1", 1, b"v1 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.1/faktor-cli-0.9.1-darwin-arm64",
        b"v1 bytes",
    );
    host.updater
        .stage_release(&signed_v1, &components(), None, NOW)
        .await
        .unwrap();
    let launcher_digest =
        faktor_updater::install::file_digest(&host.updater.layout().launcher_path()).unwrap();

    let (signed_v2, _) = signed_manifest(&host.key, "0.9.2", 2, b"v2 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.2/faktor-cli-0.9.2-darwin-arm64",
        b"v2 bytes",
    );
    host.updater
        .stage_release(&signed_v2, &components(), None, NOW + 1)
        .await
        .unwrap();
    let launcher_after =
        faktor_updater::install::file_digest(&host.updater.layout().launcher_path()).unwrap();
    assert_eq!(
        launcher_digest, launcher_after,
        "the bootstrap launcher is never replaced by a release"
    );

    // A hostile release id can never become a directory name.
    assert!(faktor_updater::release::validate_release_id("../../escape").is_err());
    assert!(host
        .updater
        .layout()
        .materialize_release("../../escape", "n", &"a".repeat(64), b"x")
        .is_err());
}

#[test]
fn the_running_build_report_hashes_the_live_executable() {
    let digest = faktor_updater::self_digest().unwrap();
    assert_eq!(digest.len(), 64, "sha256 hex of the running executable");
    // Without the bootstrap environment there is no claimed release.
    if std::env::var(faktor_updater::RELEASE_DIGEST_ENV).is_err() {
        assert!(faktor_updater::running_release().is_none());
        assert!(faktor_updater::running_install_root().is_none());
    }
}

// ------------------------------------------------- completeness / crash seams

/// The pre-atomic crash residue: the binary of a release landed but its
/// signed manifest never did. It is INCOMPLETE, never adoptable, and launch
/// resolution refuses it typed (instead of a "missing manifest" that reads
/// like tampering).
#[tokio::test]
async fn a_manifest_less_release_directory_is_refused_at_launch_typed() {
    let host = fixture([]);
    let (release_id, digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let binary = host.updater.layout().release_binary(&release_id);
    assert_eq!(
        faktor_updater::install::file_digest(&binary).unwrap(),
        digest,
        "the binary matches the pointer's digest (what the old check trusted)"
    );
    std::fs::remove_file(host.updater.layout().release_manifest(&release_id)).unwrap();
    let err = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap_err();
    assert_eq!(err.code(), "launch_refused");
    let text = err.to_string();
    assert!(text.contains("incomplete"), "{text}");
    assert!(text.contains("manifest"), "{text}");
}

/// A crash between binary and manifest (the old non-atomic order) is never
/// adopted: `apply` refuses it typed, `current` stays on the previous
/// complete release, no operation row is recorded for the refusal, and a
/// restart still resolves the previous release through the bootstrap.
#[tokio::test]
async fn an_incomplete_release_dir_is_never_adopted_and_current_stays_previous() {
    let host = fixture([]);
    let (_v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;

    // Stage v2 (complete), then simulate the crash seam: the binary landed,
    // the signed manifest never did.
    let (signed_v2, _v2_digest) = signed_manifest(&host.key, "0.9.2", 2, b"v2 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.2/faktor-cli-0.9.2-darwin-arm64",
        b"v2 bytes",
    );
    let staged = host
        .updater
        .stage_release(&signed_v2, &components(), None, host.now())
        .await
        .unwrap();
    std::fs::remove_file(host.updater.layout().release_manifest(&staged.release_id)).unwrap();
    assert!(
        std::path::Path::new(&staged.binary).is_file(),
        "the binary alone is present: exactly the manifest-less residue"
    );

    let rows_before = host.store.list(100).unwrap().len();
    let err = host.updater.apply(host.now()).unwrap_err();
    match &err {
        UpdateError::Install(detail) => {
            assert!(detail.contains("incomplete"), "{detail}");
            assert!(detail.contains("manifest"), "{detail}");
        }
        other => panic!("expected a typed incomplete-release refusal, got {other:?}"),
    }
    assert_eq!(rows_before, host.store.list(100).unwrap().len());
    // `current` never names the incomplete directory.
    assert_eq!(host.pointer().digest, v1_digest);
    assert_ne!(
        host.pointer().release_id.as_deref(),
        Some(staged.release_id.as_str())
    );
    // Restart works: the bootstrap resolves the previous COMPLETE release.
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.digest, v1_digest);
    assert!(target.binary.is_file());
}

/// Re-materializing rebuilds an incomplete directory atomically (the temp is
/// complete before the old directory is touched), leaves exactly the two
/// release files, and clears stale `.kp-tmp-` residue.
#[tokio::test]
async fn rematerializing_rebuilds_an_incomplete_release_and_clears_temp_residue() {
    let host = fixture([]);
    let (signed, digest) = signed_manifest(&host.key, "0.9.1", 1, b"v1 bytes");
    host.fetcher.serve(
        "https://mirror.test/0.9.1/faktor-cli-0.9.1-darwin-arm64",
        b"v1 bytes",
    );
    let staged = host
        .updater
        .stage_release(&signed, &components(), None, NOW)
        .await
        .unwrap();
    let layout = host.updater.layout();
    // Crash residue: no manifest, plus a stale partial temp dir from the
    // interrupted materialization.
    std::fs::remove_file(layout.release_manifest(&staged.release_id)).unwrap();
    let stale = layout
        .versions_dir()
        .join(format!(".{}.kp-tmp-999-1", staged.release_id));
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("faktor"), b"partial").unwrap();

    let dir = layout
        .materialize_release(
            &staged.release_id,
            &staged.stage.artifact.name,
            &digest,
            &signed,
        )
        .unwrap();
    assert_eq!(
        faktor_updater::install::file_digest(&layout.release_binary(&staged.release_id)).unwrap(),
        digest
    );
    assert_eq!(
        std::fs::read(layout.release_manifest(&staged.release_id)).unwrap(),
        signed
    );
    assert!(!stale.exists(), "stale temp residue is cleared");
    let temps: Vec<String> = std::fs::read_dir(layout.versions_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".kp-tmp-"))
        .collect();
    assert!(temps.is_empty(), "no temp residue survives: {temps:?}");
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["faktor".to_string(), "manifest".to_string()]);
    assert_eq!(
        layout
            .release_id_if_materialized("0.9.1", &digest)
            .unwrap()
            .as_deref(),
        Some(staged.release_id.as_str())
    );

    // An empty manifest is refused before anything is published.
    assert!(layout
        .materialize_release(
            &staged.release_id,
            &staged.stage.artifact.name,
            &digest,
            b""
        )
        .is_err());
}

/// A rollback whose previous release is absent or corrupt refuses typed and
/// leaves `current` untouched — never a false "restored" pointer the
/// bootstrap would refuse on the next restart.
#[tokio::test]
async fn rollback_refuses_typed_when_the_previous_release_is_missing_or_corrupt() {
    let host = fixture([]);
    let (v1_id, v1_digest) = host.install_release("0.9.1", 1, b"v1 bytes").await;
    let (_v2_id, v2_digest) = host.install_release("0.9.2", 2, b"v2 bytes").await;

    let v1_binary = host.updater.layout().release_binary(&v1_id);
    let saved = std::fs::read(&v1_binary).unwrap();

    // (a) the previous release binary vanished (dir holds only the manifest).
    std::fs::remove_file(&v1_binary).unwrap();
    let err = host.updater.rollback(host.now()).unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::Install(_) | UpdateError::StagedArtifactUnusable { .. }
        ),
        "{err:?}"
    );
    assert_eq!(
        host.pointer().digest,
        v2_digest,
        "no false restored: the pointer is untouched"
    );

    // (b) the previous release binary is corrupt (digest mismatch).
    std::fs::write(&v1_binary, b"rot").unwrap();
    let err = host.updater.rollback(host.now()).unwrap_err();
    assert_eq!(err.code(), "staged_artifact_unusable");
    assert_eq!(host.pointer().digest, v2_digest);
    assert!(
        host.operations(UpdateOpKind::Rollback).is_empty(),
        "a refused rollback records no rollback row"
    );

    // (c) control: a complete previous release rolls back exactly and the
    // bootstrap can resolve it again.
    std::fs::write(&v1_binary, &saved).unwrap();
    let outcome = host.updater.rollback(host.now()).unwrap();
    let ApplyOutcome::RolledBack { digest, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert_eq!(digest, v1_digest);
    assert_eq!(host.pointer().release_id.as_deref(), Some(v1_id.as_str()));
    let target = resolve_launch(&host.launch_inputs(), &host.launch_keys()).unwrap();
    assert_eq!(target.digest, v1_digest);
}

// ------------------------------------------- recovery re-verification

/// A high-water mark that moved between the crash and recovery (a concurrent
/// admission) makes resuming the interrupted activation a rollback: recovery
/// forces verification and never claims Applied.
#[tokio::test]
async fn recovery_refuses_to_resume_when_the_high_water_moved_after_the_crash() {
    let host = fixture([]);
    let (release_id, digest) = host.stage_and_activate("0.9.1", 1, b"v1 bytes").await;

    // The concurrent admission: the durable floor is raised past the
    // interrupted operation's signed generation while it is still Running.
    host.store
        .raise_high_water("stable", 9, false, host.now())
        .unwrap();
    let outcomes = host
        .updater
        .recover_with_running_digest(host.now(), Some(&digest))
        .unwrap();
    assert!(
        matches!(
            outcomes[0],
            faktor_updater::RecoveryOutcome::NeedsVerification { .. }
        ),
        "{outcomes:?}"
    );
    let applies = host.operations(UpdateOpKind::Apply);
    assert_eq!(applies[0].status, UpdateOpStatus::Unverified);
    let detail = applies[0].detail.as_deref().unwrap();
    assert!(detail.contains("high-water"), "{detail}");
    // The pointer was not moved and nothing claims Applied.
    assert_eq!(host.pointer().digest, digest);
    assert_eq!(
        host.pointer().release_id.as_deref(),
        Some(release_id.as_str())
    );
}

/// A signed release manifest that no longer verifies at recovery time (here:
/// the signature is stripped after the crash) is re-authenticated and
/// refused: recovery forces verification instead of resuming it.
#[tokio::test]
async fn recovery_refuses_to_resume_when_the_signed_manifest_no_longer_verifies() {
    let host = fixture([]);
    let (release_id, digest) = host.stage_and_activate("0.9.1", 1, b"v1 bytes").await;
    let manifest_path = host.updater.layout().release_manifest(&release_id);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("signature");
    std::fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();

    let outcomes = host
        .updater
        .recover_with_running_digest(host.now(), Some(&digest))
        .unwrap();
    assert!(
        matches!(
            outcomes[0],
            faktor_updater::RecoveryOutcome::NeedsVerification { .. }
        ),
        "{outcomes:?}"
    );
    let applies = host.operations(UpdateOpKind::Apply);
    assert_eq!(applies[0].status, UpdateOpStatus::Unverified);
    assert!(
        applies[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("no longer verifies"),
        "{:?}",
        applies[0].detail
    );
}

/// The honest recovery path still resumes when everything re-verifies: the
/// signed manifest is intact and the floor still admits the operation.
#[tokio::test]
async fn recovery_resumes_when_the_manifest_and_high_water_still_admit_it() {
    let host = fixture([]);
    let (release_id, digest) = host.stage_and_activate("0.9.1", 1, b"v1 bytes").await;
    let outcomes = host
        .updater
        .recover_with_running_digest(host.now(), Some(&digest))
        .unwrap();
    assert!(
        matches!(outcomes[0], faktor_updater::RecoveryOutcome::Resumed { .. }),
        "{outcomes:?}"
    );
    assert_eq!(
        host.pointer().release_id.as_deref(),
        Some(release_id.as_str())
    );
    assert_eq!(
        host.operations(UpdateOpKind::Apply)[0].status,
        UpdateOpStatus::Applied
    );
}
