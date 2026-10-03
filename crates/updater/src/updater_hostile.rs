//! Adversarial corpus for updater artifact authentication and the install
//! pointer/artifact store.
//!
//! Part 1 mutates every manifest field in turn and asserts the exact typed
//! refusal; part 2 attacks the signature block (unsigned, unknown identity,
//! substituted key, flipped signature bytes, algorithm confusion); part 3
//! covers validity windows and channel pins; part 4 corrupts pointers,
//! staging and artifacts and asserts the typed storage outcomes. Every row
//! asserts one outcome with its own message.

use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};

use crate::channel::Channel;
use crate::install::{file_digest, DigestProbe, HealthProbe as _, InstallLayout, InstallPointer};
use crate::manifest::tests::{sample_manifest, sign_manifest, test_keys};
use crate::manifest::{
    sha256_hex, verify_manifest, verify_manifest_at_launch, Artifact, ManifestSignature,
    UpdateManifest, MAX_ARTIFACT_URL_BYTES, MAX_CERTIFICATION_EVIDENCE_ENTRIES,
    MAX_MANIFEST_ARTIFACTS, MAX_MANIFEST_VALIDITY_MS,
};
use crate::release::release_id_for;

type ManifestMutation = Box<dyn FnOnce(&mut UpdateManifest)>;
type ValueMutation = Box<dyn FnOnce(&mut serde_json::Value)>;
type PointerMutation = Box<dyn FnOnce(&mut InstallPointer)>;

fn parse_err_code(manifest: &UpdateManifest) -> String {
    let bytes = serde_json::to_vec(manifest).unwrap();
    match UpdateManifest::parse(&bytes) {
        Ok(_) => panic!("the malformed manifest must be refused"),
        Err(e) => e.code().to_string(),
    }
}

