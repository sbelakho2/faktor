// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir,separate-process
//! Payment-token vault: provider tokens are AEAD-sealed before they touch
//! durable storage and are never rendered in `Debug`, exports or routes.
//!
//! The key is resolved from the configured environment-variable NAME when
//! present (64 hex characters or base64 for exactly 32 bytes); otherwise an
//! owner-only key file beside the payment database is created once. In-memory
//! stores get an ephemeral key, so test tokens are never written in the clear.
//! Sealing binds the ciphertext to `(organization_id, payment_method_id)` via
//! AEAD associated data, so a copied row cannot be replayed into another
//! organization/method.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::{STANDARD_NO_PAD as B64_STD, URL_SAFE_NO_PAD as B64_URL};
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use zeroize::Zeroizing;

use crate::error::PaymentError;
use crate::secrets::{SecretSource, SecretString};

/// Nonce length for AES-256-GCM.
const NONCE_LEN: usize = 12;
/// Master key length.
pub const VAULT_KEY_LEN: usize = 32;
/// Ciphertext version prefix (`FAKTOR Payment Vault v1`).
const SEAL_PREFIX: &[u8; 4] = b"FPV1";
/// Bound on one provider token accepted by the vault.
pub const MAX_TOKEN_BYTES: usize = 256;

/// The AEAD token vault.
#[derive(Clone)]
pub struct PaymentTokenVault {
    key: Zeroizing<[u8; VAULT_KEY_LEN]>,
}

