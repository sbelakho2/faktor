//! Ed25519 attestation for executed engineering receipts.
//!
//! An in-process kernel or an external solver adapter signs one canonical
//! digest covering the identity, model/input/settings digests, property
//! revisions, boundary conditions, convergence, results and artifacts. The
//! signing seed is a daemon-local, owner-only (`0600` on unix) engineering
//! key; it is never logged, exported, or carried inside a receipt. A receipt
//! carries only the public key, its fingerprint and the signature, so the
//! 11-gate verifier can refuse a missing, malformed, tampered or wrong-key
//! attestation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::{CoreError, CoreResult};
use crate::receipt::AnalysisReceipt;

/// Algorithm tag carried by every receipt attestation.
pub const ATTESTATION_ALGORITHM: &str = "ed25519";
/// Domain separation for the signed canonical receipt digest.
pub const ATTESTATION_DOMAIN: &[u8] = b"faktor-engineering-receipt-attestation/v1";
/// Daemon-local engineering signing-key filename inside the data directory.
pub const ENGINEERING_SIGNING_KEY_FILE: &str = "engineering-signing.key";
/// Daemon-local key-history filename inside the data directory.
pub const ENGINEERING_KEY_HISTORY_FILE: &str = "engineering-signing-keys.json";
/// Current key-history schema version.
pub const KEY_HISTORY_VERSION: u32 = 1;
/// A signing seed is exactly one Ed25519 seed.
pub const SIGNING_KEY_BYTES: usize = 32;
pub const MAX_TRUSTED_KEYS: usize = 32;
/// Bound on the public key-history document.
pub const MAX_KEY_HISTORY_BYTES: u64 = 64 * 1024;

#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptAttestation {
    /// Exactly [`ATTESTATION_ALGORITHM`].
    pub algorithm: String,
    /// BLAKE3 fingerprint of the public key (lowercase hex).
    pub key_id: String,
    /// Raw Ed25519 public key (lowercase hex, 32 bytes).
    pub public_key: String,
    /// Ed25519 signature over the domain-separated canonical digest
    /// (lowercase hex, 64 bytes).
    pub signature: String,
    /// Canonical receipt digest the signature commits to.
    pub digest: Digest,
    /// The signing instant (unix milliseconds) committed to by the
    /// signature. Key history uses it to distinguish receipts issued while a
    /// key was active from receipts presented after its retirement.
    #[serde(default)]
    pub signed_at_ms: u64,
}

impl ReceiptAttestation {
    pub fn validate_structure(&self) -> CoreResult<()> {
        if self.algorithm != ATTESTATION_ALGORITHM {
            return Err(CoreError::InvalidReceiptAttestation {
                reason: format!(
                    "attestation algorithm {:?} is not {ATTESTATION_ALGORITHM:?}",
                    self.algorithm
                ),
            });
        }
        let public_key = parse_fixed_hex::<32>("attestation.public_key", &self.public_key)?;
        let _ = parse_fixed_hex::<64>("attestation.signature", &self.signature)?;
        if self.key_id != Digest::of_bytes(&public_key).to_hex() {
            return Err(CoreError::InvalidReceiptAttestation {
                reason: "attestation key id does not fingerprint the enclosed public key"
                    .to_string(),
            });
        }
        Ok(())
    }
}

fn parse_fixed_hex<const N: usize>(field: &'static str, value: &str) -> CoreResult<[u8; N]> {
    if value.len() != N * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CoreError::InvalidReceiptAttestation {
            reason: format!(
                "{field} must be {} lowercase/uppercase hex characters",
                N * 2
            ),
        });
    }
    let mut out = [0u8; N];
    for (index, chunk) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let text =
            std::str::from_utf8(chunk).map_err(|_| CoreError::InvalidReceiptAttestation {
                reason: format!("{field} is not valid hex"),
            })?;
        out[index] =
            u8::from_str_radix(text, 16).map_err(|_| CoreError::InvalidReceiptAttestation {
                reason: format!("{field} is not valid hex"),
            })?;
    }
    Ok(out)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Canonical bytes signed by the attestation: a domain tag, the receipt
/// digest and the signing instant, so a signature cannot be replayed as
/// another message or moved outside its key-validity window.
fn signing_message(digest: Digest, signed_at_ms: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(ATTESTATION_DOMAIN.len() + 32 + 8);
    message.extend_from_slice(ATTESTATION_DOMAIN);
    message.extend_from_slice(digest.as_bytes());
    message.extend_from_slice(&signed_at_ms.to_le_bytes());
    message
}

pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// The validity interval recorded for one signing key. A receipt is accepted
/// only when its signed instant falls inside the interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyValidity {
    pub not_before_ms: u64,
    pub retired_at_ms: Option<u64>,
}

impl KeyValidity {
    pub fn accepts(&self, signed_at_ms: u64) -> bool {
        signed_at_ms >= self.not_before_ms
            && self
                .retired_at_ms
                .is_none_or(|retired_at_ms| signed_at_ms < retired_at_ms)
    }
}

/// In-process signer. The seed is zeroized with the `SigningKey`; `Debug`
/// never prints key material.
#[derive(Clone)]
pub struct ReceiptSigner {
    key: SigningKey,
    validity: Option<KeyValidity>,
}

impl std::fmt::Debug for ReceiptSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptSigner")
            .field("key_id", &self.key_id())
            .finish_non_exhaustive()
    }
}

impl ReceiptSigner {
    pub fn from_seed(seed: [u8; SIGNING_KEY_BYTES]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
            validity: None,
        }
    }

    /// Attach the key-history validity window. Keys loaded from the data dir
    /// carry it; an explicit window also lets embedded callers model a
    /// retired key without weakening the store.
    pub fn with_validity(mut self, validity: KeyValidity) -> Self {
        self.validity = Some(validity);
        self
    }

    pub fn validity(&self) -> Option<KeyValidity> {
        self.validity
    }

    pub fn generate() -> Self {
        use rand::Rng as _;
        let mut seed = [0u8; SIGNING_KEY_BYTES];
        rand::rng().fill(&mut seed);
        Self::from_seed(seed)
    }

    pub fn public_key_hex(&self) -> String {
        to_hex(self.key.verifying_key().as_bytes())
    }

    pub fn key_id(&self) -> String {
        Digest::of_bytes(self.key.verifying_key().as_bytes()).to_hex()
    }

    /// Sign the one canonical receipt digest at the current instant. The
    /// receipt must already be structurally valid; signing a receipt with a
    /// lying execution record would only authenticate a lie.
    pub fn sign_receipt(&self, receipt: &mut AnalysisReceipt) -> CoreResult<()> {
        self.sign_receipt_at(receipt, unix_time_ms())
    }

    /// Sign at an explicit instant, refusing to sign outside the key's
    /// recorded validity window.
    pub fn sign_receipt_at(
        &self,
        receipt: &mut AnalysisReceipt,
        signed_at_ms: u64,
    ) -> CoreResult<()> {
        if let Some(validity) = self.validity {
            if signed_at_ms < validity.not_before_ms {
                return Err(CoreError::SigningKeyNotYetValid {
                    key_id: self.key_id(),
                    not_before_ms: validity.not_before_ms,
                });
            }
            if let Some(retired_at_ms) = validity
                .retired_at_ms
                .filter(|retired_at_ms| signed_at_ms >= *retired_at_ms)
            {
                return Err(CoreError::RetiredSigningKey {
                    key_id: self.key_id(),
                    retired_at_ms,
                });
            }
        }
        receipt.validate()?;
        let digest = receipt.canonical_digest()?;
        let signature = self.key.sign(&signing_message(digest, signed_at_ms));
        receipt.attestation = Some(ReceiptAttestation {
            algorithm: ATTESTATION_ALGORITHM.to_string(),
            key_id: self.key_id(),
            public_key: self.public_key_hex(),
            signature: to_hex(&signature.to_bytes()),
            digest,
            signed_at_ms,
        });
        Ok(())
    }
}

/// Verifies the enclosed signature and, when a trust set is configured, that
/// the signing key is one of the trusted daemon keys.
#[derive(Debug, Clone, Default)]
pub struct ReceiptVerifier {
    trusted: BTreeMap<String, KeyValidity>,
    require_trust: bool,
}

impl ReceiptVerifier {
    /// Verify the Ed25519 signature under the key embedded in the receipt.
    /// This proves integrity only; use [`Self::trusting`] for identity.
    pub fn self_attested() -> Self {
        Self {
            trusted: BTreeMap::new(),
            require_trust: false,
        }
    }

