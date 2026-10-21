//! Human-operator Ed25519 keys and the durable public-key registry.
//!
//! A private seed is written owner-only (`0600` on unix) under
//! `<data-dir>/approval/operators/<principal>.key`. The registry records only
//! public keys. Possession of a seed plus the explicit `approval decide` CLI
//! is the human authority; no agent tool can register or use a key.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::model::{
    to_hex, ApprovalError, OperatorKeyHandle, Principal, PrincipalKind, MAX_PRINCIPAL_ID_BYTES,
};

/// Directory (inside the data dir) holding the approval ledger and keys.
pub const OPERATOR_DIRECTORY: &str = "approval";
/// Public-key registry filename.
pub const OPERATOR_REGISTRY_FILE: &str = "operators.json";
const KEY_SUFFIX: &str = ".key";
const SEED_BYTES: usize = 32;
const REGISTRY_SCHEMA: &str = "faktor.approval.operators/v1";

/// An in-process human-operator signing key.
pub struct OperatorKey {
    key: SigningKey,
}

impl std::fmt::Debug for OperatorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorKey")
            .field("key_id", &self.key_id())
            .finish_non_exhaustive()
    }
}

impl OperatorKey {
    /// Generate a fresh operator key.
    pub fn generate() -> Self {
        use rand::Rng as _;
        let mut seed = [0u8; SEED_BYTES];
        rand::rng().fill(&mut seed);
        Self::from_seed(seed)
    }

    /// Reconstruct from a 32-byte seed.
    pub fn from_seed(seed: [u8; SEED_BYTES]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// Raw public key (lowercase hex).
    pub fn public_key_hex(&self) -> String {
        to_hex(self.key.verifying_key().as_bytes())
    }

    /// BLAKE3 fingerprint of the public key.
    pub fn key_id(&self) -> String {
        to_hex(blake3::hash(self.key.verifying_key().as_bytes()).as_bytes())
    }

    pub(crate) fn handle(&self) -> OperatorKeyHandle<'_> {
        OperatorKeyHandle {
            key: &self.key,
            public_key: *self.key.verifying_key().as_bytes(),
        }
    }
}

/// One registered human operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredOperator {
    /// Human principal id.
    pub principal_id: String,
    /// Public-key fingerprint.
    pub key_id: String,
    /// Raw public key (lowercase hex).
    pub public_key: String,
    /// Registration time.
    pub registered_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperatorRegistryFile {
    schema: String,
    operators: Vec<RegisteredOperator>,
}

impl Default for OperatorRegistryFile {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA.to_string(),
            operators: Vec::new(),
        }
    }
}

/// The approval directory inside a data dir.
pub fn approval_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(OPERATOR_DIRECTORY)
}

/// Registry path.
pub fn registry_path(data_dir: &Path) -> PathBuf {
    approval_dir(data_dir).join(OPERATOR_REGISTRY_FILE)
}

/// Key path for one human principal.
pub fn operator_key_path(data_dir: &Path, principal_id: &str) -> Result<PathBuf, ApprovalError> {
    if principal_id.is_empty()
        || principal_id.len() > MAX_PRINCIPAL_ID_BYTES
        || principal_id == "."
        || principal_id == ".."
        || principal_id.contains('/')
        || principal_id.contains('\\')
        || !principal_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApprovalError::Invalid(
            "principal id is not a safe key filename".to_string(),
        ));
    }
    Ok(approval_dir(data_dir).join(format!("{principal_id}{KEY_SUFFIX}")))
}