impl std::fmt::Debug for PaymentTokenVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentTokenVault")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl PaymentTokenVault {
    /// Resolve the key from the named environment variable, else load/create
    /// the owner-only key file beside `db_path`. An in-memory store gets an
    /// ephemeral key that dies with the process.
    pub fn resolve(
        secrets: &dyn SecretSource,
        env_name: &str,
        db_path: Option<&Path>,
    ) -> Result<Self, PaymentError> {
        if let Some(raw) = secrets.get(env_name) {
            let key = decode_key(raw.expose())?;
            return Ok(Self {
                key: Zeroizing::new(key),
            });
        }
        match db_path {
            Some(path) => Self::load_or_create_key_file(path),
            None => Self::ephemeral(),
        }
    }

    pub fn ephemeral() -> Result<Self, PaymentError> {
        let mut key = [0u8; VAULT_KEY_LEN];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| PaymentError::Store("payment vault key entropy unavailable".into()))?;
        Ok(Self {
            key: Zeroizing::new(key),
        })
    }

    fn key_path(db_path: &Path) -> PathBuf {
        let mut name = db_path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| "payment.db".into());
        name.push(".vault-key");
        db_path.with_file_name(name)
    }

    fn load_or_create_key_file(db_path: &Path) -> Result<Self, PaymentError> {
        let path = Self::key_path(db_path);
        match std::fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() != VAULT_KEY_LEN {
                    return Err(PaymentError::Store(format!(
                        "payment vault key file {} must contain exactly {VAULT_KEY_LEN} bytes",
                        path.display()
                    )));
                }
                let mut key = [0u8; VAULT_KEY_LEN];
                key.copy_from_slice(&bytes);
                Ok(Self {
                    key: Zeroizing::new(key),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0u8; VAULT_KEY_LEN];
                SystemRandom::new().fill(&mut key).map_err(|_| {
                    PaymentError::Store("payment vault key entropy unavailable".into())
                })?;
                if let Err(create_error) = write_owner_only(&path, &key) {
                    // Two processes may construct the service against the
                    // same fresh database at once. `create_new` lets exactly
                    // one win; the loser must load the winner's key instead
                    // of failing every token operation for this process.
                    match std::fs::read(&path) {
                        Ok(bytes) if bytes.len() == VAULT_KEY_LEN => {
                            let mut shared = [0u8; VAULT_KEY_LEN];
                            shared.copy_from_slice(&bytes);
                            return Ok(Self {
                                key: Zeroizing::new(shared),
                            });
                        }
                        _ => return Err(create_error),
                    }
                }
                Ok(Self {
                    key: Zeroizing::new(key),
                })
            }
            Err(error) => Err(PaymentError::Store(format!(
                "read payment vault key {}: {error}",
                path.display()
            ))),
        }
    }

    /// Seal one provider token. The returned string is safe to persist; it
    /// is not a credential usable at the provider.
    pub fn seal(
        &self,
        organization_id: &str,
        payment_method_id: &str,
        token: &str,
    ) -> Result<String, PaymentError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(PaymentError::Malformed(format!(
                "payment token must be 1..={MAX_TOKEN_BYTES} bytes"
            )));
        }
        let aad = aad(organization_id, payment_method_id);
        let key = UnboundKey::new(&AES_256_GCM, &self.key[..])
            .map_err(|_| PaymentError::Store("payment vault key is unusable".into()))?;
        let key = LessSafeKey::new(key);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| PaymentError::Store("payment vault nonce entropy unavailable".into()))?;
        let mut out = token.as_bytes().to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(aad.as_bytes()),
            &mut out,
        )
        .map_err(|_| PaymentError::Store("payment token sealing failed".into()))?;
        let mut sealed = Vec::with_capacity(SEAL_PREFIX.len() + NONCE_LEN + out.len());
        sealed.extend_from_slice(SEAL_PREFIX);
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&out);
        Ok(format!("v1:{}", B64_URL.encode(sealed)))
    }

    /// Open one stored seal. A wrong organization/method, truncated blob or
    /// tampered ciphertext is a typed store refusal, never a plaintext guess.
    pub fn open(
        &self,
        organization_id: &str,
        payment_method_id: &str,
        sealed: &str,
    ) -> Result<SecretString, PaymentError> {
        let encoded = sealed.strip_prefix("v1:").ok_or_else(|| {
            PaymentError::Store("payment token ciphertext has an unknown version".into())
        })?;
        let bytes = B64_URL.decode(encoded).map_err(|_| {
            PaymentError::Store("payment token ciphertext is not valid base64".into())
        })?;
        if bytes.len() < SEAL_PREFIX.len() + NONCE_LEN + AES_256_GCM.tag_len()
            || &bytes[..SEAL_PREFIX.len()] != SEAL_PREFIX
        {
            return Err(PaymentError::Store(
                "payment token ciphertext is truncated or has the wrong prefix".into(),
            ));
        }
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes.copy_from_slice(&bytes[SEAL_PREFIX.len()..SEAL_PREFIX.len() + NONCE_LEN]);
        let mut ciphertext = bytes[SEAL_PREFIX.len() + NONCE_LEN..].to_vec();
        let aad = aad(organization_id, payment_method_id);
        let key = UnboundKey::new(&AES_256_GCM, &self.key[..])
            .map_err(|_| PaymentError::Store("payment vault key is unusable".into()))?;
        let key = LessSafeKey::new(key);
        let plaintext = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad.as_bytes()),
                &mut ciphertext,
            )
            .map_err(|_| {
                PaymentError::Store(
                    "payment token ciphertext failed authentication (wrong vault key or tenant)"
                        .into(),
                )
            })?;
        let text = std::str::from_utf8(plaintext)
            .map_err(|_| PaymentError::Store("payment token is not UTF-8".into()))?;
        if text.is_empty() || text.len() > MAX_TOKEN_BYTES {
            return Err(PaymentError::Store(
                "decrypted payment token is outside the accepted bound".into(),
            ));
        }
        Ok(SecretString::new(text.to_string()))
    }
}

fn aad(organization_id: &str, payment_method_id: &str) -> String {
    format!("faktor-payments:v1:{organization_id}:{payment_method_id}")
}

