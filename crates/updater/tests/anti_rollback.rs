//! Adversarial tests for the authenticated anti-rollback floor: the signed
//! `release_generation`, the durable per-channel high-water mark, the typed
//! refusals for older/legacy manifests, the separately authorized downgrade
//! (the only path below the floor) and the crash/reopen behavior of every
//! one of them.
//!
//! No network and no real install are touched: the harness injects a fake
//! transport, a scripted health probe and (for the durability tests) a real
//! SQLite updater store in a temp dir.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use faktor_updater::{
    manifest, ApplyOutcome, Artifact, Channel, Compatibility, InstallLayout, InstallPointer,
    MemoryUpdaterStore, RecoveryOutcome, RunningComponents, SqliteUpdaterStore, TrustedKey,
    TrustedKeys, UpdateError, UpdateOpKind, UpdateOpStatus, UpdateOperation, Updater,
    UpdaterConfig, UpdaterStore, VersionRange,
};

// ------------------------------------------------------------- test harness

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

fn components() -> RunningComponents {
    RunningComponents::from_strs(Some("0.1.0"), Some("0.1.0"), None, None, Some(1)).unwrap()
}

/// One signed manifest for the fake transport: `generation` is the signed
/// anti-rollback counter (`None` = a legacy manifest, generation 0). The
/// artifact payload is served by the fake fetcher under the manifest URL.
fn signed_manifest(
    fetcher: &FakeFetcher,
    key: &SigningKey,
    version: &str,
    generation: Option<u64>,
    payload: &[u8],
) -> (Vec<u8>, String) {
    let digest = manifest::sha256_hex(payload);
    let name = format!("faktor-cli-{version}-{OS}-{ARCH}.tar.gz");
    let url = format!("https://mirror.test/{version}/{name}");
    fetcher.serve(&url, payload);
    let mut doc = manifest::UpdateManifest {
        schema: manifest::UPDATE_MANIFEST_SCHEMA.to_string(),
        channel: Channel::Stable,
        version: version.to_string(),
        commit: format!("{version:0>40}").replace('.', "a"),
        release_generation: generation,
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

fn config(
    install_root: std::path::PathBuf,
    keys: TrustedKeys,
    allow_legacy: bool,
) -> UpdaterConfig {
    UpdaterConfig {
        channel: Channel::Stable,
        install_root,
        keys,
        max_artifact_bytes: 1024 * 1024,
        clock_skew_ms: manifest::DEFAULT_CLOCK_SKEW_MS,
        host_os: OS.into(),
        host_arch: ARCH.into(),
        local_version: "0.1.0".into(),
        allow_legacy_manifests_once: allow_legacy,
    }
}

struct MemoryFixture {
    _dir: tempfile::TempDir,
    updater: Arc<Updater>,
    fetcher: Arc<FakeFetcher>,
    store: Arc<MemoryUpdaterStore>,
    key: SigningKey,
}

fn memory_fixture(
    allow_legacy: bool,
    probe_results: impl IntoIterator<Item = Result<(), String>>,
) -> MemoryFixture {
    let dir = tempfile::tempdir().unwrap();
    let key = keypair();
    let fetcher = Arc::new(FakeFetcher::new());
    let store = Arc::new(MemoryUpdaterStore::new());
    let probe = Arc::new(ScriptedProbe::scripted(probe_results));
    let updater = Arc::new(
        Updater::new(
            config(dir.path().join("install"), trusted_keys(&key), allow_legacy),
            store.clone(),
            fetcher.clone(),
            probe,
        )
        .unwrap(),
    );
    MemoryFixture {
        _dir: dir,
        updater,
        fetcher,
        store,
        key,
    }
}

async fn stage_and_apply(
    fixture: &MemoryFixture,
    version: &str,
    generation: u64,
    payload: &[u8],
    now_ms: i64,
) -> String {
    let (signed, digest) = signed_manifest(
        &fixture.fetcher,
        &fixture.key,
        version,
        Some(generation),
        payload,
    );
    fixture
        .updater
        .stage(&signed, &components(), None, now_ms)
        .await
        .unwrap();
    let outcome = fixture.updater.apply(now_ms + 1).unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );
    digest
}

fn floor(updater: &Updater, channel: &str) -> Option<u64> {
    updater
        .store()
        .high_water(channel)
        .unwrap()
        .map(|mark| mark.generation)
}

// ------------------------------------------------------- floor & durability

#[tokio::test]
async fn admission_raises_the_durable_floor_and_a_reopen_cannot_lower_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("update.db");
    let install_root = dir.path().join("install");
    let key = keypair();
    let fetcher = Arc::new(FakeFetcher::new());
    let (signed, digest) = signed_manifest(&fetcher, &key, "5.0.0", Some(5), b"gen5");
    fetcher.serve(
        "https://mirror.test/5.0.0/faktor-cli-5.0.0-darwin-arm64.tar.gz",
        b"gen5",
    );

    // Admit generation 5 and "crash" before activation completes: the floor
    // is already durable (record-first).
    {
        let store = Arc::new(SqliteUpdaterStore::open(&db).unwrap());
        let updater = Updater::with_default_probe(
            config(install_root.clone(), trusted_keys(&key), false),
            store,
            fetcher.clone(),
        )
        .unwrap();
        let staged = updater
            .stage(&signed, &components(), None, NOW)
            .await
            .unwrap();
        assert_eq!(staged.release_generation, 5);
        assert_eq!(floor(&updater, "stable"), Some(5));
        assert!(
            updater.layout().read_pointer().unwrap().is_none(),
            "admission happened without any activation"
        );
    }

    // A fresh process (new SQLite connection) sees the same floor.
    let store = Arc::new(SqliteUpdaterStore::open(&db).unwrap());
    let updater = Updater::with_default_probe(
        config(install_root, trusted_keys(&key), false),
        store.clone(),
        fetcher.clone(),
    )
    .unwrap();
    assert_eq!(
        floor(&updater, "stable"),
        Some(5),
        "the durable floor survives a reopen"
    );
    let status = updater.status(NOW + 1).unwrap();
    assert_eq!(
        status.high_water.as_ref().map(|mark| mark.generation),
        Some(5)
    );
    assert_eq!(
        status.high_water.as_ref().map(|mark| mark.legacy_consumed),
        Some(false)
    );

    // An older signed manifest is refused typed; the equal generation is
    // still admissible (a re-stage of the admitted release, idempotent by
    // key).
    let (older, _) = signed_manifest(&fetcher, &key, "4.0.0", Some(4), b"gen4");
    let err = updater.check(&older, &components(), NOW + 2).unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::RollbackRefused {
                high_water: 5,
                offered: 4,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(err.code(), "rollback_refused");
    assert_eq!(floor(&updater, "stable"), Some(5));
    assert!(updater.check(&signed, &components(), NOW + 3).is_ok());

    // A crash that happens after the store is reopened still cannot lower
    // the floor: `set_high_water` is the only lowering path and no normal
    // operation calls it.
    assert_eq!(store.high_water("stable").unwrap().unwrap().generation, 5);
    assert_eq!(digest, manifest::sha256_hex(b"gen5"));
}

#[tokio::test]
async fn an_older_signed_manifest_is_refused_typed_without_staging_or_apply() {
    let fixture = memory_fixture(false, []);
    let v3_digest = stage_and_apply(&fixture, "3.0.0", 3, b"v3", NOW).await;
    let downloads_after_install = fixture.fetcher.calls().len();

    let (older, older_digest) =
        signed_manifest(&fixture.fetcher, &fixture.key, "2.0.0", Some(2), b"v2");
    let err = fixture
        .updater
        .check(&older, &components(), NOW + 2)
        .unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::RollbackRefused {
                high_water: 3,
                offered: 2,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains('3'), "{err}");
    assert!(err.to_string().contains('2'), "{err}");

    let err = fixture
        .updater
        .stage(&older, &components(), Some("k-old"), NOW + 3)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "rollback_refused");
    // No download happened, no artifact was published, no stage row exists
    // for the refused version and the install is untouched.
    assert_eq!(fixture.fetcher.calls().len(), downloads_after_install);
    assert!(!fixture
        .updater
        .layout()
        .artifact_path("faktor-cli-2.0.0-darwin-arm64.tar.gz", &older_digest)
        .exists());
    let stages: Vec<UpdateOperation> = fixture
        .store
        .list(64)
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == UpdateOpKind::Stage)
        .collect();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].after_version.as_deref(), Some("3.0.0"));
    assert_eq!(
        fixture
            .updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v3_digest
    );
    // The refusal is durable evidence (a failed check row) and the floor is
    // still 3.
    assert!(fixture.store.list(64).unwrap().iter().any(|op| {
        op.kind == UpdateOpKind::Check
            && op.status == UpdateOpStatus::Failed
            && op.release_generation == Some(2)
    }));
    assert_eq!(floor(&fixture.updater, "stable"), Some(3));
}

