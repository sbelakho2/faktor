//! Adversarial lifecycle tests for the signed updater: every refusal path,
//! the crash windows around the swap, the automatic rollback on a failed
//! health probe, durable before/after evidence, and deterministic channels.
//!
//! The harness injects a fake transport and a scripted health probe; no
//! network and no real install are ever touched.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use faktor_updater::{
    manifest, ApplyOutcome, Artifact, Channel, Compatibility, InstallLayout, InstallPointer,
    ManifestRefusal, MemoryUpdaterStore, RecoveryOutcome, RunningComponents, TrustedKey,
    TrustedKeys, UpdateError, UpdateOpKind, UpdateOpStatus, UpdateOperation, UpdateStoreError,
    Updater, UpdaterConfig, UpdaterStore, VersionRange,
};

// ------------------------------------------------------------- test harness

struct FakeFetcher {
    payloads: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
    mutate: Mutex<Option<usize>>,
}

impl FakeFetcher {
    fn new() -> Self {
        FakeFetcher {
            payloads: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            mutate: Mutex::new(None),
        }
    }

    fn serve(&self, url: &str, payload: &[u8]) {
        self.payloads
            .lock()
            .unwrap()
            .insert(url.to_string(), payload.to_vec());
    }

    /// Corrupt byte `index` of every served payload (digest mismatch).
    fn tamper(&self, index: usize) {
        *self.mutate.lock().unwrap() = Some(index);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
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
        let mut payload = payload;
        if let Some(index) = *self.mutate.lock().unwrap() {
            if let Some(byte) = payload.get_mut(index) {
                *byte ^= 0x01;
            }
        }
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
        match self.results.lock().unwrap().pop_front() {
            Some(Err(detail)) => Err(UpdateError::HealthFailed { detail }),
            Some(Ok(())) | None => Ok(()),
        }
    }
}

const OS: &str = "darwin";
const ARCH: &str = "arm64";
const NOW: i64 = 1_700_000_000_000;

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

struct HostFixture {
    _dir: tempfile::TempDir,
    updater: Arc<Updater>,
    fetcher: Arc<FakeFetcher>,
    store: Arc<MemoryUpdaterStore>,
}