    /// Verify against exactly the trusted public-key fingerprints. An empty
    /// set is a configuration error, not a wildcard.
    pub fn trusting<I, S>(key_ids: I) -> CoreResult<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut trusted = BTreeMap::new();
        for key_id in key_ids {
            let key_id = key_id.into();
            let _ = parse_fixed_hex::<32>("trusted_key_id", &key_id)?;
            trusted.insert(
                key_id.to_ascii_lowercase(),
                KeyValidity {
                    not_before_ms: 0,
                    retired_at_ms: None,
                },
            );
        }
        if trusted.is_empty() {
            return Err(CoreError::InvalidReceiptAttestation {
                reason: "a trusted verifier needs at least one key fingerprint".to_string(),
            });
        }
        if trusted.len() > MAX_TRUSTED_KEYS {
            return Err(CoreError::InvalidReceiptAttestation {
                reason: format!("trusted key set exceeds {MAX_TRUSTED_KEYS} keys"),
            });
        }
        Ok(Self {
            trusted,
            require_trust: true,
        })
    }

    /// Trust exactly one public key (32-byte hex); its fingerprint is
    /// derived, so a caller cannot pair a key with a foreign fingerprint.
    pub fn trusting_public_key(public_key_hex: &str) -> CoreResult<Self> {
        let bytes = parse_fixed_hex::<32>("trusted_public_key", public_key_hex)?;
        Self::trusting([Digest::of_bytes(&bytes).to_hex()])
    }

    /// Trust exactly one already-computed public-key fingerprint.
    pub fn trusting_key_id(key_id: &str) -> CoreResult<Self> {
        Self::trusting([key_id.to_string()])
    }

    /// Trust every key in the daemon's key history. Retired keys remain
    /// trusted only for receipts signed inside their recorded validity
    /// window; an active key remains valid until it is retired.
    pub fn trusting_history(history: &KeyHistory) -> CoreResult<Self> {
        history.validate()?;
        let mut trusted = BTreeMap::new();
        for entry in &history.keys {
            trusted.insert(entry.key_id.clone(), entry.validity());
        }
        Ok(Self {
            trusted,
            require_trust: true,
        })
    }

    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.trusted.keys().map(String::as_str)
    }

    pub fn verify(&self, receipt: &AnalysisReceipt) -> CoreResult<()> {
        receipt.verify_attestation()?;
        if self.require_trust {
            let Some(attestation) = &receipt.attestation else {
                return Err(CoreError::MissingReceiptAttestation);
            };
            let Some(validity) = self.trusted.get(&attestation.key_id) else {
                return Err(CoreError::UntrustedReceiptKey {
                    key_id: attestation.key_id.clone(),
                });
            };
            if !validity.accepts(attestation.signed_at_ms) {
                return Err(CoreError::ReceiptKeyOutsideValidity {
                    key_id: attestation.key_id.clone(),
                    signed_at_ms: attestation.signed_at_ms,
                    not_before_ms: validity.not_before_ms,
                    retired_at_ms: validity.retired_at_ms,
                });
            }
        }
        Ok(())
    }
}