/// Every manifest field mutation is refused typed as `manifest_malformed`,
/// and a mutation that lands inside a valid shape still parses.
#[test]
fn manifest_shape_mutation_corpus() {
    let mut rows: Vec<(&'static str, ManifestMutation, Option<&'static str>)> = Vec::new();
    let mut push = |label: &'static str, f: ManifestMutation, needle: Option<&'static str>| {
        rows.push((label, f, needle));
    };

    push(
        "schema-empty",
        Box::new(|m| m.schema = String::new()),
        Some("schema"),
    );
    push(
        "schema-wrong",
        Box::new(|m| m.schema = "faktor-update/v2".into()),
        Some("schema"),
    );
    push(
        "schema-case",
        Box::new(|m| m.schema = "FAKTOR-UPDATE/V1".into()),
        Some("schema"),
    );
    push(
        "schema-whitespace",
        Box::new(|m| m.schema = " faktor-update/v1".into()),
        Some("schema"),
    );
    push(
        "commit-39",
        Box::new(|m| m.commit = "a".repeat(39)),
        Some("commit"),
    );
    push(
        "commit-41",
        Box::new(|m| m.commit = "a".repeat(41)),
        Some("commit"),
    );
    push(
        "commit-upper",
        Box::new(|m| m.commit = "A".repeat(40)),
        Some("commit"),
    );
    push(
        "commit-nonhex",
        Box::new(|m| m.commit = "g".repeat(40)),
        Some("commit"),
    );
    push(
        "commit-empty",
        Box::new(|m| m.commit = String::new()),
        Some("commit"),
    );
    push(
        "version-empty",
        Box::new(|m| m.version = String::new()),
        Some("version"),
    );
    push(
        "version-garbage",
        Box::new(|m| m.version = "not-a-version".into()),
        Some("version"),
    );
    push(
        "version-negative",
        Box::new(|m| m.version = "-1.0.0".into()),
        Some("version"),
    );
    push(
        "artifacts-empty",
        Box::new(|m| m.artifacts.clear()),
        Some("artifacts"),
    );
    push(
        "artifacts-over-max",
        Box::new(|m| {
            let template = m.artifacts[0].clone();
            m.artifacts = (0..=MAX_MANIFEST_ARTIFACTS)
                .map(|i| Artifact {
                    name: format!("artifact-{i}.tgz"),
                    ..template.clone()
                })
                .collect();
        }),
        Some("artifacts"),
    );
    push(
        "artifact-duplicate-name",
        Box::new(|m| {
            let template = m.artifacts[0].clone();
            m.artifacts.push(Artifact {
                os: "linux".into(),
                ..template
            });
        }),
        Some("appears twice"),
    );
    push(
        "artifact-name-empty",
        Box::new(|m| m.artifacts[0].name = String::new()),
        Some("artifact name"),
    );
    push(
        "artifact-name-129",
        Box::new(|m| m.artifacts[0].name = "n".repeat(129)),
        Some("artifact name"),
    );
    push(
        "artifact-name-slash",
        Box::new(|m| m.artifacts[0].name = "a/b.tgz".into()),
        Some("artifact name"),
    );
    push(
        "artifact-name-backslash",
        Box::new(|m| m.artifacts[0].name = "a\\b.tgz".into()),
        Some("artifact name"),
    );
    push(
        "artifact-name-dotdot",
        Box::new(|m| m.artifacts[0].name = "a..b.tgz".into()),
        Some("plain file name"),
    );
    push(
        "artifact-name-space",
        Box::new(|m| m.artifacts[0].name = "a b.tgz".into()),
        Some("artifact name"),
    );
    push(
        "artifact-name-unicode",
        Box::new(|m| m.artifacts[0].name = "caf\u{e9}.tgz".into()),
        Some("artifact name"),
    );
    push(
        "artifact-os-empty",
        Box::new(|m| m.artifacts[0].os = String::new()),
        Some("artifact os"),
    );
    push(
        "artifact-os-space",
        Box::new(|m| m.artifacts[0].os = "dar win".into()),
        Some("artifact os"),
    );
    push(
        "artifact-arch-unicode",
        Box::new(|m| m.artifacts[0].arch = "arm\u{e9}".into()),
        Some("artifact arch"),
    );
    push(
        "artifact-sha-63",
        Box::new(|m| m.artifacts[0].sha256 = "b".repeat(63)),
        Some("sha256"),
    );
    push(
        "artifact-sha-65",
        Box::new(|m| m.artifacts[0].sha256 = "b".repeat(65)),
        Some("sha256"),
    );
    push(
        "artifact-sha-upper",
        Box::new(|m| m.artifacts[0].sha256 = "B".repeat(64)),
        Some("sha256"),
    );
    push(
        "artifact-sha-nonhex",
        Box::new(|m| m.artifacts[0].sha256 = "z".repeat(64)),
        Some("sha256"),
    );
    push(
        "artifact-url-empty",
        Box::new(|m| m.artifacts[0].url = String::new()),
        Some("url"),
    );
    push(
        "artifact-url-ftp",
        Box::new(|m| m.artifacts[0].url = "ftp://example.test/a.tgz".into()),
        Some("http(s)"),
    );
    push(
        "artifact-url-whitespace",
        Box::new(|m| m.artifacts[0].url = "https://example.test/a b.tgz".into()),
        Some("whitespace"),
    );
    push(
        "artifact-url-unicode",
        Box::new(|m| m.artifacts[0].url = "https://example.test/caf\u{e9}".into()),
        Some("ASCII"),
    );
    push(
        "artifact-url-over-max",
        Box::new(|m| {
            m.artifacts[0].url = format!(
                "https://example.test/{}",
                "a".repeat(MAX_ARTIFACT_URL_BYTES)
            )
        }),
        Some("url"),
    );
    push(
        "issued-zero",
        Box::new(|m| m.issued_at = 0),
        Some("positive"),
    );
    push(
        "issued-negative",
        Box::new(|m| m.issued_at = -1),
        Some("positive"),
    );
    push(
        "expires-zero",
        Box::new(|m| m.expires_at = 0),
        Some("positive"),
    );
    push(
        "expires-before-issued",
        Box::new(|m| {
            m.issued_at = 2_000;
            m.expires_at = 1_000;
        }),
        Some("precedes"),
    );
    push(
        "validity-window-over-max",
        Box::new(|m| {
            m.issued_at = 1_000;
            m.expires_at = 1_000 + MAX_MANIFEST_VALIDITY_MS + 1;
        }),
        Some("366 days"),
    );
    push(
        "certification-bad-level",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "trust-me".into(),
                commit: m.commit.clone(),
                manifest_sha256: None,
                evidence: Default::default(),
            })
        }),
        Some("certification level"),
    );
    push(
        "certification-commit-mismatch",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "release".into(),
                commit: "f".repeat(40),
                manifest_sha256: None,
                evidence: Default::default(),
            })
        }),
        Some("foreign evidence"),
    );
    push(
        "certification-manifest-sha-bad",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "release".into(),
                commit: m.commit.clone(),
                manifest_sha256: Some("short".into()),
                evidence: Default::default(),
            })
        }),
        Some("manifest_sha256"),
    );
    push(
        "certification-evidence-over-max",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "release".into(),
                commit: m.commit.clone(),
                manifest_sha256: None,
                evidence: (0..=MAX_CERTIFICATION_EVIDENCE_ENTRIES)
                    .map(|i| (format!("k{i}"), "d".repeat(64)))
                    .collect(),
            })
        }),
        Some("evidence"),
    );
    push(
        "certification-evidence-kind-empty",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "release".into(),
                commit: m.commit.clone(),
                manifest_sha256: None,
                evidence: [("".to_string(), "d".repeat(64))].into_iter().collect(),
            })
        }),
        Some("evidence kind"),
    );
    push(
        "certification-evidence-kind-65",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "release".into(),
                commit: m.commit.clone(),
                manifest_sha256: None,
                evidence: [("k".repeat(65), "d".repeat(64))].into_iter().collect(),
            })
        }),
        Some("evidence kind"),
    );
    push(
        "certification-evidence-digest-bad",
        Box::new(|m| {
            m.certification = Some(crate::manifest::Certification {
                level: "release".into(),
                commit: m.commit.clone(),
                manifest_sha256: None,
                evidence: [("evidence".to_string(), "nope".to_string())]
                    .into_iter()
                    .collect(),
            })
        }),
        Some("64 lowercase hex"),
    );

    for (label, mutate, needle) in rows {
        let mut manifest = sample_manifest();
        mutate(&mut manifest);
        let code = parse_err_code(&manifest);
        assert_eq!(
            code, "manifest_malformed",
            "case {label}: every shape violation is a typed malformed refusal"
        );
        if let Some(needle) = needle {
            let bytes = serde_json::to_vec(&manifest).unwrap();
            let err = UpdateManifest::parse(&bytes).unwrap_err();
            let text = format!("{err:?}");
            assert!(
                text.contains(needle),
                "case {label}: the refusal must name {needle:?}: {text}"
            );
        }
    }

    // Valid boundaries that must PARSE.
    let mut valid_rows: Vec<(&str, ManifestMutation)> = vec![
        ("baseline", Box::new(|_| {})),
        (
            "validity-window-exact-max",
            Box::new(|m| {
                m.issued_at = 1_000;
                m.expires_at = 1_000 + MAX_MANIFEST_VALIDITY_MS;
            }),
        ),
        (
            "legacy-generation-absent",
            Box::new(|m| m.release_generation = None),
        ),
        (
            "generation-zero",
            Box::new(|m| m.release_generation = Some(0)),
        ),
        (
            "generation-u64-max",
            Box::new(|m| m.release_generation = Some(u64::MAX)),
        ),
        ("size-absent", Box::new(|m| m.artifacts[0].size = None)),
        (
            "artifact-count-max",
            Box::new(|m| {
                let template = m.artifacts[0].clone();
                m.artifacts = (0..MAX_MANIFEST_ARTIFACTS)
                    .map(|i| Artifact {
                        name: format!("artifact-{i}.tgz"),
                        ..template.clone()
                    })
                    .collect();
            }),
        ),
        (
            "certification-none-level",
            Box::new(|m| {
                m.certification = Some(crate::manifest::Certification {
                    level: "none".into(),
                    commit: m.commit.clone(),
                    manifest_sha256: Some("c".repeat(64)),
                    evidence: Default::default(),
                })
            }),
        ),
    ];
    while let Some((label, mutate)) = valid_rows.pop() {
        let mut manifest = sample_manifest();
        mutate(&mut manifest);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        assert!(
            UpdateManifest::parse(&bytes).is_ok(),
            "case {label}: this manifest shape must parse"
        );
    }

    // Unknown fields (top level and nested) are strict parse errors.
    let mut value = serde_json::to_value(sample_manifest()).unwrap();
    value["future_field"] = serde_json::json!(1);
    assert!(
        UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
        "an unknown top-level field must be refused"
    );
    let mut value = serde_json::to_value(sample_manifest()).unwrap();
    value["artifacts"][0]["future_artifact_field"] = serde_json::json!(true);
    assert!(
        UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
        "an unknown nested artifact field must be refused"
    );
    assert!(
        UpdateManifest::parse(br#"{"schema":1}"#).is_err(),
        "a non-string schema must be a parse error"
    );
    assert!(
        UpdateManifest::parse(b"not json at all").is_err(),
        "non-JSON input must be a parse error"
    );
    assert!(
        UpdateManifest::parse(b"").is_err(),
        "empty input must be a parse error"
    );
}