fn fixture(probe_results: impl IntoIterator<Item = Result<(), String>>) -> HostFixture {
    let dir = tempfile::tempdir().unwrap();
    let install_root = dir.path().join("install");
    let fetcher = Arc::new(FakeFetcher::new());
    let store = Arc::new(MemoryUpdaterStore::new());
    let probe = Arc::new(ScriptedProbe::scripted(probe_results));
    let key = keypair();
    let updater = Arc::new(
        Updater::new(
            UpdaterConfig {
                channel: Channel::Stable,
                install_root: install_root.clone(),
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
            probe.clone(),
        )
        .unwrap(),
    );
    HostFixture {
        _dir: dir,
        updater,
        fetcher,
        store,
    }
}

/// The signed anti-rollback generation of one test version: monotonic in
/// the versions these tests stage (major.minor.patch -> major*1e6 +
/// minor*1e3 + patch), so the lifecycle fixtures exercise the normal
/// forward path without touching the legacy-manifest allowance.
fn generation_of(version: &str) -> u64 {
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    major * 1_000_000 + minor * 1_000 + patch
}

/// One artifact for the fake transport + its signed manifest bytes.
fn signed_manifest(
    key: &SigningKey,
    version: &str,
    payload: &[u8],
    overrides: impl FnOnce(&mut manifest::UpdateManifest),
) -> (Vec<u8>, String) {
    let digest = manifest::sha256_hex(payload);
    let name = format!("faktor-cli-{version}-{OS}-{ARCH}.tar.gz");
    let url = format!("https://mirror.test/{version}/{name}");
    let mut manifest = manifest::UpdateManifest {
        schema: manifest::UPDATE_MANIFEST_SCHEMA.to_string(),
        channel: Channel::Stable,
        version: version.to_string(),
        commit: format!("{version:0>40}").replace('.', "a"),
        release_generation: Some(generation_of(version)),
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
    overrides(&mut manifest);
    let signing_payload = manifest.signing_payload().unwrap();
    let signature = key.sign(&signing_payload);
    manifest.signature = Some(manifest::ManifestSignature {
        algorithm: "ed25519".into(),
        identity: "operator".into(),
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        value: BASE64.encode(signature.to_bytes()),
    });
    (serde_json::to_vec(&manifest).unwrap(), digest)
}

fn components() -> RunningComponents {
    RunningComponents::from_strs(Some("0.1.0"), Some("0.1.0"), None, None, Some(1)).unwrap()
}

fn operations(store: &MemoryUpdaterStore, kind: UpdateOpKind) -> Vec<UpdateOperation> {
    store
        .list(100)
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == kind)
        .collect()
}

// ------------------------------------------------------------------- refusals

#[tokio::test]
async fn unsigned_unknown_key_expired_and_tampered_manifests_are_refused() {
    let host = fixture([]);
    let key = keypair();
    let payload = b"bundle-v1";
    let (signed, _digest) = signed_manifest(&key, "0.2.0", payload, |_| {});
    let value: serde_json::Value = serde_json::from_slice(&signed).unwrap();

    // (a) unsigned.
    let mut unsigned = value.clone();
    unsigned.as_object_mut().unwrap().remove("signature");
    let err = host
        .updater
        .check(&serde_json::to_vec(&unsigned).unwrap(), &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_unsigned");

    // (b) unknown operator identity.
    let mut unknown = value.clone();
    unknown["signature"]["identity"] = serde_json::json!("somebody-else");
    let err = host
        .updater
        .check(&serde_json::to_vec(&unknown).unwrap(), &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_unknown_key");

    // (c) tampered artifact digest after signing.
    let mut tampered = value.clone();
    tampered["artifacts"][0]["sha256"] = serde_json::json!("c".repeat(64));
    let err = host
        .updater
        .check(&serde_json::to_vec(&tampered).unwrap(), &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_tampered");

    // (d) expired (signature still authentic).
    let (expired, _) = signed_manifest(&key, "0.2.0", payload, |m| {
        m.issued_at = NOW - 200_000;
        m.expires_at = NOW - 100_000;
    });
    let err = host
        .updater
        .check(&expired, &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_expired");
    assert!(matches!(
        err,
        UpdateError::Refused(ManifestRefusal::Expired { .. })
    ));

    // Nothing reached the store as staged and no artifact was published.
    assert!(operations(&host.store, UpdateOpKind::Stage).is_empty());
    assert!(host.updater.layout().read_pointer().unwrap().is_none());
    assert!(host.fetcher.calls().is_empty(), "refusals never download");
}

#[tokio::test]
async fn a_stage_refuses_an_unknown_artifact_host_and_records_the_refusal() {
    let host = fixture([]);
    let key = keypair();
    let (signed, _) = signed_manifest(&key, "0.2.0", b"bundle", |m| {
        m.artifacts[0].os = "windows".into();
        m.artifacts[0].arch = "x86_64".into();
    });
    let err = host.updater.check(&signed, &components(), NOW).unwrap_err();
    assert_eq!(err.code(), "manifest_no_artifact_for_host");
    let checks = operations(&host.store, UpdateOpKind::Check);
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, UpdateOpStatus::Failed);
    assert!(host.fetcher.calls().is_empty());
}

#[tokio::test]
async fn incompatible_ranges_are_refused_with_every_component_named() {
    let host = fixture([]);
    let key = keypair();
    let (signed, _) = signed_manifest(&key, "0.9.0", b"bundle", |m| {
        m.compatibility.cli = range("1.0.0", "2.0.0");
        m.compatibility.daemon = range("1.0.0", "2.0.0");
    });
    let err = host.updater.check(&signed, &components(), NOW).unwrap_err();
    assert_eq!(err.code(), "incompatible");
    let UpdateError::Incompatible(report) = &err else {
        panic!("expected a typed compatibility report, got {err}");
    };
    assert_eq!(report.refused_components(), vec!["cli", "daemon"]);
    assert!(report.to_string().contains("cli running 0.1.0"));
    // The refusal is durable, and nothing was downloaded.
    let checks = operations(&host.store, UpdateOpKind::Check);
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, UpdateOpStatus::Failed);
}

#[tokio::test]
async fn a_bounded_extension_range_with_an_unknown_running_version_refuses_by_name() {
    let host = fixture([]);
    let key = keypair();
    let (signed, _) = signed_manifest(&key, "0.2.0", b"bundle", |m| {
        m.compatibility.vscode = range("0.1.0", "0.2.0");
    });
    let err = host.updater.check(&signed, &components(), NOW).unwrap_err();
    let UpdateError::Incompatible(report) = err else {
        panic!("expected incompatibility");
    };
    assert_eq!(report.refused_components(), vec!["vscode"]);
}

#[tokio::test]
async fn an_expired_manifest_is_refused_even_when_everything_else_matches() {
    let host = fixture([]);
    let key = keypair();
    let (signed, _) = signed_manifest(&key, "0.2.0", b"bundle", |m| {
        m.issued_at = NOW - 500_000;
        m.expires_at = NOW + 1;
    });
    assert!(host.updater.check(&signed, &components(), NOW).is_ok());
    let err = host
        .updater
        .check(&signed, &components(), NOW + 2)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_expired");
}

#[tokio::test]
async fn channel_selection_is_deterministic_and_the_pin_refuses_other_channels() {
    let key = keypair();
    let (beta, _) = signed_manifest(&key, "0.3.0", b"beta", |m| {
        m.channel = Channel::Beta;
    });
    let host = fixture([]);
    let err = host.updater.check(&beta, &components(), NOW).unwrap_err();
    assert_eq!(err.code(), "manifest_channel_mismatch");

    // A beta client accepts the beta manifest, a dev client accepts both;
    // selection over a feed is pure and repeatable.
    let candidates = vec![
        faktor_updater::ChannelCandidate {
            channel: Channel::Stable,
            version: faktor_updater::Version::parse("0.2.0").unwrap(),
            commit: "a".repeat(40),
            issued_at: NOW - 10,
            expires_at: NOW + 100,
        },
        faktor_updater::ChannelCandidate {
            channel: Channel::Beta,
            version: faktor_updater::Version::parse("0.3.0").unwrap(),
            commit: "b".repeat(40),
            issued_at: NOW - 10,
            expires_at: NOW + 100,
        },
    ];
    let stable = faktor_updater::select(Channel::Stable, &candidates, NOW);
    let beta_pick = faktor_updater::select(Channel::Beta, &candidates, NOW);
    let dev = faktor_updater::select(Channel::Dev, &candidates, NOW);
    assert_eq!(stable, Some(0));
    assert_eq!(beta_pick, Some(1));
    assert_eq!(dev, Some(1));
    for _ in 0..16 {
        assert_eq!(
            faktor_updater::select(Channel::Beta, &candidates, NOW),
            beta_pick
        );
    }
}

// ------------------------------------------------------------------ integrity

#[tokio::test]
async fn a_digest_mismatch_download_is_refused_and_leaves_no_staged_artifact() {
    let host = fixture([]);
    let key = keypair();
    let (signed, digest) = signed_manifest(&key, "0.2.0", b"good bundle bytes", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"good bundle bytes",
    );
    host.fetcher.tamper(0);
    let err = host
        .updater
        .stage(&signed, &components(), None, NOW)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "digest_mismatch");
    assert!(matches!(err, UpdateError::DigestMismatch { .. }));
    // The bad bytes were discarded; nothing is published or pointed at.
    assert!(host.updater.layout().read_pointer().unwrap().is_none());
    assert!(!host
        .updater
        .layout()
        .artifact_path("faktor-cli-0.2.0-darwin-arm64.tar.gz", &digest)
        .exists());
    let stages = operations(&host.store, UpdateOpKind::Stage);
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].status, UpdateOpStatus::Failed);
    assert!(stages[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("digest mismatch"));
}

#[tokio::test]
async fn an_artifact_larger_than_the_bound_is_refused_before_publishing() {
    let host = fixture([]);
    let key = keypair();
    let big = vec![0u8; 2048];
    let (_signed, _digest) = signed_manifest(&key, "0.2.0", &big, |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        &big,
    );
    // The host bound is 1 MiB; a manifest declaring 4 GiB is refused before
    // any download starts.
    let (huge, _) = signed_manifest(&key, "0.2.1", &big, |m| {
        m.artifacts[0].size = Some(4 * 1024 * 1024 * 1024);
        m.artifacts[0].url = "https://mirror.test/huge".into();
    });
    let err = host
        .updater
        .stage(&huge, &components(), None, NOW)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "artifact_too_large");
    assert!(host.fetcher.calls().is_empty(), "no byte was fetched");
}