/// Every operator key path in the approval directory (private seeds; only
/// used for existence/audit reporting, never read in bulk).
pub fn operator_key_paths(data_dir: &Path) -> Result<Vec<PathBuf>, ApprovalError> {
    let directory = approval_dir(data_dir);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ApprovalError::Store(error.to_string())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ApprovalError::Store(error.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("key") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Save a human-operator seed owner-only. Refuses to overwrite.
pub fn save_operator_key(
    data_dir: &Path,
    principal_id: &str,
    key: &OperatorKey,
    replace: bool,
) -> Result<PathBuf, ApprovalError> {
    let path = operator_key_path(data_dir, principal_id)?;
    let parent = path
        .parent()
        .ok_or_else(|| ApprovalError::Store("key path has no parent".to_string()))?;
    std::fs::create_dir_all(parent).map_err(|error| ApprovalError::Store(error.to_string()))?;
    if path.exists() && !replace {
        return Err(ApprovalError::Conflict(format!(
            "operator key {} already exists; use --replace only after rotating the registry",
            path.display()
        )));
    }
    let seed = key.key.as_bytes();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| ApprovalError::Store(error.to_string()))?;
    file.write_all(seed)
        .map_err(|error| ApprovalError::Store(error.to_string()))?;
    file.sync_all()
        .map_err(|error| ApprovalError::Store(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| ApprovalError::Store(error.to_string()))?;
    }
    Ok(path)
}

/// Load a human-operator seed. Refuses group/world-readable files.
pub fn load_operator_key(path: &Path) -> Result<OperatorKey, ApprovalError> {
    let mut file =
        std::fs::File::open(path).map_err(|error| ApprovalError::Store(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| ApprovalError::Store(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ApprovalError::Store(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = metadata.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ApprovalError::Store(format!(
                "operator key {} has mode {mode:o}; expected 0600",
                path.display()
            )));
        }
    }
    let mut seed = [0u8; SEED_BYTES];
    file.read_exact(&mut seed)
        .map_err(|error| ApprovalError::Store(error.to_string()))?;
    let mut trailing = [0u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|error| ApprovalError::Store(error.to_string()))?
        != 0
    {
        return Err(ApprovalError::Store(
            "operator key must be exactly 32 bytes".to_string(),
        ));
    }
    Ok(OperatorKey::from_seed(seed))
}

/// Load the public-key registry, failing typed on corruption.
pub fn load_registry(data_dir: &Path) -> Result<Vec<RegisteredOperator>, ApprovalError> {
    let path = registry_path(data_dir);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ApprovalError::Store(error.to_string())),
    };
    if raw.len() > 1 << 20 {
        return Err(ApprovalError::Store(
            "operator registry exceeds 1 MiB".to_string(),
        ));
    }
    let file: OperatorRegistryFile = serde_json::from_slice(&raw)
        .map_err(|error| ApprovalError::Store(format!("operator registry: {error}")))?;
    if file.schema != REGISTRY_SCHEMA {
        return Err(ApprovalError::Store(format!(
            "operator registry schema {:?} is not {REGISTRY_SCHEMA:?}",
            file.schema
        )));
    }
    for operator in &file.operators {
        let principal =
            Principal::new(operator.principal_id.clone(), PrincipalKind::HumanOperator)?;
        principal.validate()?;
        if operator.public_key.len() != 64
            || !operator
                .public_key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApprovalError::Store(
                "operator registry carries a malformed public key".to_string(),
            ));
        }
        let expected = to_hex(blake3::hash(&parse_hex(&operator.public_key)?).as_bytes());
        if operator.key_id != expected {
            return Err(ApprovalError::Store(
                "operator registry key id does not fingerprint its public key".to_string(),
            ));
        }
    }
    Ok(file.operators)
}

/// Register (or idempotently re-register) a human operator public key.
pub fn register_operator_key(
    data_dir: &Path,
    principal_id: &str,
    key: &OperatorKey,
    now_ms: u64,
) -> Result<RegisteredOperator, ApprovalError> {
    let principal = Principal::new(principal_id.to_string(), PrincipalKind::HumanOperator)?;
    // The same safe-filename rule as the key store: a registry entry that
    // cannot name its own key file is unusable and could shadow a path.
    operator_key_path(data_dir, &principal.id)?;
    if now_ms == 0 {
        return Err(ApprovalError::Invalid(
            "operator registration time must be non-zero".to_string(),
        ));
    }
    let mut operators = load_registry(data_dir)?;
    let record = RegisteredOperator {
        principal_id: principal.id.clone(),
        key_id: key.key_id(),
        public_key: key.public_key_hex(),
        registered_at_ms: now_ms,
    };
    if let Some(existing) = operators.iter().find(|entry| entry.key_id == record.key_id) {
        if existing.principal_id != record.principal_id {
            return Err(ApprovalError::Conflict(format!(
                "operator key {} is already registered to principal {}",
                record.key_id, existing.principal_id
            )));
        }
        return Ok(existing.clone());
    }
    if operators
        .iter()
        .any(|entry| entry.principal_id == record.principal_id)
    {
        return Err(ApprovalError::Conflict(format!(
            "human principal {} already has a registered operator key",
            record.principal_id
        )));
    }
    operators.push(record.clone());
    operators.sort_by(|left, right| {
        left.principal_id
            .cmp(&right.principal_id)
            .then(left.key_id.cmp(&right.key_id))
    });
    let file = OperatorRegistryFile {
        schema: REGISTRY_SCHEMA.to_string(),
        operators,
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|error| ApprovalError::Store(format!("registry encode: {error}")))?;
    let directory = approval_dir(data_dir);
    std::fs::create_dir_all(&directory).map_err(|error| ApprovalError::Store(error.to_string()))?;
    faktor_fs::atomic::atomic_replace(&registry_path(data_dir), &bytes)
        .map_err(|error| ApprovalError::Store(error.to_string()))?;
    Ok(record)
}

pub(crate) fn trusted_key_ids(
    data_dir: &Path,
) -> Result<std::collections::BTreeSet<String>, ApprovalError> {
    Ok(load_registry(data_dir)?
        .into_iter()
        .map(|operator| operator.key_id)
        .collect())
}

pub(crate) fn key_registered_for(
    data_dir: &Path,
    principal_id: &str,
    key: &OperatorKey,
) -> Result<(), ApprovalError> {
    let operators = load_registry(data_dir)?;
    let key_id = key.key_id();
    if operators.iter().any(|operator| {
        operator.principal_id == principal_id
            && operator.key_id == key_id
            && operator.public_key == key.public_key_hex()
    }) {
        return Ok(());
    }
    Err(ApprovalError::UntrustedOperator { key_id })
}

fn parse_hex(value: &str) -> Result<Vec<u8>, ApprovalError> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApprovalError::Store("malformed hex value".to_string()));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().as_chunks::<2>().0.iter() {
        let text = std::str::from_utf8(chunk)
            .map_err(|_| ApprovalError::Store("malformed hex value".to_string()))?;
        out.push(
            u8::from_str_radix(text, 16)
                .map_err(|_| ApprovalError::Store("malformed hex value".to_string()))?,
        );
    }
    Ok(out)
}