impl AnalysisReceipt {
    /// The canonical digest over every field that makes the receipt claim.
    /// The attestation itself is excluded (it signs this digest). Encoding is
    /// stable: fixed struct field order, `BTreeMap` boundaries, lowercase-hex
    /// digests and deterministic enum tags.
    pub fn canonical_digest(&self) -> CoreResult<Digest> {
        #[derive(Serialize)]
        struct Canonical<'a> {
            schema: &'static str,
            identity: &'a crate::receipt::AnalysisIdentity,
            input_digest: Digest,
            model_digest: Digest,
            property_revisions: &'a [crate::receipt::PropertyRevision],
            boundary_conditions:
                &'a std::collections::BTreeMap<String, crate::value::EngineeringValue>,
            settings_digest: Digest,
            mesh_digest: Option<Digest>,
            convergence: &'a crate::receipt::Convergence,
            warnings: &'a [crate::receipt::AnalysisWarning],
            results: &'a [crate::receipt::ReceiptResult],
            uncertainty: engineering_units::Uncertainty,
            artifacts: &'a [crate::receipt::ArtifactRef],
            repeatability: &'a crate::receipt::Repeatability,
            executed: bool,
            execution: &'a crate::receipt::ExecutionEvidence,
            selection_digest: Option<Digest>,
        }
        let canonical = Canonical {
            schema: "faktor.engineering.analysis-receipt/v1",
            identity: &self.identity,
            input_digest: self.input_digest,
            model_digest: self.model_digest,
            property_revisions: &self.property_revisions,
            boundary_conditions: &self.boundary_conditions,
            settings_digest: self.settings_digest,
            mesh_digest: self.mesh_digest,
            convergence: &self.convergence,
            warnings: &self.warnings,
            results: &self.results,
            uncertainty: self.uncertainty,
            artifacts: &self.artifacts,
            repeatability: &self.repeatability,
            executed: self.executed,
            execution: &self.execution,
            selection_digest: self.selection_digest,
        };
        Digest::of_json(&canonical)
    }

    /// Verify the enclosed Ed25519 attestation against the canonical digest.
    pub fn verify_attestation(&self) -> CoreResult<()> {
        let Some(attestation) = &self.attestation else {
            return Err(CoreError::MissingReceiptAttestation);
        };
        attestation.validate_structure()?;
        let expected = self.canonical_digest()?;
        if attestation.digest != expected {
            return Err(CoreError::InvalidReceiptAttestation {
                reason: "attestation digest does not match the canonical receipt digest"
                    .to_string(),
            });
        }
        let key_bytes = parse_fixed_hex::<32>("attestation.public_key", &attestation.public_key)?;
        let verifying_key = VerifyingKey::from_bytes(&key_bytes).map_err(|error| {
            CoreError::InvalidReceiptAttestation {
                reason: format!("attestation public key is not a valid Ed25519 point: {error}"),
            }
        })?;
        let signature_bytes =
            parse_fixed_hex::<64>("attestation.signature", &attestation.signature)?;
        let signature = Signature::from_bytes(&signature_bytes);
        verifying_key
            .verify(
                &signing_message(expected, attestation.signed_at_ms),
                &signature,
            )
            .map_err(|_| CoreError::ReceiptSignatureMismatch)
    }
}

/// The daemon-local signing-key path inside `data_dir`.
pub fn signing_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ENGINEERING_SIGNING_KEY_FILE)
}

/// The daemon-local public key-history path inside `data_dir`.
pub fn key_history_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ENGINEERING_KEY_HISTORY_FILE)
}

/// One recorded signing key: public material plus the interval in which it
/// was allowed to sign. Retired entries keep their public key so receipts
/// issued before retirement remain auditable.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyHistoryEntry {
    /// BLAKE3 fingerprint of the public key (lowercase hex).
    pub key_id: String,
    /// Raw Ed25519 public key (lowercase hex, 32 bytes).
    pub public_key: String,
    /// First instant (unix milliseconds) this key could sign.
    pub not_before_ms: u64,
    /// Retirement instant (unix milliseconds), if retired. A retired key is
    /// verify-only: it can never sign a receipt at or after this instant.
    #[serde(default)]
    pub retired_at_ms: Option<u64>,
}

impl KeyHistoryEntry {
    pub fn new(signer: &ReceiptSigner, not_before_ms: u64) -> Self {
        Self {
            key_id: signer.key_id(),
            public_key: signer.public_key_hex(),
            not_before_ms,
            retired_at_ms: None,
        }
    }

    pub fn validity(&self) -> KeyValidity {
        KeyValidity {
            not_before_ms: self.not_before_ms,
            retired_at_ms: self.retired_at_ms,
        }
    }
}

/// The append-only key history. Rotation retires the current entry and adds
/// the new key; old entries are never deleted or rewritten.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyHistory {
    pub version: u32,
    pub keys: Vec<KeyHistoryEntry>,
}