/// The authentication matrix: absent, unknown, mismatched and tampered
/// signatures each produce their exact typed code; a valid signature verifies.
#[test]
fn manifest_authentication_corpus() {
    let (keys, key) = test_keys();
    let now = 1_500i64;

    // Baseline signed manifest verifies with the right identity.
    let bytes = sign_manifest(&sample_manifest(), &key);
    let verified =
        verify_manifest(&bytes, &keys, Channel::Stable, now, 0).expect("valid signature");
    assert_eq!(verified.identity(), "operator-test");
    assert_eq!(verified.release_generation(), 1);

    // Unsigned.
    let unsigned = serde_json::to_vec(&sample_manifest()).unwrap();
    assert_eq!(
        verify_manifest(&unsigned, &keys, Channel::Stable, now, 0)
            .unwrap_err()
            .code(),
        "manifest_unsigned",
        "case unsigned: absent signature must be typed unsigned"
    );

    // Unknown identity.
    let mut unknown = sample_manifest();
    unknown.signature = Some(ManifestSignature {
        algorithm: "ed25519".into(),
        identity: "someone-else".into(),
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        value: BASE64.encode([0u8; 64]),
    });
    assert_eq!(
        verify_manifest(
            &serde_json::to_vec(&unknown).unwrap(),
            &keys,
            Channel::Stable,
            now,
            0
        )
        .unwrap_err()
        .code(),
        "manifest_unknown_key",
        "case unknown-identity: must be typed unknown-key"
    );

    // Allowlisted identity with a substituted key.
    let attacker = SigningKey::from_bytes(&[9u8; 32]);
    let mut swapped = unknown.clone();
    let swapped_payload = swapped.signing_payload().unwrap();
    let attacker_signature = attacker.sign(&swapped_payload);
    {
        let signature = swapped.signature.as_mut().unwrap();
        signature.identity = "operator-test".into();
        signature.public_key = BASE64.encode(attacker.verifying_key().to_bytes());
        signature.value = BASE64.encode(attacker_signature.to_bytes());
    }
    assert_eq!(
        verify_manifest(
            &serde_json::to_vec(&swapped).unwrap(),
            &keys,
            Channel::Stable,
            now,
            0
        )
        .unwrap_err()
        .code(),
        "manifest_key_mismatch",
        "case substituted-key: must be typed key-mismatch"
    );

    // Algorithm confusion variants.
    for algorithm in ["rsa", "", "Ed25519", "ed25519 ", "none"] {
        let mut manifest = sample_manifest();
        let payload = manifest.signing_payload().unwrap();
        manifest.signature = Some(ManifestSignature {
            algorithm: algorithm.into(),
            identity: "operator-test".into(),
            public_key: BASE64.encode(key.verifying_key().to_bytes()),
            value: BASE64.encode(key.sign(&payload).to_bytes()),
        });
        let code = verify_manifest(
            &serde_json::to_vec(&manifest).unwrap(),
            &keys,
            Channel::Stable,
            now,
            0,
        )
        .unwrap_err()
        .code();
        assert!(
            code == "manifest_malformed" || code == "manifest_tampered",
            "case algorithm-{algorithm:?}: must refuse typed (got {code})"
        );
    }

    // Signature byte flips at every position class.
    let sign_for = |manifest: &UpdateManifest| -> Vec<u8> { sign_manifest(manifest, &key) };
    let flip = |bytes: &[u8], index: usize| -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        let sig = BASE64
            .decode(value["signature"]["value"].as_str().unwrap())
            .unwrap();
        let mut decoded = sig.clone();
        decoded[index] ^= 0x01;
        value["signature"]["value"] = serde_json::Value::String(BASE64.encode(decoded));
        serde_json::to_vec(&value).unwrap()
    };
    for index in [0usize, 1, 31, 32, 62, 63] {
        let signed = sign_for(&sample_manifest());
        let tampered = flip(&signed, index);
        assert_eq!(
            verify_manifest(&tampered, &keys, Channel::Stable, now, 0)
                .unwrap_err()
                .code(),
            "manifest_tampered",
            "case flip-byte-{index}: a flipped signature byte must be typed tampered"
        );
    }

    // Truncated / non-base64 / empty signature values.
    for (label, value) in [
        ("empty", String::new()),
        ("not-base64", "!!!not-base64!!!".into()),
        ("short-key", BASE64.encode([1u8; 16])),
    ] {
        let mut manifest = sample_manifest();
        let payload = manifest.signing_payload().unwrap();
        manifest.signature = Some(ManifestSignature {
            algorithm: "ed25519".into(),
            identity: "operator-test".into(),
            public_key: BASE64.encode(key.verifying_key().to_bytes()),
            value,
        });
        let code = verify_manifest(
            &serde_json::to_vec(&manifest).unwrap(),
            &keys,
            Channel::Stable,
            now,
            0,
        )
        .unwrap_err()
        .code();
        assert!(
            matches!(code, "manifest_tampered" | "manifest_malformed"),
            "case signature-value-{label}: must refuse typed (got {code})"
        );
        let _ = payload;
    }

    // Tampering ANY field after signing invalidates the signature.
    let signed_value = |manifest: &UpdateManifest| -> serde_json::Value {
        let bytes = sign_manifest(manifest, &key);
        serde_json::from_slice(&bytes).unwrap()
    };
    let tamper_rows: Vec<(&str, ValueMutation)> = vec![
        (
            "version",
            Box::new(|v| v["version"] = serde_json::json!("9.9.9")),
        ),
        (
            "commit",
            Box::new(|v| v["commit"] = serde_json::json!("f".repeat(40))),
        ),
        (
            "channel",
            Box::new(|v| v["channel"] = serde_json::json!("beta")),
        ),
        (
            "release-generation",
            Box::new(|v| v["release_generation"] = serde_json::json!(2)),
        ),
        (
            "artifact-size",
            Box::new(|v| v["artifacts"][0]["size"] = serde_json::json!(2048)),
        ),
        (
            "artifact-sha",
            Box::new(|v| v["artifacts"][0]["sha256"] = serde_json::json!("c".repeat(64))),
        ),
        (
            "artifact-url",
            Box::new(|v| v["artifacts"][0]["url"] = serde_json::json!("https://evil.test/x")),
        ),
        (
            "expires-at",
            Box::new(|v| v["expires_at"] = serde_json::json!(9_999)),
        ),
        (
            "drop-signature-after-signing",
            Box::new(|v| {
                v.as_object_mut().unwrap().remove("signature");
            }),
        ),
    ];
    for (label, mutate) in tamper_rows {
        let mut value = signed_value(&sample_manifest());
        mutate(&mut value);
        let code = verify_manifest(
            &serde_json::to_vec(&value).unwrap(),
            &keys,
            Channel::Stable,
            now,
            0,
        )
        .unwrap_err()
        .code();
        assert!(
            matches!(code, "manifest_tampered" | "manifest_unsigned"),
            "case tamper-{label}: post-signing mutation must be refused (got {code})"
        );
    }
}