#[tokio::test]
async fn apply_refuses_when_the_floor_moves_past_the_staged_generation() {
    let fixture = memory_fixture(false, []);
    let (signed, _) = signed_manifest(&fixture.fetcher, &fixture.key, "2.0.0", Some(2), b"v2");
    fixture
        .updater
        .stage(&signed, &components(), None, NOW)
        .await
        .unwrap();
    // A higher floor appears after the stage (e.g. a concurrent admission or
    // a rewritten row): the swap must refuse, not silently install below the
    // floor.
    fixture
        .store
        .raise_high_water("stable", 7, false, NOW + 1)
        .unwrap();
    let err = fixture.updater.apply(NOW + 2).unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::RollbackRefused {
                high_water: 7,
                offered: 2,
                ..
            }
        ),
        "{err}"
    );
    assert!(fixture.updater.layout().read_pointer().unwrap().is_none());
}

// -------------------------------------------------------- authorized downgrade

#[tokio::test]
async fn the_authorized_downgrade_resets_the_floor_and_leaves_a_durable_audit_row() {
    let fixture = memory_fixture(false, []);
    let v3_digest = stage_and_apply(&fixture, "3.0.0", 3, b"v3", NOW).await;

    let (v1, v1_digest) = signed_manifest(&fixture.fetcher, &fixture.key, "1.0.0", Some(1), b"v1");
    let outcome = fixture
        .updater
        .downgrade(&v1, &components(), Some("k-dg"), Some("user:ops"), NOW + 2)
        .await
        .unwrap();
    let ApplyOutcome::Applied {
        version, digest, ..
    } = outcome
    else {
        panic!("expected an applied downgrade, got {outcome:?}");
    };
    assert_eq!(version, "1.0.0");
    assert_eq!(digest, v1_digest);
    let pointer = fixture.updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, v1_digest);
    assert_eq!(pointer.version, "1.0.0");

    // The floor is reset to the downgraded generation (the only lowering
    // path) and the audit row names the actor, both generations and the
    // signed generation.
    assert_eq!(floor(&fixture.updater, "stable"), Some(1));
    let audits: Vec<UpdateOperation> = fixture
        .store
        .list(64)
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == UpdateOpKind::Downgrade)
        .collect();
    assert_eq!(audits.len(), 1);
    let audit = &audits[0];
    assert_eq!(audit.status, UpdateOpStatus::Applied);
    assert_eq!(audit.actor.as_deref(), Some("user:ops"));
    assert_eq!(audit.release_generation, Some(1));
    assert_eq!(audit.before_digest.as_deref(), Some(v3_digest.as_str()));
    assert_eq!(audit.after_digest.as_deref(), Some(v1_digest.as_str()));
    assert_eq!(audit.idempotency_key.as_deref(), Some("k-dg"));

    // Re-admitting generation 1 is fine; generation 0 is below the new floor
    // and a legacy target can never authorize itself.
    assert!(fixture.updater.check(&v1, &components(), NOW + 3).is_ok());
    let (v0, _) = signed_manifest(&fixture.fetcher, &fixture.key, "0.9.0", Some(0), b"v0");
    assert_eq!(
        fixture
            .updater
            .check(&v0, &components(), NOW + 4)
            .unwrap_err()
            .code(),
        "rollback_refused"
    );
    let (legacy, _) = signed_manifest(&fixture.fetcher, &fixture.key, "0.1.0", None, b"legacy");
    let err = fixture
        .updater
        .downgrade(&legacy, &components(), None, Some("user:ops"), NOW + 5)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "legacy_manifest_refused");
    assert_eq!(floor(&fixture.updater, "stable"), Some(1));
}