impl KeyHistory {
    pub fn validate(&self) -> CoreResult<()> {
        if self.version != KEY_HISTORY_VERSION {
            return Err(CoreError::KeyStoreRefused {
                path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                reason: format!(
                    "key history version {} is not {KEY_HISTORY_VERSION}",
                    self.version
                ),
            });
        }
        if self.keys.is_empty() || self.keys.len() > MAX_TRUSTED_KEYS {
            return Err(CoreError::KeyStoreRefused {
                path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                reason: format!(
                    "key history must contain 1..={MAX_TRUSTED_KEYS} keys, got {}",
                    self.keys.len()
                ),
            });
        }
        let mut seen = BTreeSet::new();
        let mut active = 0usize;
        for entry in &self.keys {
            let _ =
                parse_fixed_hex::<32>("key_history.key_id", &entry.key_id).map_err(|error| {
                    CoreError::KeyStoreRefused {
                        path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                        reason: format!("key history entry has an invalid key id: {error}"),
                    }
                })?;
            let public_key = parse_fixed_hex::<32>("key_history.public_key", &entry.public_key)
                .map_err(|error| CoreError::KeyStoreRefused {
                    path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                    reason: format!("key history entry has an invalid public key: {error}"),
                })?;
            VerifyingKey::from_bytes(&public_key).map_err(|error| CoreError::KeyStoreRefused {
                path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                reason: format!("key history public key is not a valid Ed25519 point: {error}"),
            })?;
            if entry.key_id != Digest::of_bytes(&public_key).to_hex() {
                return Err(CoreError::KeyStoreRefused {
                    path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                    reason: format!(
                        "key history fingerprint {} does not match its public key",
                        entry.key_id
                    ),
                });
            }
            if !seen.insert(entry.key_id.clone()) {
                return Err(CoreError::KeyStoreRefused {
                    path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                    reason: format!("key history repeats key {}", entry.key_id),
                });
            }
            if let Some(retired_at_ms) = entry.retired_at_ms {
                if retired_at_ms < entry.not_before_ms {
                    return Err(CoreError::KeyStoreRefused {
                        path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                        reason: format!(
                            "key {} retires at {retired_at_ms} before it becomes valid at {}",
                            entry.key_id, entry.not_before_ms
                        ),
                    });
                }
            } else {
                active += 1;
            }
        }
        if active > 1 {
            return Err(CoreError::KeyStoreRefused {
                path: ENGINEERING_KEY_HISTORY_FILE.to_string(),
                reason: format!("key history records {active} active keys; at most one is allowed"),
            });
        }
        Ok(())
    }

    pub fn entry_for(&self, key_id: &str) -> Option<&KeyHistoryEntry> {
        self.keys.iter().find(|entry| entry.key_id == key_id)
    }

    pub fn active(&self) -> Option<&KeyHistoryEntry> {
        self.keys.iter().find(|entry| entry.retired_at_ms.is_none())
    }
}

/// Load the daemon-local key without creating one. Verification must not
/// mint a key that silently makes stored receipts unverifiable.
pub fn load_signing_key(data_dir: &Path) -> CoreResult<ReceiptSigner> {
    #[cfg(unix)]
    {
        store::load_signing_key(data_dir)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(CoreError::KeyStorePlatformUnsupported {
            platform: std::env::consts::OS,
        })
    }
}

/// Load the daemon-local key, creating it on first use (32 random bytes,
/// owner-only `0600` on unix). A retired key is refused for signing; use
/// [`rotate_signing_key`] to mint an active successor. The bytes are never
/// logged or exported.
pub fn load_or_create_signing_key(data_dir: &Path) -> CoreResult<ReceiptSigner> {
    #[cfg(unix)]
    {
        store::load_or_create_signing_key(data_dir)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(CoreError::KeyStorePlatformUnsupported {
            platform: std::env::consts::OS,
        })
    }
}

/// Load the recorded public key history without creating or rotating
/// anything.
pub fn load_key_history(data_dir: &Path) -> CoreResult<KeyHistory> {
    #[cfg(unix)]
    {
        store::load_key_history(data_dir)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(CoreError::KeyStorePlatformUnsupported {
            platform: std::env::consts::OS,
        })
    }
}

/// The verifier for this daemon: every key in the history (active and
/// retired, each with its validity window), or the current key when no
/// history exists yet.
pub fn load_trusted_verifier(data_dir: &Path) -> CoreResult<ReceiptVerifier> {
    #[cfg(unix)]
    {
        store::load_trusted_verifier(data_dir)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(CoreError::KeyStorePlatformUnsupported {
            platform: std::env::consts::OS,
        })
    }
}