#[tokio::test]
async fn stage_then_apply_records_every_transition_with_before_after_digests() {
    let host = fixture([]);
    let key = keypair();
    let (signed, digest) = signed_manifest(&key, "0.2.0", b"bundle bytes", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"bundle bytes",
    );

    let staged = host
        .updater
        .stage(&signed, &components(), Some("key-1"), NOW)
        .await
        .unwrap();
    assert_eq!(staged.digest, digest);
    assert!(!staged.idempotent);
    // The install is untouched by staging.
    assert!(host.updater.layout().read_pointer().unwrap().is_none());

    // Idempotent replay: the same key returns the same op without a second
    // download.
    let replay = host
        .updater
        .stage(&signed, &components(), Some("key-1"), NOW + 1)
        .await
        .unwrap();
    assert!(replay.idempotent);
    assert_eq!(replay.op_id, staged.op_id);
    assert_eq!(host.fetcher.calls().len(), 1);
    // A different artifact under the same key is a conflict.
    let (other, _) = signed_manifest(&key, "0.2.1", b"other bytes", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.1/faktor-cli-0.2.1-darwin-arm64.tar.gz",
        b"other bytes",
    );
    let err = host
        .updater
        .stage(&other, &components(), Some("key-1"), NOW + 2)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "conflict");

    // Apply.
    let applied = host.updater.apply(NOW + 3).unwrap();
    let ApplyOutcome::Applied {
        digest: installed,
        version,
        ..
    } = applied
    else {
        panic!("expected an applied outcome");
    };
    assert_eq!(installed, digest);
    assert_eq!(version, "0.2.0");
    let pointer = host.updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, digest);
    assert_eq!(pointer.version, "0.2.0");

    // Durable rows: check(succeeded), stage(staged), apply(applied), with
    // before/after evidence.
    let checks = operations(&host.store, UpdateOpKind::Check);
    assert_eq!(
        checks.len(),
        3,
        "stage re-runs its check; the replay and the refused key reuse each record one"
    );
    assert!(checks
        .iter()
        .all(|op| op.status == UpdateOpStatus::Succeeded));
    let stages = operations(&host.store, UpdateOpKind::Stage);
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].status, UpdateOpStatus::Staged);
    assert_eq!(stages[0].before_digest, None);
    assert_eq!(stages[0].after_digest.as_deref(), Some(digest.as_str()));
    let applies = operations(&host.store, UpdateOpKind::Apply);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].status, UpdateOpStatus::Applied);
    assert_eq!(applies[0].before_version, None);
    assert_eq!(applies[0].after_version.as_deref(), Some("0.2.0"));
    assert_eq!(applies[0].after_digest.as_deref(), Some(digest.as_str()));

    // Applying the same staged artifact again is refused (it is already
    // installed), not silently repeated.
    let err = host.updater.apply(NOW + 4).unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert!(err.to_string().contains("already applied"), "{err}");
}