#[tokio::test]
async fn a_failed_downgrade_keeps_the_floor_and_restores_the_previous_pointer() {
    // Probe script: the v3 apply passes, the downgrade swap fails its health
    // probe; the automatic restore probe passes.
    let fixture = memory_fixture(
        false,
        [Ok(()), Err("cannot launch older release".into()), Ok(())],
    );
    let v3_digest = stage_and_apply(&fixture, "3.0.0", 3, b"v3", NOW).await;
    let (v1, v1_digest) = signed_manifest(&fixture.fetcher, &fixture.key, "1.0.0", Some(1), b"v1");
    let outcome = fixture
        .updater
        .downgrade(&v1, &components(), None, Some("user:ops"), NOW + 2)
        .await
        .unwrap();
    let ApplyOutcome::RolledBack { digest, .. } = outcome else {
        panic!("expected a rolled-back downgrade, got {outcome:?}");
    };
    assert_eq!(digest, v1_digest);
    // The previous install is restored EXACTLY and the floor never moved.
    let pointer = fixture.updater.layout().read_pointer().unwrap().unwrap();
    assert_eq!(pointer.digest, v3_digest);
    assert_eq!(pointer.version, "3.0.0");
    assert_eq!(floor(&fixture.updater, "stable"), Some(3));
    let audits: Vec<UpdateOperation> = fixture
        .store
        .list(64)
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == UpdateOpKind::Downgrade)
        .collect();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].status, UpdateOpStatus::RolledBack);
}

