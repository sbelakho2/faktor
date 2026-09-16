//! Cross-language interop vector: a manifest SIGNED BY
//! `scripts/update-manifest.mjs` (node, operator seed 0x07..) must verify in
//! Rust, and the Rust canonical signing payload must be byte-identical to
//! node's (pinned by the payload sha256 below).
//!
//! Regenerate the vector with:
//! ```text
//! FAKTOR_UPDATE_SIGNING_KEY=0707..07 node scripts/update-manifest.mjs \
//!   --artifacts <artifacts.json> --commit <HEAD> --key-id operator-test \
//!   --url-base https://mirror.test/faktor --out <out>
//! ```
//! then update the JSON, the payload digest and `issued_at`/`expires_at`
//! bounds in the test below (the vector is a frozen release-script output).

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use faktor_updater::{verify_manifest, Channel, TrustedKey, TrustedKeys};

/// The node-produced signature's canonical-payload sha256 — the byte-level
/// interop pin between `scripts/update-manifest.mjs` and
/// `crates/updater/src/manifest.rs`.
const NODE_PAYLOAD_SHA256: &str =
    "3b70097087d93e73818bb19cf9735fb6a9bc9279a8530df0ad21013d8923e3b5";

const VECTOR: &str = r#"{
  "schema": "faktor-update/v1",
  "channel": "stable",
  "version": "0.2.0",
  "commit": "e8ea392e7695d70c068c08dedb631914679a3398",
  "artifacts": [
    {
      "name": "faktor-0.2.0.vsix",
      "os": "any",
      "arch": "any",
      "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "url": "https://mirror.test/faktor/0.2.0/faktor-0.2.0.vsix",
      "size": 512
    },
    {
      "name": "faktor-cli-0.2.0-darwin-arm64.tar.gz",
      "os": "darwin",
      "arch": "arm64",
      "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "url": "https://mirror.test/faktor/0.2.0/faktor-cli-0.2.0-darwin-arm64.tar.gz",
      "size": 4096
    }
  ],
  "compatibility": {
    "cli": { "min": "0.0.0", "max": "0.2.0" },
    "daemon": { "min": "0.0.0", "max": "0.2.0" },
    "vscode": { "min": "*", "max": "*" },
    "jetbrains": { "min": "*", "max": "*" },
    "schema": { "min": 1, "max": 1 }
  },
  "issued_at": 1789567572457,
  "expires_at": 1792159572457,
  "signature": {
    "algorithm": "ed25519",
    "identity": "operator-test",
    "public_key": "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=",
    "value": "Ok3fwWwmVTHn4j1kPBaNP23ud2uh73wdR57BR05X693wSaBt/AwzRJLuYYJOxX/vRqY9zxXHlq/5BvMAWwSkDw=="
  }
}"#;

fn keys() -> TrustedKeys {
    let manifest = faktor_updater::UpdateManifest::parse(VECTOR.as_bytes()).unwrap();
    TrustedKeys::new(vec![TrustedKey::from_base64(
        "operator-test",
        &manifest.signature.unwrap().public_key,
    )
    .unwrap()])
    .unwrap()
}

#[test]
fn a_node_signed_manifest_verifies_in_rust() {
    let keys = keys();
    let verified = verify_manifest(VECTOR.as_bytes(), &keys, Channel::Stable, 1789567580000, 0)
        .expect("the release script's signature must verify");
    assert_eq!(verified.identity(), "operator-test");
    assert_eq!(verified.manifest().version, "0.2.0");
    assert!(verified
        .manifest()
        .artifact_for_host("darwin", "arm64")
        .is_some());
}

#[test]
fn the_canonical_signing_payload_is_byte_identical_across_languages() {
    let manifest = faktor_updater::UpdateManifest::parse(VECTOR.as_bytes()).unwrap();
    let payload = manifest.signing_payload().unwrap();
    assert_eq!(
        faktor_updater::manifest::sha256_hex(&payload),
        NODE_PAYLOAD_SHA256,
        "the Rust canonical payload must equal the node signer's payload"
    );
    // The signature is over exactly those bytes.
    let keys = keys();
    let trusted = keys.get("operator-test").unwrap();
    trusted
        .verify(&payload, &manifest.signature.unwrap().value)
        .expect("signature verifies over the canonical payload");
}

#[test]
fn the_vector_is_refused_after_any_tamper() {
    let keys = keys();
    let now = 1789567580000;
    for (field, replacement) in [
        ("version", "\"9.9.9\""),
        ("commit", "\"ffffffffffffffffffffffffffffffffffffffff\""),
        (
            "sha256",
            "\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\"",
        ),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(VECTOR).unwrap();
        match field {
            "sha256" => {
                value["artifacts"][0]["sha256"] = serde_json::json!(replacement.trim_matches('"'));
            }
            _ => {
                value[field] = serde_json::from_str(replacement).unwrap();
            }
        }
        let err = verify_manifest(
            &serde_json::to_vec(&value).unwrap(),
            &keys,
            Channel::Stable,
            now,
            0,
        )
        .unwrap_err();
        assert_eq!(err.code(), "manifest_tampered", "{field}");
    }
    // Expiry is honored over the frozen vector too.
    let err =
        verify_manifest(VECTOR.as_bytes(), &keys, Channel::Stable, 1792159572458, 0).unwrap_err();
    assert_eq!(err.code(), "manifest_expired");
    // And the allowlist is enforced: a different identity refuses.
    let other = TrustedKeys::new(vec![TrustedKey::from_base64(
        "somebody-else",
        "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=",
    )
    .unwrap()])
    .unwrap();
    let err =
        verify_manifest(VECTOR.as_bytes(), &other, Channel::Stable, 1789567580000, 0).unwrap_err();
    assert_eq!(err.code(), "manifest_unknown_key");
    // The embedded public key is a real one (32 raw bytes).
    assert_eq!(
        BASE64
            .decode("6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=")
            .unwrap()
            .len(),
        32
    );
}