// ---------------------------------------------------- health probe & rollback

#[tokio::test]
async fn a_failed_health_probe_rolls_back_to_the_previous_digest_exactly() {
    // Probe script: apply of v1 passes, apply of v2 fails, the rollback
    // probe passes.
    let host = fixture([Ok(()), Err("store failed to reopen".into()), Ok(())]);
    let key = keypair();

    // Install v1.
    let (v1, v1_digest) = signed_manifest(&key, "0.1.0", b"v1 bytes", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.1.0/faktor-cli-0.1.0-darwin-arm64.tar.gz",
        b"v1 bytes",
    );
    host.updater
        .stage(&v1, &components(), None, NOW)
        .await
        .unwrap();
    assert!(matches!(
        host.updater.apply(NOW + 1).unwrap(),
        ApplyOutcome::Applied { .. }
    ));
    let installed_v1 = host.updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(installed_v1.digest, v1_digest);

    // Stage + apply v2; the probe fails and the install must return to v1.
    let (v2, v2_digest) = signed_manifest(&key, "0.2.0", b"v2 bytes", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"v2 bytes",
    );
    host.updater
        .stage(&v2, &components(), None, NOW + 2)
        .await
        .unwrap();
    let outcome = host.updater.apply(NOW + 3).unwrap();
    let ApplyOutcome::RolledBack { digest, reason, .. } = outcome else {
        panic!("expected a rollback outcome, got {outcome:?}");
    };
    assert_eq!(digest, v2_digest);
    assert!(reason.contains("store failed to reopen"), "{reason}");

    // EXACTLY the previous digest, version and artifact are installed again.
    let pointer = host.updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, v1_digest);
    assert_eq!(pointer.version, "0.1.0");
    assert_eq!(pointer.artifact, installed_v1.artifact);
    assert_eq!(pointer.digest, installed_v1.digest);

    // The apply row is rolled back and an explicit rollback row records the
    // before/after evidence.
    let applies = operations(&host.store, UpdateOpKind::Apply);
    assert_eq!(applies.len(), 2);
    assert_eq!(applies[0].status, UpdateOpStatus::RolledBack);
    assert!(applies[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("health probe failed"));
    let rollbacks = operations(&host.store, UpdateOpKind::Rollback);
    assert_eq!(rollbacks.len(), 1);
    assert_eq!(
        rollbacks[0].before_digest.as_deref(),
        Some(v2_digest.as_str())
    );
    assert_eq!(
        rollbacks[0].after_digest.as_deref(),
        Some(v1_digest.as_str())
    );
    // The v2 artifact is still on disk (content-addressed, harmless) but
    // nothing points at it.
    assert!(host
        .updater
        .layout()
        .artifact_path("faktor-cli-0.2.0-darwin-arm64.tar.gz", &v2_digest)
        .exists());
}