#[tokio::test]
async fn an_incompatible_downgrade_target_is_refused_and_never_touches_the_floor() {
    let fixture = memory_fixture(false, []);
    stage_and_apply(&fixture, "3.0.0", 3, b"v3", NOW).await;
    let (mut target, _) = signed_manifest(&fixture.fetcher, &fixture.key, "1.0.0", Some(1), b"v1");
    // Re-sign an incompatible variant of the older manifest.
    {
        let mut doc = manifest::UpdateManifest::parse(&target).unwrap();
        doc.compatibility.cli = range("9.0.0", "9.9.9");
        doc.signature = None;
        let payload = doc.signing_payload().unwrap();
        doc.signature = Some(manifest::ManifestSignature {
            algorithm: "ed25519".into(),
            identity: "operator".into(),
            public_key: BASE64.encode(fixture.key.verifying_key().to_bytes()),
            value: BASE64.encode(fixture.key.sign(&payload).to_bytes()),
        });
        target = serde_json::to_vec(&doc).unwrap();
    }
    let err = fixture
        .updater
        .downgrade(&target, &components(), None, Some("user:ops"), NOW + 2)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "incompatible");
    assert_eq!(floor(&fixture.updater, "stable"), Some(3));
    assert_eq!(
        fixture
            .updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .version,
        "3.0.0"
    );
}

// --------------------------------------------------------------- legacy rule