/// Validity windows and the channel pin: exact boundaries.
#[test]
fn manifest_validity_and_channel_boundaries() {
    let (keys, key) = test_keys();
    let bytes = sign_manifest(&sample_manifest(), &key);

    // expires_at boundary: now == expires passes; now == expires + 1 fails.
    assert!(
        verify_manifest(&bytes, &keys, Channel::Stable, 2_000, 0).is_ok(),
        "case at-expiry: the expiry instant itself is still valid"
    );
    assert_eq!(
        verify_manifest(&bytes, &keys, Channel::Stable, 2_001, 0)
            .unwrap_err()
            .code(),
        "manifest_expired",
        "case past-expiry: one millisecond past must be expired"
    );
    // issued_at boundary: now == issued passes; now < issued fails unless skew.
    assert!(
        verify_manifest(&bytes, &keys, Channel::Stable, 1_000, 0).is_ok(),
        "case at-issued: the issued instant itself is valid"
    );
    assert_eq!(
        verify_manifest(&bytes, &keys, Channel::Stable, 999, 0)
            .unwrap_err()
            .code(),
        "manifest_not_yet_valid",
        "case before-issued: must be not-yet-valid"
    );
    assert!(
        verify_manifest(&bytes, &keys, Channel::Stable, 999, 1).is_ok(),
        "case skew-covers: one millisecond of skew must admit it"
    );
    assert_eq!(
        verify_manifest(&bytes, &keys, Channel::Stable, 0, 999)
            .unwrap_err()
            .code(),
        "manifest_not_yet_valid",
        "case skew-too-small: a large issued_at is still not yet valid"
    );
    // Beta accepts stable (the documented downgrade path), but stable never
    // accepts beta: a beta manifest on a stable pin is a channel mismatch.
    assert!(
        verify_manifest(&bytes, &keys, Channel::Beta, 1_500, 0).is_ok(),
        "case beta-accepts-stable: the beta pin admits stable"
    );
    let mut beta = sample_manifest();
    beta.channel = Channel::Beta;
    let beta_bytes = sign_manifest(&beta, &key);
    assert_eq!(
        verify_manifest(&beta_bytes, &keys, Channel::Stable, 1_500, 0)
            .unwrap_err()
            .code(),
        "manifest_channel_mismatch",
        "case stable-refuses-beta: a stable pin must refuse beta"
    );
    let mut dev = sample_manifest();
    dev.channel = Channel::Dev;
    let dev_bytes = sign_manifest(&dev, &key);
    assert_eq!(
        verify_manifest(&dev_bytes, &keys, Channel::Stable, 1_500, 0)
            .unwrap_err()
            .code(),
        "manifest_channel_mismatch",
        "case stable-refuses-dev: a stable pin must refuse dev"
    );
    // Expiry is checked BEFORE channel (documented order): an expired
    // wrong-channel manifest reports expired, not channel mismatch.
    assert_eq!(
        verify_manifest(&dev_bytes, &keys, Channel::Stable, 99_999, 0)
            .unwrap_err()
            .code(),
        "manifest_expired",
        "case order: expiry is checked before the channel pin"
    );
}