#[tokio::test]
async fn a_first_install_whose_probe_fails_removes_the_pointer() {
    let host = fixture([Err("new artifact is not runnable".into())]);
    let key = keypair();
    let (signed, _) = signed_manifest(&key, "0.2.0", b"first", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"first",
    );
    host.updater
        .stage(&signed, &components(), None, NOW)
        .await
        .unwrap();
    let outcome = host.updater.apply(NOW + 1).unwrap();
    assert!(matches!(outcome, ApplyOutcome::RolledBack { .. }));
    // The previous state (nothing installed) is restored exactly.
    assert!(host.updater.layout().read_pointer().unwrap().is_none());
}

#[tokio::test]
async fn an_explicit_rollback_restores_the_previous_artifact_and_is_refused_twice() {
    let host = fixture([]);
    let key = keypair();
    let (v1, v1_digest) = signed_manifest(&key, "0.1.0", b"v1", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.1.0/faktor-cli-0.1.0-darwin-arm64.tar.gz",
        b"v1",
    );
    host.updater
        .stage(&v1, &components(), None, NOW)
        .await
        .unwrap();
    host.updater.apply(NOW + 1).unwrap();

    let (v2, _) = signed_manifest(&key, "0.2.0", b"v2", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"v2",
    );
    host.updater
        .stage(&v2, &components(), None, NOW + 2)
        .await
        .unwrap();
    host.updater.apply(NOW + 3).unwrap();

    let outcome = host.updater.rollback(NOW + 4).unwrap();
    let ApplyOutcome::RolledBack { digest, .. } = outcome else {
        panic!("expected rollback");
    };
    assert_eq!(digest, v1_digest);
    // A second rollback has no applied operation with a previous artifact.
    let err = host.updater.rollback(NOW + 5).unwrap_err();
    assert_eq!(err.code(), "conflict");
}

