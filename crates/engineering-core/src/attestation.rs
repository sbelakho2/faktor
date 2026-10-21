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

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
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
/// A signing seed is exactly one Ed25519 seed.
pub const SIGNING_KEY_BYTES: usize = 32;
pub const MAX_TRUSTED_KEYS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Canonical bytes signed by the attestation: a domain tag plus the receipt
/// digest, so a signature cannot be replayed as another message.
fn signing_message(digest: Digest) -> Vec<u8> {
    let mut message = Vec::with_capacity(ATTESTATION_DOMAIN.len() + 32);
    message.extend_from_slice(ATTESTATION_DOMAIN);
    message.extend_from_slice(digest.as_bytes());
    message
}

/// In-process signer. The seed is zeroized with the `SigningKey`; `Debug`
/// never prints key material.
#[derive(Clone)]
pub struct ReceiptSigner {
    key: SigningKey,
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
        }
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

    /// Sign the one canonical receipt digest. The receipt must already be
    /// structurally valid; signing a receipt with a lying execution record
    /// would only authenticate a lie.
    pub fn sign_receipt(&self, receipt: &mut AnalysisReceipt) -> CoreResult<()> {
        receipt.validate()?;
        let digest = receipt.canonical_digest()?;
        let signature = self.key.sign(&signing_message(digest));
        receipt.attestation = Some(ReceiptAttestation {
            algorithm: ATTESTATION_ALGORITHM.to_string(),
            key_id: self.key_id(),
            public_key: self.public_key_hex(),
            signature: to_hex(&signature.to_bytes()),
            digest,
        });
        Ok(())
    }
}

/// Verifies the enclosed signature and, when a trust set is configured, that
/// the signing key is one of the trusted daemon keys.
#[derive(Debug, Clone, Default)]
pub struct ReceiptVerifier {
    trusted: BTreeSet<String>,
    require_trust: bool,
}

impl ReceiptVerifier {
    /// Verify the Ed25519 signature under the key embedded in the receipt.
    /// This proves integrity only; use [`Self::trusting`] for identity.
    pub fn self_attested() -> Self {
        Self {
            trusted: BTreeSet::new(),
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
        let mut trusted = BTreeSet::new();
        for key_id in key_ids {
            let key_id = key_id.into();
            let _ = parse_fixed_hex::<32>("trusted_key_id", &key_id)?;
            trusted.insert(key_id.to_ascii_lowercase());
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

    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.trusted.iter().map(String::as_str)
    }

    pub fn verify(&self, receipt: &AnalysisReceipt) -> CoreResult<()> {
        receipt.verify_attestation()?;
        if self.require_trust {
            let Some(attestation) = &receipt.attestation else {
                return Err(CoreError::MissingReceiptAttestation);
            };
            if !self.trusted.contains(&attestation.key_id) {
                return Err(CoreError::UntrustedReceiptKey {
                    key_id: attestation.key_id.clone(),
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
            .verify(&signing_message(expected), &signature)
            .map_err(|_| CoreError::ReceiptSignatureMismatch)
    }
}

/// The daemon-local signing-key path inside `data_dir`.
pub fn signing_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ENGINEERING_SIGNING_KEY_FILE)
}

/// Load the daemon-local key without creating one. Verification must not
/// mint a key that silently makes stored receipts unverifiable.
pub fn load_signing_key(data_dir: &Path) -> CoreResult<ReceiptSigner> {
    let path = signing_key_path(data_dir);
    let mut file = std::fs::File::open(&path).map_err(|error| CoreError::KeyStore {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    validate_key_file(&file, &path)?;
    let mut seed = [0u8; SIGNING_KEY_BYTES];
    file.read_exact(&mut seed)
        .map_err(|error| CoreError::KeyStore {
            path: path.display().to_string(),
            reason: format!("signing key must be exactly {SIGNING_KEY_BYTES} bytes: {error}"),
        })?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing).unwrap_or(0) != 0 {
        return Err(CoreError::KeyStore {
            path: path.display().to_string(),
            reason: format!("signing key must be exactly {SIGNING_KEY_BYTES} bytes"),
        });
    }
    Ok(ReceiptSigner::from_seed(seed))
}

/// Load the daemon-local key, creating it on first use (32 random bytes,
/// owner-only `0600` on unix). The bytes are never logged or exported.
pub fn load_or_create_signing_key(data_dir: &Path) -> CoreResult<ReceiptSigner> {
    let path = signing_key_path(data_dir);
    if path.exists() {
        return load_signing_key(data_dir);
    }
    std::fs::create_dir_all(data_dir).map_err(|error| CoreError::KeyStore {
        path: data_dir.display().to_string(),
        reason: error.to_string(),
    })?;
    let signer = ReceiptSigner::generate();
    let seed: [u8; SIGNING_KEY_BYTES] = *signer.key.as_bytes();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        // A concurrent first use won the race: adopt the existing key.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return load_signing_key(data_dir);
        }
        Err(error) => {
            return Err(CoreError::KeyStore {
                path: path.display().to_string(),
                reason: error.to_string(),
            })
        }
    };
    file.write_all(&seed).map_err(|error| CoreError::KeyStore {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    file.sync_all().map_err(|error| CoreError::KeyStore {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| CoreError::KeyStore {
                path: path.display().to_string(),
                reason: error.to_string(),
            })?;
    }
    load_signing_key(data_dir)
}

fn validate_key_file(file: &std::fs::File, path: &Path) -> CoreResult<()> {
    let metadata = file.metadata().map_err(|error| CoreError::KeyStore {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(CoreError::KeyStore {
            path: path.display().to_string(),
            reason: "signing key is not a regular file".to_string(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = metadata.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(CoreError::KeyStore {
                path: path.display().to_string(),
                reason: format!(
                    "signing key mode {mode:o} is accessible to other users; expected 0600"
                ),
            });
        }
    }
    Ok(())
}