/// Retire the current key at `retired_at_ms` and create a fresh active key.
/// Old public keys stay in the history, so receipts signed before retirement
/// remain auditable; the retired key can never sign at or after the
/// retirement instant.
pub fn rotate_signing_key(data_dir: &Path, retired_at_ms: u64) -> CoreResult<ReceiptSigner> {
    #[cfg(unix)]
    {
        store::rotate_signing_key(data_dir, retired_at_ms)
    }
    #[cfg(not(unix))]
    {
        let _ = (data_dir, retired_at_ms);
        Err(CoreError::KeyStorePlatformUnsupported {
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(unix)]
mod store {
    use super::*;
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    use std::os::fd::AsFd as _;
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    struct AnchoredDir {
        handle: File,
    }

    impl AnchoredDir {
        fn open(data_dir: &Path) -> CoreResult<Self> {
            let fd = rustix::fs::open(
                data_dir,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| CoreError::KeyStore {
                path: data_dir.display().to_string(),
                reason: error.to_string(),
            })?;
            let handle = File::from(fd);
            validate_data_dir(&handle, data_dir)?;
            Ok(Self { handle })
        }
    }

    fn ensure_data_dir(data_dir: &Path) -> CoreResult<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        match builder.create(data_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(CoreError::KeyStore {
                path: data_dir.display().to_string(),
                reason: error.to_string(),
            }),
        }
    }

    fn validate_owner(what: &str, path: &Path, owner: u32, daemon_uid: u32) -> CoreResult<()> {
        if owner != daemon_uid {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!(
                    "{what} is owned by uid {owner}, but the daemon user is uid {daemon_uid}"
                ),
            });
        }
        Ok(())
    }

    fn validate_data_dir(handle: &File, path: &Path) -> CoreResult<()> {
        let metadata = handle.metadata().map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        if !metadata.is_dir() {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: "data directory is not a directory".to_string(),
            });
        }
        let daemon_uid = rustix::process::geteuid().as_raw();
        validate_owner("data directory", path, metadata.uid(), daemon_uid)?;
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!(
                    "data directory mode {mode:o} is group/world writable; expected no group/world write"
                ),
            });
        }
        Ok(())
    }

    fn validate_owner_only_file(file: &File, path: &Path, what: &str) -> CoreResult<()> {
        let metadata = file.metadata().map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        if !metadata.is_file() {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!("{what} is not a regular file"),
            });
        }
        let daemon_uid = rustix::process::geteuid().as_raw();
        validate_owner(what, path, metadata.uid(), daemon_uid)?;
        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!("{what} mode {mode:o} is not 0600"),
            });
        }
        Ok(())
    }

    fn open_error(path: &Path, what: &str, error: Errno) -> CoreError {
        if error == Errno::LOOP {
            CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!("{what} is a symlink; refusing to follow it"),
            }
        } else {
            CoreError::KeyStore {
                path: path.display().to_string(),
                reason: error.to_string(),
            }
        }
    }

    fn try_open_key(dir: &File, path: &Path) -> CoreResult<Option<File>> {
        match rustix::fs::openat(
            dir.as_fd(),
            ENGINEERING_SIGNING_KEY_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Some(File::from(fd))),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(open_error(path, "signing key", error)),
        }
    }

    fn try_open_history(dir: &File, path: &Path) -> CoreResult<Option<File>> {
        match rustix::fs::openat(
            dir.as_fd(),
            ENGINEERING_KEY_HISTORY_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Some(File::from(fd))),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(open_error(path, "signing key history", error)),
        }
    }

    fn signer_from_file(file: &mut File, path: &Path) -> CoreResult<ReceiptSigner> {
        validate_owner_only_file(file, path, "signing key")?;
        let metadata = file.metadata().map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        if metadata.len() != SIGNING_KEY_BYTES as u64 {
            return Err(CoreError::KeyStore {
                path: path.display().to_string(),
                reason: format!(
                    "signing key must be exactly {SIGNING_KEY_BYTES} bytes, got {}",
                    metadata.len()
                ),
            });
        }
        let mut seed = [0u8; SIGNING_KEY_BYTES];
        file.read_exact(&mut seed)
            .map_err(|error| CoreError::KeyStore {
                path: path.display().to_string(),
                reason: format!("signing key must be exactly {SIGNING_KEY_BYTES} bytes: {error}"),
            })?;
        Ok(ReceiptSigner::from_seed(seed))
    }

    fn read_history(dir: &File, data_dir: &Path) -> CoreResult<Option<KeyHistory>> {
        let path = key_history_path(data_dir);
        let Some(mut file) = try_open_history(dir, &path)? else {
            return Ok(None);
        };
        validate_owner_only_file(&file, &path, "signing key history")?;
        let len = file
            .metadata()
            .map_err(|error| CoreError::KeyStore {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?
            .len();
        if len > MAX_KEY_HISTORY_BYTES {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!(
                    "key history is {len} bytes, above the {MAX_KEY_HISTORY_BYTES}-byte bound"
                ),
            });
        }
        let mut bytes = Vec::with_capacity(len as usize);
        (&mut file)
            .take(MAX_KEY_HISTORY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| CoreError::KeyStore {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?;
        if bytes.len() as u64 > MAX_KEY_HISTORY_BYTES {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!("key history exceeds the {MAX_KEY_HISTORY_BYTES}-byte bound"),
            });
        }
        let history: KeyHistory =
            serde_json::from_slice(&bytes).map_err(|error| CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!("key history is not valid JSON: {error}"),
            })?;
        history.validate()?;
        Ok(Some(history))
    }

    fn write_history(dir: &File, data_dir: &Path, history: &KeyHistory) -> CoreResult<()> {
        history.validate()?;
        let path = key_history_path(data_dir);
        let bytes = serde_json::to_vec_pretty(history).map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        if bytes.len() as u64 > MAX_KEY_HISTORY_BYTES {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!("key history exceeds the {MAX_KEY_HISTORY_BYTES}-byte bound"),
            });
        }
        let fd = rustix::fs::openat(
            dir.as_fd(),
            ENGINEERING_KEY_HISTORY_FILE,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from(0o600),
        )
        .map_err(|error| open_error(&path, "signing key history", error))?;
        let mut file = File::from(fd);
        rustix::fs::fchmod(&file, Mode::from(0o600)).map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        file.write_all(&bytes)
            .map_err(|error| CoreError::KeyStore {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?;
        file.sync_all().map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })
    }

    fn create_key_file(dir: &File, path: &Path, replace: bool) -> CoreResult<bool> {
        let mut flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if replace {
            flags |= OFlags::TRUNC;
        } else {
            flags |= OFlags::EXCL;
        }
        let fd = match rustix::fs::openat(
            dir.as_fd(),
            ENGINEERING_SIGNING_KEY_FILE,
            flags,
            Mode::from(0o600),
        ) {
            Ok(fd) => fd,
            Err(Errno::EXIST) if !replace => return Ok(false),
            Err(error) => return Err(open_error(path, "signing key", error)),
        };
        let mut file = File::from(fd);
        rustix::fs::fchmod(&file, Mode::from(0o600)).map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        let signer = ReceiptSigner::generate();
        let seed: [u8; SIGNING_KEY_BYTES] = *signer.key.as_bytes();
        file.write_all(&seed).map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        file.sync_all().map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        Ok(true)
    }

    fn with_history_validity(
        dir: &File,
        data_dir: &Path,
        signer: ReceiptSigner,
        create_history: bool,
        default_not_before_ms: u64,
    ) -> CoreResult<ReceiptSigner> {
        let mut history = read_history(dir, data_dir)?.unwrap_or(KeyHistory {
            version: KEY_HISTORY_VERSION,
            keys: Vec::new(),
        });
        let key_id = signer.key_id();
        let entry = match history.entry_for(&key_id).cloned() {
            Some(entry) => entry,
            None if create_history => {
                if let Some(active) = history.active() {
                    return Err(CoreError::KeyStoreRefused {
                        path: key_history_path(data_dir).display().to_string(),
                        reason: format!(
                            "signing key {key_id} is not the active key {} recorded in the key history",
                            active.key_id
                        ),
                    });
                }
                let entry = KeyHistoryEntry::new(&signer, default_not_before_ms);
                history.keys.push(entry.clone());
                write_history(dir, data_dir, &history)?;
                entry
            }
            None => {
                return Err(CoreError::KeyStoreRefused {
                    path: key_history_path(data_dir).display().to_string(),
                    reason: format!("signing key {key_id} is not recorded in the key history"),
                });
            }
        };
        let signer = signer.with_validity(entry.validity());
        if let Some(retired_at_ms) = entry.retired_at_ms {
            return Err(CoreError::RetiredSigningKey {
                key_id,
                retired_at_ms,
            });
        }
        Ok(signer)
    }

    pub(super) fn load_signing_key(data_dir: &Path) -> CoreResult<ReceiptSigner> {
        let dir = AnchoredDir::open(data_dir)?;
        let path = signing_key_path(data_dir);
        let mut file = try_open_key(&dir.handle, &path)?.ok_or_else(|| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: "signing key does not exist".to_string(),
        })?;
        let signer = signer_from_file(&mut file, &path)?;
        if read_history(&dir.handle, data_dir)?.is_some() {
            with_history_validity(&dir.handle, data_dir, signer, false, 0)
        } else {
            Ok(signer)
        }
    }

    pub(super) fn load_or_create_signing_key(data_dir: &Path) -> CoreResult<ReceiptSigner> {
        ensure_data_dir(data_dir)?;
        let dir = AnchoredDir::open(data_dir)?;
        let path = signing_key_path(data_dir);
        let now_ms = unix_time_ms();
        for _ in 0..2 {
            if let Some(mut file) = try_open_key(&dir.handle, &path)? {
                let signer = signer_from_file(&mut file, &path)?;
                return with_history_validity(&dir.handle, data_dir, signer, true, 0);
            }
            if !create_key_file(&dir.handle, &path, false)? {
                continue;
            }
            let mut file =
                try_open_key(&dir.handle, &path)?.ok_or_else(|| CoreError::KeyStore {
                    path: path.display().to_string(),
                    reason: "created signing key disappeared".to_string(),
                })?;
            let signer = signer_from_file(&mut file, &path)?;
            return with_history_validity(&dir.handle, data_dir, signer, true, now_ms);
        }
        Err(CoreError::KeyStore {
            path: path.display().to_string(),
            reason: "concurrent signing key creation did not settle".to_string(),
        })
    }

    pub(super) fn load_key_history(data_dir: &Path) -> CoreResult<KeyHistory> {
        let dir = AnchoredDir::open(data_dir)?;
        read_history(&dir.handle, data_dir)?.ok_or_else(|| CoreError::KeyStore {
            path: key_history_path(data_dir).display().to_string(),
            reason: "key history does not exist".to_string(),
        })
    }

    pub(super) fn load_trusted_verifier(data_dir: &Path) -> CoreResult<ReceiptVerifier> {
        let dir = AnchoredDir::open(data_dir)?;
        if let Some(history) = read_history(&dir.handle, data_dir)? {
            return ReceiptVerifier::trusting_history(&history);
        }
        let path = signing_key_path(data_dir);
        let mut file = try_open_key(&dir.handle, &path)?.ok_or_else(|| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: "signing key does not exist".to_string(),
        })?;
        let signer = signer_from_file(&mut file, &path)?;
        ReceiptVerifier::trusting_key_id(&signer.key_id())
    }

    pub(super) fn rotate_signing_key(
        data_dir: &Path,
        retired_at_ms: u64,
    ) -> CoreResult<ReceiptSigner> {
        ensure_data_dir(data_dir)?;
        let dir = AnchoredDir::open(data_dir)?;
        let path = signing_key_path(data_dir);
        let mut history = read_history(&dir.handle, data_dir)?.unwrap_or(KeyHistory {
            version: KEY_HISTORY_VERSION,
            keys: Vec::new(),
        });
        if let Some(mut file) = try_open_key(&dir.handle, &path)? {
            let signer = signer_from_file(&mut file, &path)?;
            let key_id = signer.key_id();
            let entry =
                if let Some(entry) = history.keys.iter_mut().find(|entry| entry.key_id == key_id) {
                    entry
                } else {
                    history.keys.push(KeyHistoryEntry::new(&signer, 0));
                    history.keys.last_mut().expect("the entry was just pushed")
                };
            if retired_at_ms < entry.not_before_ms {
                return Err(CoreError::KeyStoreRefused {
                    path: key_history_path(data_dir).display().to_string(),
                    reason: format!(
                        "cannot retire key {key_id} at {retired_at_ms} before its validity starts at {}",
                        entry.not_before_ms
                    ),
                });
            }
            entry.retired_at_ms = Some(retired_at_ms);
        } else if let Some(active) = history.active() {
            return Err(CoreError::KeyStoreRefused {
                path: path.display().to_string(),
                reason: format!(
                    "signing key file is missing but the key history records active key {}",
                    active.key_id
                ),
            });
        }
        write_history(&dir.handle, data_dir, &history)?;
        create_key_file(&dir.handle, &path, true)?;
        let mut file = try_open_key(&dir.handle, &path)?.ok_or_else(|| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: "rotated signing key disappeared".to_string(),
        })?;
        let signer = signer_from_file(&mut file, &path)?;
        let entry = KeyHistoryEntry::new(&signer, retired_at_ms);
        history.keys.push(entry.clone());
        write_history(&dir.handle, data_dir, &history)?;
        Ok(signer.with_validity(entry.validity()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_key_owner_that_is_not_the_daemon_user_is_refused() {
            let path = Path::new("/tmp/engineering-signing.key");
            let error = validate_owner("signing key", path, 1000, 501).unwrap_err();
            assert!(matches!(error, CoreError::KeyStoreRefused { .. }));
            assert!(error.to_string().contains("owned by uid 1000"), "{error}");
            validate_owner("signing key", path, 501, 501).unwrap();
        }
    }
}