// ----------------------------------------------------------------- crash safety

#[tokio::test]
async fn a_crash_between_stage_and_swap_leaves_the_old_version_intact() {
    let host = fixture([]);
    let key = keypair();
    // Install v1 normally.
    let (v1, v1_digest) = signed_manifest(&key, "0.1.0", b"v1", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.1.0/faktor-cli-0.1.0-darwin-arm64.tar.gz",
        b"v1",
    );
    host.updater
        .stage(&v1, &components(), None, NOW)
        .await
        .unwrap();
    host.updater.apply(NOW + 1).unwrap();

    // Crash residue #1: a partially downloaded staging directory plus the
    // Running `stage` row a real crash leaves behind.
    let (v2, _) = signed_manifest(&key, "0.2.0", b"v2", |_| {});
    let mut crashed_stage = UpdateOperation::new(UpdateOpKind::Stage, NOW + 2, None);
    crashed_stage.after_version = Some("0.2.0".into());
    crashed_stage.artifact = Some("faktor-cli-0.2.0-darwin-arm64.tar.gz".into());
    host.store.insert(&crashed_stage.clone()).unwrap();
    let staging_dir = host
        .updater
        .layout()
        .staging_dir_for(crashed_stage.id.as_str());
    std::fs::create_dir_all(&staging_dir).unwrap();
    std::fs::write(
        staging_dir.join("faktor-cli-0.2.0-darwin-arm64.tar.gz"),
        b"partial download",
    )
    .unwrap();

    // The unfinished stage blocks new work until recovery.
    let err = host
        .updater
        .stage(&v2, &components(), None, NOW + 3)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "conflict");

    let outcomes = host.updater.recover(NOW + 4).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], RecoveryOutcome::Abandoned { .. }));
    // The old version is still installed, byte-exact, and the partial
    // staging directory is gone.
    let pointer = host.updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, v1_digest);
    assert_eq!(pointer.version, "0.1.0");
    assert!(!staging_dir.exists());
    let recovered = host.store.get(crashed_stage.id.as_str()).unwrap().unwrap();
    assert_eq!(recovered.status, UpdateOpStatus::Failed);

    // Crash residue #2: the swap happened but the probe never ran (a crash
    // between the pointer write and the outcome row).
    let (v3, v3_digest) = signed_manifest(&key, "0.3.0", b"v3", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.3.0/faktor-cli-0.3.0-darwin-arm64.tar.gz",
        b"v3",
    );
    host.updater
        .stage(&v3, &components(), None, NOW + 5)
        .await
        .unwrap();
    let mut crashed_apply = UpdateOperation::new(UpdateOpKind::Apply, NOW + 6, None);
    crashed_apply.before_version = Some("0.1.0".into());
    crashed_apply.before_digest = Some(v1_digest.clone());
    crashed_apply.before_artifact = Some("faktor-cli-0.1.0-darwin-arm64.tar.gz".into());
    crashed_apply.after_version = Some("0.3.0".into());
    crashed_apply.after_digest = Some(v3_digest.clone());
    crashed_apply.artifact = Some("faktor-cli-0.3.0-darwin-arm64.tar.gz".into());
    host.store.insert(&crashed_apply.clone()).unwrap();
    // The swap was durable before the crash.
    host.updater
        .layout()
        .write_pointer(
            &InstallPointer::new(
                "faktor-cli-0.3.0-darwin-arm64.tar.gz",
                &v3_digest,
                "0.3.0",
                "stable",
                NOW + 6,
            )
            .unwrap(),
        )
        .unwrap();
    // Recovery RESUMES: the probe passes, the operation becomes applied.
    let outcomes = host.updater.recover(NOW + 7).unwrap();
    assert!(matches!(outcomes[0], RecoveryOutcome::Resumed { .. }));
    assert_eq!(
        host.store
            .get(crashed_apply.id.as_str())
            .unwrap()
            .unwrap()
            .status,
        UpdateOpStatus::Applied
    );
    assert_eq!(
        host.updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v3_digest
    );
}