#[tokio::test]
async fn legacy_manifests_follow_the_documented_one_time_rule() {
    // Default (allowance off): a legacy manifest is refused typed and never
    // downloaded.
    let strict = memory_fixture(false, []);
    let (legacy, _legacy_digest) =
        signed_manifest(&strict.fetcher, &strict.key, "0.2.0", None, b"legacy");
    let err = strict
        .updater
        .check(&legacy, &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "legacy_manifest_refused");
    let err = strict
        .updater
        .stage(&legacy, &components(), None, NOW)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "legacy_manifest_refused");
    assert!(strict.fetcher.calls().is_empty());
    assert_eq!(floor(&strict.updater, "stable"), None);

    // Allowance on with no floor: admitted EXACTLY once (the allowance is
    // consumed durably by the first admission).
    let lenient = memory_fixture(true, []);
    let (legacy, legacy_digest) =
        signed_manifest(&lenient.fetcher, &lenient.key, "0.2.0", None, b"legacy");
    let check = lenient.updater.check(&legacy, &components(), NOW).unwrap();
    assert_eq!(check.release_generation, 0);
    let staged = lenient
        .updater
        .stage(&legacy, &components(), None, NOW + 1)
        .await
        .unwrap();
    assert_eq!(staged.release_generation, 0);
    assert_eq!(floor(&lenient.updater, "stable"), Some(0));
    assert!(
        lenient
            .store
            .high_water("stable")
            .unwrap()
            .unwrap()
            .legacy_consumed,
        "the allowance is consumed durably"
    );
    // The second legacy attempt (even with a different idempotency key) is
    // refused: the documented allowance is a one-time migration.
    let err = lenient
        .updater
        .check(&legacy, &components(), NOW + 2)
        .unwrap_err();
    assert_eq!(err.code(), "legacy_manifest_refused");
    assert!(err.to_string().contains("consumed"), "{err}");
    let (other_legacy, _) =
        signed_manifest(&lenient.fetcher, &lenient.key, "0.3.0", None, b"legacy-2");
    let err = lenient
        .updater
        .stage(&other_legacy, &components(), None, NOW + 3)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "legacy_manifest_refused");

    // A legacy manifest offered while a higher floor exists is a ROLLBACK
    // refusal, not a legacy refusal (the floor names both generations).
    let floored = memory_fixture(true, []);
    stage_and_apply(&floored, "2.0.0", 2, b"v2", NOW).await;
    let err = floored
        .updater
        .check(&legacy, &components(), NOW + 2)
        .unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::RollbackRefused {
                high_water: 2,
                offered: 0,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(floor(&floored.updater, "stable"), Some(2));
    assert_eq!(legacy_digest, manifest::sha256_hex(b"legacy"));

    // A downgrade target must carry an authenticated generation.
    let (legacy_target, _) = signed_manifest(&floored.fetcher, &floored.key, "1.0.0", None, b"old");
    let err = floored
        .updater
        .downgrade(
            &legacy_target,
            &components(),
            None,
            Some("user:ops"),
            NOW + 3,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "legacy_manifest_refused");
}

// --------------------------------------------------------- tamper & recovery

