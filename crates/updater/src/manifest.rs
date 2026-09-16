//! The signed update manifest (`faktor-update/v1`).
//!
//! Wire shape:
//!
//! ```json
//! {
//!   "schema": "faktor-update/v1",
//!   "channel": "stable",
//!   "version": "0.2.0",
//!   "commit": "<40-hex git sha>",
//!   "artifacts": [{"name": "...", "os": "darwin", "arch": "arm64",
//!                  "sha256": "<64-hex>", "url": "https://...",
//!                  "size": 123}],
//!   "compatibility": {
//!     "cli": {"min": "0.1.0", "max": "0.3.0"},
//!     "daemon": {"min": "0.1.0", "max": "0.3.0"},
//!     "vscode": {"min": "*", "max": "*"},
//!     "jetbrains": {"min": "*", "max": "*"},
//!     "schema": {"min": 1, "max": 1}
//!   },
//!   "issued_at": 1760000000000,
//!   "expires_at": 1762592000000,
//!   "certification": {"level": "local_offline", "commit": "...",
//!                     "manifest_sha256": "...", "evidence": {"...": "..."}},
//!   "signature": {"algorithm": "ed25519", "identity": "operator",
//!                 "public_key": "<base64 raw 32B>",
//!                 "value": "<base64 64B>"}
//! }
//! ```
//!
//! `certification` and `size` are OPTIONAL additive fields; everything else
//! is required and validated (bounds, digest shape, URL shape, artifact-name
//! path safety, validity window). Unknown fields are parse errors.
//!
//! The signature payload is the CANONICAL JSON (object keys sorted
//! recursively, compact separators) of the manifest WITHOUT its `signature`
//! field — byte-identical to `scripts/certification/evidence.mjs`'s
//! canonicalization and `scripts/update-manifest.mjs`'s signer.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::channel::Channel;
use crate::compat::SchemaRange;
use crate::error::{ManifestRefusal, UpdateError};
use crate::keys::{TrustedKey, TrustedKeys};
use crate::version::{Version, VersionRange};

/// The only manifest schema this runtime understands.
pub const UPDATE_MANIFEST_SCHEMA: &str = "faktor-update/v1";
/// Bound on one artifact list.
pub const MAX_MANIFEST_ARTIFACTS: usize = 32;
/// Bound on one artifact name / os / arch token.
pub const MAX_ARTIFACT_TOKEN_BYTES: usize = 128;
/// Bound on one artifact URL.
pub const MAX_ARTIFACT_URL_BYTES: usize = 2048;
/// Bound on one certification evidence map.
pub const MAX_CERTIFICATION_EVIDENCE_ENTRIES: usize = 16;
/// The largest validity window a manifest may declare (366 days).
pub const MAX_MANIFEST_VALIDITY_MS: i64 = 366 * 24 * 60 * 60 * 1000;
/// The largest artifact the updater will download by default (256 MiB).
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
/// Tolerance applied to `issued_at` when a manifest was signed slightly in
/// the future by a skewed signer.
pub const DEFAULT_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

/// One downloadable artifact entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub sha256: String,
    pub url: String,
    /// Optional declared size (lets a client refuse a too-large download
    /// before opening the stream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// The compatibility block: closed ranges per component plus the numeric
/// native-schema range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub cli: VersionRange,
    pub daemon: VersionRange,
    pub vscode: VersionRange,
    pub jetbrains: VersionRange,
    pub schema: SchemaRange,
}

/// The optional certification provenance attached by the release script
/// (digests of the certification evidence the manifest was assembled from).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Certification {
    pub level: String,
    pub commit: String,
    pub manifest_sha256: Option<String>,
    #[serde(default)]
    pub evidence: std::collections::BTreeMap<String, String>,
}

/// The manifest signature (the repository's evidence signature shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestSignature {
    pub algorithm: String,
    pub identity: String,
    pub public_key: String,
    pub value: String,
}