#[tokio::test]
async fn a_crash_after_the_swap_is_rolled_back_when_the_resumed_probe_fails() {
    let host = fixture([Ok(()), Err("the new bundle is not runnable".into())]);
    let key = keypair();
    let (v1, v1_digest) = signed_manifest(&key, "0.1.0", b"v1", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.1.0/faktor-cli-0.1.0-darwin-arm64.tar.gz",
        b"v1",
    );
    host.updater
        .stage(&v1, &components(), None, NOW)
        .await
        .unwrap();
    host.updater.apply(NOW + 1).unwrap();
    let (v2, v2_digest) = signed_manifest(&key, "0.2.0", b"v2", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"v2",
    );
    host.updater
        .stage(&v2, &components(), None, NOW + 2)
        .await
        .unwrap();

    // Crash with the pointer already swapped to v2 and the apply running.
    let mut crashed_apply = UpdateOperation::new(UpdateOpKind::Apply, NOW + 3, None);
    crashed_apply.before_version = Some("0.1.0".into());
    crashed_apply.before_digest = Some(v1_digest.clone());
    crashed_apply.before_artifact = Some("faktor-cli-0.1.0-darwin-arm64.tar.gz".into());
    crashed_apply.after_version = Some("0.2.0".into());
    crashed_apply.after_digest = Some(v2_digest.clone());
    crashed_apply.artifact = Some("faktor-cli-0.2.0-darwin-arm64.tar.gz".into());
    host.store.insert(&crashed_apply).unwrap();
    host.updater
        .layout()
        .write_pointer(
            &InstallPointer::new(
                "faktor-cli-0.2.0-darwin-arm64.tar.gz",
                &v2_digest,
                "0.2.0",
                "stable",
                NOW + 3,
            )
            .unwrap(),
        )
        .unwrap();

    let outcomes = host.updater.recover(NOW + 4).unwrap();
    assert!(matches!(outcomes[0], RecoveryOutcome::RolledBack { .. }));
    assert_eq!(
        host.updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v1_digest
    );
}

#[tokio::test]
async fn recovery_never_guesses_when_the_pointer_matches_neither_side() {
    let host = fixture([]);
    let mut crashed_apply = UpdateOperation::new(UpdateOpKind::Apply, NOW, None);
    crashed_apply.before_digest = Some("1".repeat(64));
    crashed_apply.after_digest = Some("2".repeat(64));
    host.store.insert(&crashed_apply.clone()).unwrap();
    host.updater
        .layout()
        .write_pointer(
            &InstallPointer::new("a.tar.gz", &"3".repeat(64), "9", "stable", NOW).unwrap(),
        )
        .unwrap();
    let outcomes = host.updater.recover(NOW + 1).unwrap();
    assert!(matches!(
        outcomes[0],
        RecoveryOutcome::NeedsVerification { .. }
    ));
    let row = host.store.get(crashed_apply.id.as_str()).unwrap().unwrap();
    assert_eq!(row.status, UpdateOpStatus::Unverified);
    // The pointer was NOT silently changed.
    assert_eq!(
        host.updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        "3".repeat(64)
    );
}

#[tokio::test]
async fn recovery_of_a_crashed_apply_before_the_swap_abandons_it() {
    let host = fixture([]);
    let mut crashed_apply = UpdateOperation::new(UpdateOpKind::Apply, NOW, None);
    crashed_apply.before_digest = Some("1".repeat(64));
    crashed_apply.before_artifact = Some("old.tar.gz".into());
    crashed_apply.before_version = Some("0.1.0".into());
    crashed_apply.after_digest = Some("2".repeat(64));
    host.store.insert(&crashed_apply.clone()).unwrap();
    host.updater
        .layout()
        .write_pointer(
            &InstallPointer::new("old.tar.gz", &"1".repeat(64), "0.1.0", "stable", NOW).unwrap(),
        )
        .unwrap();
    let outcomes = host.updater.recover(NOW + 1).unwrap();
    assert!(matches!(outcomes[0], RecoveryOutcome::Abandoned { .. }));
    assert_eq!(
        host.store
            .get(crashed_apply.id.as_str())
            .unwrap()
            .unwrap()
            .status,
        UpdateOpStatus::Failed
    );
}