fn decode_key(raw: &str) -> Result<[u8; VAULT_KEY_LEN], PaymentError> {
    let trimmed = raw.trim();
    let bytes =
        if trimmed.len() == VAULT_KEY_LEN * 2 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut out = Vec::with_capacity(VAULT_KEY_LEN);
            for pair in trimmed.as_bytes().as_chunks::<2>().0 {
                let text = std::str::from_utf8(pair)
                    .map_err(|_| PaymentError::Config("payment vault key is not hex".into()))?;
                out.push(
                    u8::from_str_radix(text, 16)
                        .map_err(|_| PaymentError::Config("payment vault key is not hex".into()))?,
                );
            }
            out
        } else {
            B64_STD
                .decode(trimmed)
                .or_else(|_| B64_URL.decode(trimmed))
                .map_err(|_| {
                    PaymentError::Config(
                        "payment vault key must be 64 hex characters or base64 for 32 bytes".into(),
                    )
                })?
        };
    if bytes.len() != VAULT_KEY_LEN {
        return Err(PaymentError::Config(format!(
            "payment vault key must decode to exactly {VAULT_KEY_LEN} bytes"
        )));
    }
    let mut key = [0u8; VAULT_KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PaymentError> {
    use std::io::Write;
    // Write the complete key to a private temporary file, fsync it, then
    // publish it with an atomic no-replace hard link. A concurrent reader
    // therefore never observes a partially written key file, and exactly one
    // process wins the first-use race.
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = directory.join(format!(".vault-key.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|error| {
            PaymentError::Store(format!(
                "create payment vault key {}: {error}",
                temp.display()
            ))
        })?;
        file.write_all(bytes).map_err(|error| {
            PaymentError::Store(format!(
                "write payment vault key {}: {error}",
                temp.display()
            ))
        })?;
        file.sync_all().map_err(|error| {
            PaymentError::Store(format!(
                "sync payment vault key {}: {error}",
                temp.display()
            ))
        })?;
        std::fs::hard_link(&temp, path).map_err(|error| {
            PaymentError::Store(format!(
                "publish payment vault key {}: {error}",
                path.display()
            ))
        })
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::StaticSecrets;

    #[test]
    fn seal_is_tenant_bound_and_never_plaintext() {
        let vault = PaymentTokenVault::ephemeral().unwrap();
        let sealed = vault
            .seal("org-a", "pm_1", "pm_stripe_token_value")
            .unwrap();
        assert!(!sealed.contains("pm_stripe_token_value"));
        assert_eq!(
            vault.open("org-a", "pm_1", &sealed).unwrap().expose(),
            "pm_stripe_token_value"
        );
        assert!(vault.open("org-b", "pm_1", &sealed).is_err());
        assert!(vault.open("org-a", "pm_2", &sealed).is_err());
        let mut tampered = sealed.clone();
        tampered.push('A');
        assert!(vault.open("org-a", "pm_1", &tampered).is_err());
    }

    #[test]
    fn env_key_and_key_file_are_strict() {
        let key_hex = "11".repeat(VAULT_KEY_LEN);
        let secrets = StaticSecrets::new([("VAULT_KEY".to_string(), key_hex.clone())]);
        let vault = PaymentTokenVault::resolve(&secrets, "VAULT_KEY", None).unwrap();
        let sealed = vault.seal("org", "pm", "tok_value").unwrap();
        let same = PaymentTokenVault::resolve(&secrets, "VAULT_KEY", None).unwrap();
        assert_eq!(
            same.open("org", "pm", &sealed).unwrap().expose(),
            "tok_value"
        );
        let bad = StaticSecrets::new([("VAULT_KEY".to_string(), "short".to_string())]);
        assert!(PaymentTokenVault::resolve(&bad, "VAULT_KEY", None).is_err());
    }

    #[test]
    fn concurrent_first_use_shares_one_key_without_failing() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("payment.db");
        let barrier = Arc::new(Barrier::new(16));
        let mut threads = Vec::new();
        for index in 0..16 {
            let db = db.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let vault =
                    PaymentTokenVault::resolve(&StaticSecrets::default(), "ABSENT", Some(&db))
                        .expect("a losing key-file creator loads the winner key");
                let sealed = vault.seal("org", "pm", &format!("tok_{index}")).unwrap();
                (vault, sealed, index)
            }));
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        let shared = results[0].0.clone();
        for (vault, sealed, index) in results {
            assert_eq!(
                vault.open("org", "pm", &sealed).unwrap().expose(),
                format!("tok_{index}")
            );
            assert_eq!(
                shared.open("org", "pm", &sealed).unwrap().expose(),
                format!("tok_{index}"),
                "every first-use process must share one key"
            );
        }
    }

    #[test]
    fn key_file_is_owner_only_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("payment.db");
        let vault =
            PaymentTokenVault::resolve(&StaticSecrets::default(), "ABSENT", Some(&db)).unwrap();
        let sealed = vault.seal("org", "pm", "tok_value").unwrap();
        let reopened =
            PaymentTokenVault::resolve(&StaticSecrets::default(), "ABSENT", Some(&db)).unwrap();
        assert_eq!(
            reopened.open("org", "pm", &sealed).unwrap().expose(),
            "tok_value"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("payment.db.vault-key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "vault key must not be group/world readable"
            );
        }
    }
}