/// One complete update manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    pub schema: String,
    pub channel: Channel,
    pub version: String,
    pub commit: String,
    pub artifacts: Vec<Artifact>,
    pub compatibility: Compatibility,
    pub issued_at: i64,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certification: Option<Certification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ManifestSignature>,
}

/// A manifest that PASSED signature verification, expiry checks and the
/// channel pin. Only this type can be staged or applied: the type is the
/// proof, so a code path cannot forget the verification step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    manifest: UpdateManifest,
    identity: String,
}

impl VerifiedManifest {
    pub fn manifest(&self) -> &UpdateManifest {
        &self.manifest
    }

    /// The allowlisted identity whose signature was verified.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn into_manifest(self) -> UpdateManifest {
        self.manifest
    }
}

impl UpdateManifest {
    /// Parse and shape-validate one manifest (no authenticity claim yet).
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestRefusal> {
        let manifest: UpdateManifest = serde_json::from_slice(bytes)
            .map_err(|e| ManifestRefusal::Malformed(format!("strict manifest parse: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Shape validation: schema tag, commit, version, bounded artifacts with
    /// path-safe names and shaped digests/URLs, and a bounded validity
    /// window. Every failure is a typed [`ManifestRefusal::Malformed`].
    pub fn validate(&self) -> Result<(), ManifestRefusal> {
        let malformed = |m: String| Err(ManifestRefusal::Malformed(m));
        if self.schema != UPDATE_MANIFEST_SCHEMA {
            return malformed(format!(
                "schema {:?} != {UPDATE_MANIFEST_SCHEMA:?}",
                self.schema
            ));
        }
        if !is_lower_hex(&self.commit, 40) {
            return malformed("commit must be a 40-character lowercase hex sha".into());
        }
        if let Err(e) = Version::parse(&self.version) {
            return malformed(format!("version: {e}"));
        }
        if self.artifacts.is_empty() || self.artifacts.len() > MAX_MANIFEST_ARTIFACTS {
            return malformed(format!(
                "artifacts must contain 1..={MAX_MANIFEST_ARTIFACTS} entries"
            ));
        }
        let mut names: Vec<&str> = Vec::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            validate_token("artifact name", &artifact.name)?;
            if artifact.name.contains('/')
                || artifact.name.contains('\\')
                || artifact.name.contains("..")
            {
                return malformed(format!(
                    "artifact name {:?} must be a plain file name (no paths, no traversal)",
                    artifact.name
                ));
            }
            validate_token("artifact os", &artifact.os)?;
            validate_token("artifact arch", &artifact.arch)?;
            if !is_lower_hex(&artifact.sha256, 64) {
                return malformed(format!(
                    "artifact {:?} sha256 must be 64 lowercase hex characters",
                    artifact.name
                ));
            }
            if artifact.url.is_empty() || artifact.url.len() > MAX_ARTIFACT_URL_BYTES {
                return malformed(format!(
                    "artifact {:?} url must be 1..={MAX_ARTIFACT_URL_BYTES} bytes",
                    artifact.name
                ));
            }
            if !artifact.url.is_ascii() {
                return malformed(format!("artifact {:?} url must be ASCII", artifact.name));
            }
            if !(artifact.url.starts_with("https://") || artifact.url.starts_with("http://")) {
                return malformed(format!("artifact {:?} url must be http(s)", artifact.name));
            }
            if artifact.url.contains(char::is_whitespace) {
                return malformed(format!("artifact {:?} url has whitespace", artifact.name));
            }
            if names.contains(&artifact.name.as_str()) {
                return malformed(format!("artifact name {:?} appears twice", artifact.name));
            }
            names.push(&artifact.name);
        }
        if self.issued_at <= 0 || self.expires_at <= 0 {
            return malformed("issued_at/expires_at must be positive unix milliseconds".into());
        }
        if self.expires_at < self.issued_at {
            return malformed("expires_at precedes issued_at".into());
        }
        if self.expires_at - self.issued_at > MAX_MANIFEST_VALIDITY_MS {
            return malformed("the manifest validity window exceeds 366 days".into());
        }
        if let Some(certification) = &self.certification {
            if !["none", "local_offline", "release"].contains(&certification.level.as_str()) {
                return malformed(format!(
                    "certification level {:?} is not none|local_offline|release",
                    certification.level
                ));
            }
            if certification.commit != self.commit {
                return malformed(
                    "certification.commit does not match the manifest commit (foreign evidence)"
                        .into(),
                );
            }
            if let Some(digest) = &certification.manifest_sha256 {
                if !is_lower_hex(digest, 64) {
                    return malformed("certification.manifest_sha256 must be 64 hex".into());
                }
            }
            if certification.evidence.len() > MAX_CERTIFICATION_EVIDENCE_ENTRIES {
                return malformed(format!(
                    "certification.evidence may hold at most {MAX_CERTIFICATION_EVIDENCE_ENTRIES} entries"
                ));
            }
            for (kind, digest) in &certification.evidence {
                if kind.is_empty() || kind.len() > 64 || !kind.is_ascii() {
                    return malformed(
                        "certification.evidence kind must be 1..=64 ASCII bytes".into(),
                    );
                }
                if !is_lower_hex(digest, 64) {
                    return malformed(format!(
                        "certification.evidence[{kind:?}] must be 64 lowercase hex"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The artifact for one host, when the manifest carries one.
    pub fn artifact_for_host(&self, os: &str, arch: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.os == os && artifact.arch == arch)
    }

    /// The CANONICAL signing payload: the manifest's JSON object with the
    /// `signature` field removed, keys sorted recursively, compact
    /// separators. Both the signer script and this verifier must agree byte
    /// for byte; the interop vector test pins it.
    pub fn signing_payload(&self) -> Result<Vec<u8>, ManifestRefusal> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            ManifestRefusal::Malformed(format!("manifest is not canonically serializable: {e}"))
        })?;
        if let Some(object) = value.as_object_mut() {
            object.remove("signature");
        }
        canonical_bytes(&value)
    }
}

/// Canonical JSON bytes: object keys sorted recursively (byte-wise, which
/// equals code-point order for the ASCII tokens this schema allows), compact
/// separators, no insignificant whitespace. Mirrors `canonicalJson` in
/// `scripts/certification/evidence.mjs` and `scripts/update-manifest.mjs`.
///
/// The sort is EXPLICIT here, never inherited from `serde_json`'s map type:
/// with the `preserve_order` feature (enabled somewhere else in a workspace
/// build) a `Value` object would serialize in insertion order, which would
/// silently change the signed bytes and break the release script's
/// signatures. The interop vector test pins the resulting digest.
pub fn canonical_bytes(value: &serde_json::Value) -> Result<Vec<u8>, ManifestRefusal> {
    let mut out = Vec::with_capacity(256);
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) -> Result<(), ManifestRefusal> {
    let encode_string = |out: &mut Vec<u8>, s: &str| -> Result<(), ManifestRefusal> {
        let encoded = serde_json::to_string(s)
            .map_err(|e| ManifestRefusal::Malformed(format!("canonical JSON string: {e}")))?;
        out.extend_from_slice(encoded.as_bytes());
        Ok(())
    };
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(true) => out.extend_from_slice(b"true"),
        serde_json::Value::Bool(false) => out.extend_from_slice(b"false"),
        serde_json::Value::Number(number) => {
            // The signed schema carries integers only (no floats).
            out.extend_from_slice(number.to_string().as_bytes());
        }
        serde_json::Value::String(s) => encode_string(out, s)?,
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        serde_json::Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                encode_string(out, key)?;
                out.push(b':');
                write_canonical(&object[key.as_str()], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// Authenticate one manifest:
///
/// 1. strict parse + shape validation;
/// 2. signature: absent → [`ManifestRefusal::Unsigned`]; unknown identity →
///    [`ManifestRefusal::UnknownKey`]; embedded key != allowlisted key →
///    [`ManifestRefusal::KeyMismatch`]; signature does not verify →
///    [`ManifestRefusal::Tampered`];
/// 3. validity window (`expires_at` honored, `issued_at` with skew);
/// 4. channel pin (`configured.accepts(manifest.channel)`).
///
/// Only then does anything in this runtime trust a manifest.
pub fn verify_manifest(
    bytes: &[u8],
    keys: &TrustedKeys,
    configured: Channel,
    now_ms: i64,
    skew_ms: i64,
) -> Result<VerifiedManifest, UpdateError> {
    let manifest = UpdateManifest::parse(bytes)?;
    let signature = manifest
        .signature
        .clone()
        .ok_or(ManifestRefusal::Unsigned)?;
    if signature.algorithm != "ed25519" {
        return Err(ManifestRefusal::Malformed(format!(
            "unsupported signature algorithm {:?}",
            signature.algorithm
        ))
        .into());
    }
    let trusted: &TrustedKey =
        keys.get(&signature.identity)
            .ok_or_else(|| ManifestRefusal::UnknownKey {
                identity: signature.identity.clone(),
            })?;
    if trusted.public_key_base64() != signature.public_key {
        return Err(ManifestRefusal::KeyMismatch {
            identity: signature.identity.clone(),
        }
        .into());
    }
    let payload = manifest.signing_payload()?;
    trusted
        .verify(&payload, &signature.value)
        .map_err(|_| ManifestRefusal::Tampered)?;
    if now_ms > manifest.expires_at {
        return Err(ManifestRefusal::Expired {
            expires_at: manifest.expires_at,
            now_ms,
        }
        .into());
    }
    if now_ms.saturating_add(skew_ms) < manifest.issued_at {
        return Err(ManifestRefusal::NotYetValid {
            issued_at: manifest.issued_at,
            now_ms,
        }
        .into());
    }
    if !configured.accepts(manifest.channel) {
        return Err(ManifestRefusal::ChannelMismatch {
            configured: configured.to_string(),
            found: manifest.channel.to_string(),
        }
        .into());
    }
    Ok(VerifiedManifest {
        manifest,
        identity: signature.identity,
    })
}

/// sha256 hex of arbitrary bytes (digest checks are always over the full
/// streamed payload).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// True when `value` is exactly `len` lowercase hex characters.
pub fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_token(kind: &str, value: &str) -> Result<(), ManifestRefusal> {
    if value.is_empty() || value.len() > MAX_ARTIFACT_TOKEN_BYTES || !value.is_ascii() {
        return Err(ManifestRefusal::Malformed(format!(
            "{kind} {value:?} must be 1..={MAX_ARTIFACT_TOKEN_BYTES} ASCII bytes"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
    {
        return Err(ManifestRefusal::Malformed(format!(
            "{kind} {value:?} contains a character outside [A-Za-z0-9._+-]"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};

    fn range(min: &str, max: &str) -> VersionRange {
        VersionRange::from_parts(min, max).unwrap()
    }

    /// A deterministic test keypair (seed = 7) — signatures are reproducible.
    pub(crate) fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    pub(crate) fn test_keys() -> (TrustedKeys, SigningKey) {
        let key = test_signing_key();
        let trusted = TrustedKey::from_base64(
            "operator-test",
            &BASE64.encode(key.verifying_key().to_bytes()),
        )
        .unwrap();
        (TrustedKeys::new(vec![trusted]).unwrap(), key)
    }

    pub(crate) fn sample_manifest() -> UpdateManifest {
        UpdateManifest {
            schema: UPDATE_MANIFEST_SCHEMA.to_string(),
            channel: Channel::Stable,
            version: "0.2.0".into(),
            commit: "a".repeat(40),
            artifacts: vec![Artifact {
                name: "faktor-cli-0.2.0-darwin-arm64.tar.gz".into(),
                os: "darwin".into(),
                arch: "arm64".into(),
                sha256: "b".repeat(64),
                url: "https://updates.example.test/faktor/0.2.0/bundle.tar.gz".into(),
                size: Some(1024),
            }],
            compatibility: Compatibility {
                cli: range("0.1.0", "0.3.0"),
                daemon: range("0.1.0", "0.3.0"),
                vscode: range("*", "*"),
                jetbrains: range("*", "*"),
                schema: SchemaRange { min: 1, max: 1 },
            },
            issued_at: 1_000,
            expires_at: 2_000,
            certification: None,
            signature: None,
        }
    }

    pub(crate) fn sign_manifest(manifest: &UpdateManifest, key: &SigningKey) -> Vec<u8> {
        let payload = manifest.signing_payload().unwrap();
        let signature = key.sign(&payload);
        let mut signed = manifest.clone();
        signed.signature = Some(ManifestSignature {
            algorithm: "ed25519".into(),
            identity: "operator-test".into(),
            public_key: BASE64.encode(key.verifying_key().to_bytes()),
            value: BASE64.encode(signature.to_bytes()),
        });
        serde_json::to_vec(&signed).unwrap()
    }

    #[test]
    fn a_signed_manifest_verifies_and_the_payload_covers_every_field() {
        let (keys, key) = test_keys();
        let bytes = sign_manifest(&sample_manifest(), &key);
        let verified = verify_manifest(&bytes, &keys, Channel::Stable, 1_500, 0).unwrap();
        assert_eq!(verified.identity(), "operator-test");
        let mut mutated = sample_manifest();
        mutated.version = "0.2.1".into();
        let bytes = sign_manifest(&mutated, &key);
        assert!(verify_manifest(&bytes, &keys, Channel::Stable, 1_500, 0).is_ok());
        // The signature no longer covers a manifest whose field changed
        // after signing.
        let mut tampered = mutated.clone();
        tampered.version = "9.9.9".into();
        let reshaped = serde_json::to_vec(&tampered).unwrap();
        let refused = verify_manifest(&reshaped, &keys, Channel::Stable, 1_500, 0).unwrap_err();
        assert_eq!(refused.code(), "manifest_unsigned");
    }

    #[test]
    fn unsigned_unknown_key_and_tampered_manifests_are_distinct_refusals() {
        let (keys, key) = test_keys();
        // Unsigned.
        let unsigned = serde_json::to_vec(&sample_manifest()).unwrap();
        assert_eq!(
            verify_manifest(&unsigned, &keys, Channel::Stable, 1_500, 0)
                .unwrap_err()
                .code(),
            "manifest_unsigned"
        );
        // Unknown identity.
        let mut mine = sample_manifest();
        mine.signature = Some(ManifestSignature {
            algorithm: "ed25519".into(),
            identity: "not-allowlisted".into(),
            public_key: BASE64.encode(key.verifying_key().to_bytes()),
            value: BASE64.encode([0u8; 64]),
        });
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&mine).unwrap(),
                &keys,
                Channel::Stable,
                1_500,
                0
            )
            .unwrap_err()
            .code(),
            "manifest_unknown_key"
        );
        // Allowlisted identity but a substituted public key.
        let attacker = SigningKey::from_bytes(&[9u8; 32]);
        let mut swapped = mine.clone();
        let signature = swapped.signature.as_mut().unwrap();
        signature.identity = "operator-test".into();
        signature.public_key = BASE64.encode(attacker.verifying_key().to_bytes());
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&swapped).unwrap(),
                &keys,
                Channel::Stable,
                1_500,
                0
            )
            .unwrap_err()
            .code(),
            "manifest_key_mismatch"
        );
        // Signature bytes flipped.
        let signed = sign_manifest(&sample_manifest(), &key);
        let mut value: serde_json::Value = serde_json::from_slice(&signed).unwrap();
        let sig = value["signature"]["value"].as_str().unwrap();
        let mut decoded = BASE64.decode(sig).unwrap();
        decoded[0] ^= 0x01;
        value["signature"]["value"] = serde_json::Value::String(BASE64.encode(decoded));
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&value).unwrap(),
                &keys,
                Channel::Stable,
                1_500,
                0
            )
            .unwrap_err()
            .code(),
            "manifest_tampered"
        );
    }

    #[test]
    fn a_tampered_field_invalidates_the_signature_not_just_the_parse() {
        let (keys, key) = test_keys();
        let signed = sign_manifest(&sample_manifest(), &key);
        let mut value: serde_json::Value = serde_json::from_slice(&signed).unwrap();
        value["version"] = serde_json::Value::String("9.9.9".into());
        let refused = verify_manifest(
            &serde_json::to_vec(&value).unwrap(),
            &keys,
            Channel::Stable,
            1_500,
            0,
        )
        .unwrap_err();
        assert_eq!(refused.code(), "manifest_tampered", "{refused}");
    }

    #[test]
    fn expiry_is_honored_and_skew_is_bounded() {
        let (keys, key) = test_keys();
        let mut manifest = sample_manifest();
        manifest.issued_at = 1_000;
        manifest.expires_at = 1_500;
        let bytes = sign_manifest(&manifest, &key);
        assert!(verify_manifest(&bytes, &keys, Channel::Stable, 1_500, 0).is_ok());
        let refused = verify_manifest(&bytes, &keys, Channel::Stable, 1_501, 0).unwrap_err();
        assert_eq!(refused.code(), "manifest_expired");
        // Issued in the future: refused without skew, tolerated within skew.
        let mut future = sample_manifest();
        future.issued_at = 2_000;
        future.expires_at = 3_000;
        let bytes = sign_manifest(&future, &key);
        assert_eq!(
            verify_manifest(&bytes, &keys, Channel::Stable, 1_000, 0)
                .unwrap_err()
                .code(),
            "manifest_not_yet_valid"
        );
        assert!(
            verify_manifest(&bytes, &keys, Channel::Stable, 1_000, DEFAULT_CLOCK_SKEW_MS).is_ok()
        );
    }

    #[test]
    fn channel_pin_refuses_a_manifest_from_another_channel() {
        let (keys, key) = test_keys();
        let mut beta = sample_manifest();
        beta.channel = Channel::Beta;
        let bytes = sign_manifest(&beta, &key);
        assert_eq!(
            verify_manifest(&bytes, &keys, Channel::Stable, 1_500, 0)
                .unwrap_err()
                .code(),
            "manifest_channel_mismatch"
        );
        assert!(verify_manifest(&bytes, &keys, Channel::Beta, 1_500, 0).is_ok());
        assert!(verify_manifest(&bytes, &keys, Channel::Dev, 1_500, 0).is_ok());
    }

    #[test]
    fn an_empty_allowlist_refuses_every_manifest() {
        let (_, key) = test_keys();
        let bytes = sign_manifest(&sample_manifest(), &key);
        let refused =
            verify_manifest(&bytes, &TrustedKeys::empty(), Channel::Stable, 1_500, 0).unwrap_err();
        assert_eq!(refused.code(), "manifest_unknown_key");
    }

    #[test]
    fn malformed_shapes_are_refused_before_any_trust() {
        let (keys, key) = test_keys();
        let cases: Vec<(String, UpdateManifest)> = vec![
            (
                "wrong schema".into(),
                UpdateManifest {
                    schema: "faktor-update/v2".into(),
                    ..sample_manifest()
                },
            ),
            (
                "uppercase digest".into(),
                UpdateManifest {
                    artifacts: vec![Artifact {
                        sha256: "B".repeat(64),
                        ..sample_manifest().artifacts[0].clone()
                    }],
                    ..sample_manifest()
                },
            ),
            (
                "short commit".into(),
                UpdateManifest {
                    commit: "abc".into(),
                    ..sample_manifest()
                },
            ),
            (
                "path traversal name".into(),
                UpdateManifest {
                    artifacts: vec![Artifact {
                        name: "../evil".into(),
                        ..sample_manifest().artifacts[0].clone()
                    }],
                    ..sample_manifest()
                },
            ),
            (
                "duplicate artifact names".into(),
                UpdateManifest {
                    artifacts: vec![
                        sample_manifest().artifacts[0].clone(),
                        sample_manifest().artifacts[0].clone(),
                    ],
                    ..sample_manifest()
                },
            ),
            (
                "file url".into(),
                UpdateManifest {
                    artifacts: vec![Artifact {
                        url: "file:///etc/passwd".into(),
                        ..sample_manifest().artifacts[0].clone()
                    }],
                    ..sample_manifest()
                },
            ),
            (
                "expiry before issue".into(),
                UpdateManifest {
                    issued_at: 5_000,
                    expires_at: 1_000,
                    ..sample_manifest()
                },
            ),
            (
                "validity window too long".into(),
                UpdateManifest {
                    issued_at: 1_000,
                    expires_at: 1_000 + MAX_MANIFEST_VALIDITY_MS + 1,
                    ..sample_manifest()
                },
            ),
            (
                "foreign certification commit".into(),
                UpdateManifest {
                    certification: Some(Certification {
                        level: "release".into(),
                        commit: "c".repeat(40),
                        manifest_sha256: None,
                        evidence: Default::default(),
                    }),
                    ..sample_manifest()
                },
            ),
        ];
        for (label, manifest) in cases {
            // The bytes are signed, so ONLY shape validation can refuse them.
            let bytes = sign_manifest(&manifest, &key);
            let refused = verify_manifest(&bytes, &keys, Channel::Stable, 1_500, 0).unwrap_err();
            assert_eq!(refused.code(), "manifest_malformed", "{label}: {refused}");
        }
        // Unknown fields are parse errors.
        let mut value: serde_json::Value =
            serde_json::from_slice(&sign_manifest(&sample_manifest(), &key)).unwrap();
        value["surprise"] = serde_json::Value::Bool(true);
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&value).unwrap(),
                &keys,
                Channel::Stable,
                1_500,
                0
            )
            .unwrap_err()
            .code(),
            "manifest_malformed"
        );
    }

    #[test]
    fn canonical_payload_is_order_independent_and_excludes_the_signature() {
        let manifest = sample_manifest();
        let payload = manifest.signing_payload().unwrap();
        let text = String::from_utf8(payload.clone()).unwrap();
        assert!(!text.contains("signature"));
        assert!(!text.contains(' '));
        // Object keys sorted recursively: compatibility < expires_at is the
        // canonical (alphabetical) order.
        let compatibility = text.find("\"compatibility\"").unwrap();
        let expires = text.find("\"expires_at\"").unwrap();
        assert!(compatibility < expires);
        // Re-parsing a reordered input produces the same payload.
        let mut value: serde_json::Value = serde_json::to_value(&manifest).unwrap();
        let object = value.as_object_mut().unwrap();
        let reordered: serde_json::Map<String, serde_json::Value> = object
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let reshaped = serde_json::Value::Object(reordered);
        assert_eq!(canonical_bytes(&reshaped).unwrap(), payload);
    }

    #[test]
    fn artifact_selection_is_exact_on_os_and_arch() {
        let manifest = sample_manifest();
        assert!(manifest.artifact_for_host("darwin", "arm64").is_some());
        assert!(manifest.artifact_for_host("darwin", "x86_64").is_none());
        assert!(manifest.artifact_for_host("linux", "arm64").is_none());
    }
}