#[tokio::test]
async fn a_corrupted_staged_artifact_is_refused_before_the_swap() {
    let host = fixture([]);
    let key = keypair();
    let (signed, digest) = signed_manifest(&key, "0.2.0", b"payload", |_| {});
    host.fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"payload",
    );
    host.updater
        .stage(&signed, &components(), None, NOW)
        .await
        .unwrap();
    // Corrupt the published artifact behind the pointer.
    let path = host
        .updater
        .layout()
        .artifact_path("faktor-cli-0.2.0-darwin-arm64.tar.gz", &digest);
    std::fs::write(&path, b"corrupted").unwrap();
    let err = host.updater.apply(NOW + 1).unwrap_err();
    assert_eq!(err.code(), "staged_artifact_unusable");
    assert!(host.updater.layout().read_pointer().unwrap().is_none());
}

#[tokio::test]
async fn the_durable_store_survives_reopen_with_the_full_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("update.db");
    let install_root = dir.path().join("install");
    let key = keypair();
    let fetcher = Arc::new(FakeFetcher::new());
    let (signed, digest) = signed_manifest(&key, "0.2.0", b"payload", |_| {});
    fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
        b"payload",
    );
    {
        let store = Arc::new(faktor_updater::SqliteUpdaterStore::open(&db).unwrap());
        let updater = Updater::with_default_probe(
            UpdaterConfig {
                channel: Channel::Stable,
                install_root: install_root.clone(),
                keys: trusted_keys(&key),
                max_artifact_bytes: 1024 * 1024,
                clock_skew_ms: manifest::DEFAULT_CLOCK_SKEW_MS,
                host_os: OS.into(),
                host_arch: ARCH.into(),
                local_version: "0.1.0".into(),
                allow_legacy_manifests_once: false,
            },
            store,
            fetcher.clone(),
        )
        .unwrap();
        updater
            .stage(&signed, &components(), None, NOW)
            .await
            .unwrap();
        updater.apply(NOW + 1).unwrap();
    }
    // A fresh process sees the same install and the full durable history.
    let store = Arc::new(faktor_updater::SqliteUpdaterStore::open(&db).unwrap());
    let updater = Updater::with_default_probe(
        UpdaterConfig {
            channel: Channel::Stable,
            install_root,
            keys: trusted_keys(&key),
            max_artifact_bytes: 1024 * 1024,
            clock_skew_ms: manifest::DEFAULT_CLOCK_SKEW_MS,
            host_os: OS.into(),
            host_arch: ARCH.into(),
            local_version: "0.1.0".into(),
            allow_legacy_manifests_once: false,
        },
        store.clone(),
        fetcher,
    )
    .unwrap();
    let status = updater.status(NOW + 2).unwrap();
    assert_eq!(status.installed.unwrap().digest, digest);
    assert!(!status.recovery_required);
    let kinds: Vec<&str> = status
        .operations
        .iter()
        .map(|op| op.kind.as_str())
        .collect();
    assert!(kinds.contains(&"check"));
    assert!(kinds.contains(&"stage"));
    assert!(kinds.contains(&"apply"));
    // The store rejects malformed rows on read.
    let bad = UpdateOperation {
        id: faktor_updater::UpdateOpId::try_new("upd_x").unwrap(),
        ..UpdateOperation::new(UpdateOpKind::Check, NOW, None)
    };
    store.insert(&bad).unwrap();
    assert!(matches!(
        store.latest(UpdateOpKind::Check, UpdateOpStatus::Succeeded),
        Ok(Some(_))
    ));
    let _ = UpdateStoreError::Malformed("x".into());
}