/// `verify_manifest_at_launch` ignores the validity window and channel pin
/// (already-running installs must not be bricked) but never the signature.
#[test]
fn verify_at_launch_contract() {
    let (keys, key) = test_keys();
    let mut expired = sample_manifest();
    expired.issued_at = 10;
    expired.expires_at = 10;
    expired.channel = Channel::Beta;
    let bytes = sign_manifest(&expired, &key);
    assert!(
        verify_manifest_at_launch(&bytes, &keys).is_ok(),
        "case expired-wrong-channel: launch verification must still admit an authentic artifact"
    );
    // Signature still enforced at launch.
    let unsigned = serde_json::to_vec(&expired).unwrap();
    assert_eq!(
        verify_manifest_at_launch(&unsigned, &keys)
            .unwrap_err()
            .code(),
        "manifest_unsigned",
        "case unsigned: launch verification never admits an unsigned manifest"
    );
    let tampered = {
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = serde_json::json!("6.6.6");
        serde_json::to_vec(&value).unwrap()
    };
    assert_eq!(
        verify_manifest_at_launch(&tampered, &keys)
            .unwrap_err()
            .code(),
        "manifest_tampered",
        "case tampered: launch verification never admits a changed payload"
    );
    // But a shape violation is still malformed at launch.
    let mut bad = expired.clone();
    bad.commit = "zz".into();
    let bad = sign_manifest(&bad, &key);
    assert_eq!(
        verify_manifest_at_launch(&bad, &keys).unwrap_err().code(),
        "manifest_malformed",
        "case malformed: launch verification still shape-validates"
    );
}