#[tokio::test]
async fn a_tampered_or_stripped_generation_breaks_the_signature() {
    let fixture = memory_fixture(false, []);
    let (signed, _) = signed_manifest(&fixture.fetcher, &fixture.key, "2.0.0", Some(2), b"v2");

    // Lower the signed generation after signing: the floor must not see a
    // "generation 1" manifest at all, only a tamper refusal.
    let mut lowered: serde_json::Value = serde_json::from_slice(&signed).unwrap();
    lowered["release_generation"] = serde_json::json!(1);
    let err = fixture
        .updater
        .check(&serde_json::to_vec(&lowered).unwrap(), &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_tampered");
    assert_eq!(floor(&fixture.updater, "stable"), None);

    // Strip it back to "legacy" to try to dodge the floor: same refusal.
    let mut stripped: serde_json::Value = serde_json::from_slice(&signed).unwrap();
    stripped
        .as_object_mut()
        .unwrap()
        .remove("release_generation");
    let err = fixture
        .updater
        .check(&serde_json::to_vec(&stripped).unwrap(), &components(), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "manifest_tampered");
    assert_eq!(floor(&fixture.updater, "stable"), None);

    // The authentic manifest still admits and raises the floor.
    fixture
        .updater
        .stage(&signed, &components(), None, NOW)
        .await
        .unwrap();
    assert_eq!(floor(&fixture.updater, "stable"), Some(2));
}

#[tokio::test]
async fn recovery_of_an_interrupted_downgrade_completes_the_floor_reset() {
    let fixture = memory_fixture(false, []);
    let v3_digest = stage_and_apply(&fixture, "3.0.0", 3, b"v3", NOW).await;
    let old_digest = "b".repeat(64);

    // Simulate the crash window: the downgrade row exists in `running`, the
    // pointer was already swapped to the older release, the floor was NOT
    // reset yet.
    let mut crashed = UpdateOperation::new(UpdateOpKind::Downgrade, NOW + 1, None);
    crashed.channel = Some("stable".into());
    crashed.before_version = Some("3.0.0".into());
    crashed.before_digest = Some(v3_digest.clone());
    crashed.before_artifact = Some("faktor-cli-3.0.0-darwin-arm64.tar.gz".into());
    crashed.after_version = Some("1.0.0".into());
    crashed.after_digest = Some(old_digest.clone());
    crashed.artifact = Some("faktor-cli-1.0.0-darwin-arm64.tar.gz".into());
    crashed.release_generation = Some(1);
    crashed.actor = Some("user:ops".into());
    fixture.store.insert(&crashed.clone()).unwrap();
    fixture
        .updater
        .layout()
        .write_pointer(
            &InstallPointer::new(
                "faktor-cli-1.0.0-darwin-arm64.tar.gz",
                &old_digest,
                "1.0.0",
                "stable",
                NOW + 2,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(floor(&fixture.updater, "stable"), Some(3));

    let outcomes = fixture.updater.recover(NOW + 3).unwrap();
    assert!(
        matches!(outcomes[0], RecoveryOutcome::Resumed { .. }),
        "{outcomes:?}"
    );
    // The resumed downgrade completes its deferred floor reset.
    assert_eq!(floor(&fixture.updater, "stable"), Some(1));
    assert_eq!(
        fixture
            .store
            .get(crashed.id.as_str())
            .unwrap()
            .unwrap()
            .status,
        UpdateOpStatus::Applied
    );

    // If the resumed probe fails, the previous release is restored and the
    // floor stays high (fail closed). Probe script: the v3 apply passes, the
    // interrupted downgrade fails.
    let failing = memory_fixture(false, [Ok(()), Err("older release will not run".into())]);
    let v3 = stage_and_apply(&failing, "3.0.0", 3, b"v3", NOW).await;
    let mut crashed = UpdateOperation::new(UpdateOpKind::Downgrade, NOW + 1, None);
    crashed.channel = Some("stable".into());
    crashed.before_version = Some("3.0.0".into());
    crashed.before_digest = Some(v3.clone());
    crashed.before_artifact = Some("faktor-cli-3.0.0-darwin-arm64.tar.gz".into());
    crashed.after_version = Some("1.0.0".into());
    crashed.after_digest = Some(old_digest.clone());
    crashed.artifact = Some("faktor-cli-1.0.0-darwin-arm64.tar.gz".into());
    crashed.release_generation = Some(1);
    failing.store.insert(&crashed).unwrap();
    failing
        .updater
        .layout()
        .write_pointer(
            &InstallPointer::new(
                "faktor-cli-1.0.0-darwin-arm64.tar.gz",
                &old_digest,
                "1.0.0",
                "stable",
                NOW + 2,
            )
            .unwrap(),
        )
        .unwrap();
    let outcomes = failing.updater.recover(NOW + 3).unwrap();
    assert!(
        matches!(outcomes[0], RecoveryOutcome::RolledBack { .. }),
        "{outcomes:?}"
    );
    assert_eq!(floor(&failing.updater, "stable"), Some(3));
    assert_eq!(
        failing
            .updater
            .layout()
            .read_pointer()
            .unwrap()
            .unwrap()
            .digest,
        v3
    );
}