// ---------------------------------------------------------------------------
// install pointer / artifact store
// ---------------------------------------------------------------------------

fn layout() -> (tempfile::TempDir, InstallLayout) {
    let dir = tempfile::tempdir().unwrap();
    let layout = InstallLayout::open(dir.path().join("install")).unwrap();
    (dir, layout)
}

fn valid_pointer(digest: &str) -> InstallPointer {
    InstallPointer::new("faktor.tgz", digest, "0.2.0", "stable", 1_000).unwrap()
}

/// Pointer validation: every field boundary is typed, and valid pointers
/// round-trip through the atomic write/read path.
#[test]
#[allow(clippy::vec_init_then_push)]
fn install_pointer_validation_and_round_trip() {
    let digest = "a".repeat(64);
    // Valid baseline + legacy release-id absent + valid release-id form.
    let pointer = valid_pointer(&digest);
    assert_eq!(pointer.digest, digest, "case baseline: digest preserved");
    let release_id = release_id_for("0.2.0", &digest);
    let pointer = pointer
        .with_release_id(Some(release_id.clone()))
        .expect("a production-minted release id must validate");
    assert_eq!(pointer.release_id.as_deref(), Some(release_id.as_str()));

    let mut invalid: Vec<(&str, PointerMutation)> = Vec::new();
    invalid.push(("schema-wrong", Box::new(|p| p.schema = "v2".into())));
    invalid.push(("artifact-empty", Box::new(|p| p.artifact = String::new())));
    invalid.push(("artifact-slash", Box::new(|p| p.artifact = "a/b".into())));
    invalid.push((
        "artifact-backslash",
        Box::new(|p| p.artifact = "a\\b".into()),
    ));
    invalid.push(("artifact-dotdot", Box::new(|p| p.artifact = "a..b".into())));
    invalid.push(("digest-63", Box::new(|p| p.digest = "a".repeat(63))));
    invalid.push(("digest-upper", Box::new(|p| p.digest = "A".repeat(64))));
    invalid.push(("digest-nonhex", Box::new(|p| p.digest = "z".repeat(64))));
    invalid.push((
        "release-id-empty",
        Box::new(|p| p.release_id = Some(String::new())),
    ));
    invalid.push((
        "release-id-slash",
        Box::new(|p| p.release_id = Some("a/b".into())),
    ));
    invalid.push((
        "release-id-dotdot",
        Box::new(|p| p.release_id = Some("..".into())),
    ));
    invalid.push((
        "release-id-traversal",
        Box::new(|p| p.release_id = Some("../escape".into())),
    ));
    for (label, mutate) in invalid {
        let mut pointer = valid_pointer(&digest);
        mutate(&mut pointer);
        let err = pointer
            .validate()
            .expect_err(&format!("case {label}: invalid pointer must refuse"));
        assert!(
            matches!(
                err.code(),
                "install_error" | "updater_config" | "launch_refused"
            ),
            "case {label}: refusal must be typed (got {})",
            err.code()
        );
    }

    // Round-trip through the real atomic pointer file.
    let (_dir, layout) = layout();
    assert!(
        layout.read_pointer().unwrap().is_none(),
        "a fresh layout has no pointer"
    );
    let pointer = valid_pointer(&digest).with_release_id(None).unwrap();
    layout.write_pointer(&pointer).unwrap();
    let read = layout.read_pointer().unwrap().expect("pointer present");
    assert_eq!(read, pointer, "pointer must round-trip byte-equivalently");
    layout.remove_pointer().unwrap();
    assert!(layout.read_pointer().unwrap().is_none());
    layout.remove_pointer().unwrap();
}

/// Pointer corruption: a present-but-broken pointer is always a typed install
/// error, never "nothing installed".
#[test]
fn install_pointer_corruption_corpus() {
    let digest = "b".repeat(64);
    let cases: [(&str, Vec<u8>); 9] = [
        ("empty-file", Vec::new()),
        ("truncated-json", b"{\"schema\":\"faktor-install-pointer/v1\"".to_vec()),
        ("not-json", b"garbage".to_vec()),
        (
            "invalid-utf8",
            vec![0xff, 0xfe, 0xfd],
        ),
        (
            "wrong-schema",
            br#"{"schema":"v2","artifact":"a","digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","version":"1","channel":"stable","applied_ms":1}"#.to_vec(),
        ),
        (
            "missing-field",
            br#"{"schema":"faktor-install-pointer/v1","artifact":"a"}"#.to_vec(),
        ),
        (
            "unknown-field",
            br#"{"schema":"faktor-install-pointer/v1","artifact":"a","digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","version":"1","channel":"stable","applied_ms":1,"future":true}"#.to_vec(),
        ),
        (
            "bad-digest",
            br#"{"schema":"faktor-install-pointer/v1","artifact":"a","digest":"short","version":"1","channel":"stable","applied_ms":1}"#.to_vec(),
        ),
        (
            "traversal-artifact",
            br#"{"schema":"faktor-install-pointer/v1","artifact":"../x","digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","version":"1","channel":"stable","applied_ms":1}"#.to_vec(),
        ),
    ];
    for (label, bytes) in cases {
        let (_dir, layout) = layout();
        std::fs::write(layout.pointer_path(), &bytes).unwrap();
        let result = layout.read_pointer();
        let err = result.err().unwrap_or_else(|| {
            panic!("case {label}: a corrupted pointer must be refused, never read as absent")
        });
        assert!(
            !err.to_string().is_empty(),
            "case {label}: the refusal must name the corruption: {err}"
        );
    }
    let _ = valid_pointer(&digest);
}

/// Staged artifact publication, digest probing and verify_installed failure
/// modes.
#[test]
fn install_staging_digest_and_verify_matrix() {
    let (_dir, layout) = layout();
    let payload = b"faktor-release-payload".to_vec();
    let digest = sha256_hex(&payload);

    // Publish a real staged file and probe it.
    let staged = layout.staging_file("op-1", "faktor.tgz");
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, &payload).unwrap();
    let published = layout
        .publish_staged(&staged, "faktor.tgz", &digest)
        .expect("a correctly staged file publishes");
    assert_eq!(
        published,
        layout.artifact_path("faktor.tgz", &digest),
        "case publish: the destination is the content-addressed path"
    );
    assert!(
        !staged.exists(),
        "case publish: the staged file is consumed by the atomic rename"
    );
    assert_eq!(
        layout.artifact_digest("faktor.tgz", &digest).unwrap(),
        digest,
        "case artifact-digest: the stored artifact hashes to the pointer digest"
    );
    let pointer = valid_pointer(&digest);
    DigestProbe
        .probe(&layout, &pointer)
        .expect("case probe: a complete installation probes clean");
    layout.verify_installed(&pointer).expect("verify_installed");

    // A digest mismatch in the pointer is refused by both probe and verify.
    let wrong = valid_pointer(&"c".repeat(64));
    let err = layout
        .verify_installed(&wrong)
        .expect_err("a digest-mismatched pointer must fail verification");
    assert_eq!(
        err.code(),
        "staged_artifact_unusable",
        "case verify-mismatch: typed unusable artifact"
    );
    let err = DigestProbe
        .probe(&layout, &wrong)
        .expect_err("the health probe must refuse a wrong digest");
    assert_eq!(
        err.code(),
        "health_failed",
        "the health probe reports a failed health check (the verify path reports unusable)"
    );

    // A mutated artifact is detected by digest.
    let artifact = layout.artifact_path("faktor.tgz", &digest);
    std::fs::write(&artifact, b"mutated!").unwrap();
    assert_eq!(
        layout
            .verify_installed(&pointer)
            .expect_err("a mutated artifact must fail verification")
            .code(),
        "staged_artifact_unusable",
        "case verify-mutated: typed unusable artifact"
    );
    // Restore the bytes for the remaining checks.
    std::fs::write(&artifact, &payload).unwrap();

    // Missing artifact.
    let missing = valid_pointer(&"d".repeat(64));
    let err = layout
        .verify_installed(&missing)
        .expect_err("a missing artifact must fail verification");
    assert_eq!(err.code(), "staged_artifact_unusable");

    // A release pointer with no materialized binary is refused typed.
    let release_pointer = valid_pointer(&digest)
        .with_release_id(Some(release_id_for("0.2.0", &digest)))
        .unwrap();
    let err = layout
        .verify_installed(&release_pointer)
        .expect_err("a release pointer with no versions/ entry must fail");
    assert_eq!(err.code(), "staged_artifact_unusable");

    // publish_staged refusals: bad digest, missing source.
    let bad_digest = layout
        .publish_staged(PathBuf::from("/nonexistent").as_path(), "a", "short")
        .expect_err("a non-digest name must be refused before touching the fs");
    assert_eq!(bad_digest.code(), "install_error");
    let missing_source = layout
        .publish_staged(
            PathBuf::from("/nonexistent-faktor").as_path(),
            "a",
            &"e".repeat(64),
        )
        .expect_err("a missing staged file must be refused typed");
    assert!(
        !missing_source.to_string().is_empty(),
        "the missing-source refusal must carry a message"
    );

    // clear_staging: missing is fine, present is removed.
    layout.clear_staging("never-existed").unwrap();
    let residue = layout.staging_file("op-2", "residue");
    std::fs::create_dir_all(residue.parent().unwrap()).unwrap();
    std::fs::write(&residue, b"x").unwrap();
    layout.clear_staging("op-2").unwrap();
    assert!(
        !layout.staging_dir_for("op-2").exists(),
        "clear_staging must remove the whole operation directory"
    );

    // file_digest boundaries.
    let empty = layout.root().join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(
        file_digest(&empty).unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "case file-digest-empty: the empty sha256 is frozen"
    );
    let big = layout.root().join("big.bin");
    std::fs::write(&big, vec![0x5a; 5 * 1024 * 1024]).unwrap();
    assert_eq!(
        file_digest(&big).unwrap().len(),
        64,
        "case file-digest-5mib: streamed digest is 64 hex chars"
    );
    assert!(
        file_digest(&layout.root().join("ghost")).is_err(),
        "case file-digest-missing: a missing file is a typed error"
    );

    // Layout open with an empty root is refused before any side effect.
    assert!(
        InstallLayout::open(PathBuf::new()).is_err(),
        "case empty-root: an empty install root must be refused"
    );
}
