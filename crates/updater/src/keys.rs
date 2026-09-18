//! The operator key allowlist and ed25519 verification.
//!
//! The signature shape mirrors the repository's certification evidence
//! (`scripts/certification/evidence.mjs`): `algorithm` + `identity` +
//! `public_key` (base64 raw 32 bytes) + `value` (base64 64-byte signature),
//! over the CANONICAL JSON of the manifest with the `signature` field
//! removed (object keys sorted recursively, compact separators).
//!
//! Two independent checks guard authenticity: the embedded public key must
//! EQUAL the allowlisted key for that identity, and the signature must
//! verify against it. An empty allowlist refuses every manifest (unknown
//! key) — there is no implicit trust anchor.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// One allowlisted operator identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    identity: String,
    public_key: [u8; 32],
}

impl TrustedKey {
    /// Build one allowlisted key from its raw base64 public key (exactly 32
    /// bytes). Malformed key material is a configuration error, never a
    /// silently dropped trust anchor.
    pub fn from_base64(identity: &str, public_key_base64: &str) -> Result<Self, String> {
        if identity.is_empty() || identity.len() > 128 || !identity.is_ascii() {
            return Err(format!(
                "updater key identity {identity:?} must be 1..=128 ASCII bytes"
            ));
        }
        let raw = BASE64
            .decode(public_key_base64)
            .map_err(|e| format!("updater key {identity:?}: public key is not base64: {e}"))?;
        let public_key: [u8; 32] = raw.try_into().map_err(|raw: Vec<u8>| {
            format!(
                "updater key {identity:?}: public key must be 32 raw bytes (got {})",
                raw.len()
            )
        })?;
        // Reject non-canonical/weak points at load time instead of at the
        // first verification.
        VerifyingKey::from_bytes(&public_key)
            .map_err(|e| format!("updater key {identity:?}: invalid ed25519 point: {e}"))?;
        Ok(TrustedKey {
            identity: identity.to_string(),
            public_key,
        })
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.public_key)
    }

    fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey::from_bytes(&self.public_key)
            .expect("allowlisted keys were validated at construction")
    }

    /// Verify one detached signature (base64 64 bytes) over `payload`.
    pub fn verify(&self, payload: &[u8], signature_base64: &str) -> Result<(), String> {
        let raw = BASE64
            .decode(signature_base64)
            .map_err(|e| format!("signature is not base64: {e}"))?;
        let bytes: [u8; 64] = raw
            .try_into()
            .map_err(|raw: Vec<u8>| format!("signature must be 64 bytes (got {})", raw.len()))?;
        let signature = Signature::from_bytes(&bytes);
        self.verifying_key()
            .verify(payload, &signature)
            .map_err(|e| format!("signature does not verify: {e}"))
    }
}

/// The allowlist: an ordered set of operator identities. Duplicate
/// identities are refused (an ambiguous allowlist is never accepted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedKeys {
    keys: Vec<TrustedKey>,
}

impl TrustedKeys {
    pub fn new(keys: Vec<TrustedKey>) -> Result<Self, String> {
        let mut seen: Vec<&str> = Vec::with_capacity(keys.len());
        for key in &keys {
            if seen.contains(&key.identity()) {
                return Err(format!(
                    "updater key identity {:?} is configured twice",
                    key.identity()
                ));
            }
            seen.push(key.identity());
        }
        Ok(TrustedKeys { keys })
    }

    /// The empty allowlist: every manifest is refused as an unknown key.
    pub fn empty() -> Self {
        TrustedKeys { keys: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn get(&self, identity: &str) -> Option<&TrustedKey> {
        self.keys.iter().find(|k| k.identity() == identity)
    }

    /// The allowlisted identities, in configuration order.
    pub fn identities(&self) -> Vec<&str> {
        self.keys.iter().map(|k| k.identity()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ed25519 public key: arbitrary 32-byte strings are not points
    /// and are refused at load time.
    fn public_key(seed: u8) -> String {
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        BASE64.encode(key.verifying_key().to_bytes())
    }

    #[test]
    fn malformed_key_material_is_refused_not_dropped() {
        // Each hostile input is refused with the message naming the exact
        // rule, so a broken allowlist cannot be mistaken for a weak key.
        let not_base64 = TrustedKey::from_base64("op", "not base64!!").unwrap_err();
        assert!(not_base64.contains("not base64"), "{not_base64}");
        let short = TrustedKey::from_base64("op", &BASE64.encode([0u8; 31])).unwrap_err();
        assert!(short.contains("32 raw bytes"), "{short}");
        let empty_id = TrustedKey::from_base64("", &public_key(1)).unwrap_err();
        assert!(empty_id.contains("identity"), "{empty_id}");
        let key = TrustedKey::from_base64("op", &public_key(1)).unwrap();
        assert_eq!(key.identity(), "op");
        // A 32-byte string that does not decompress to a curve point is a
        // typed config refusal, never an accepted trust anchor.
        let refused = (0u8..=255)
            .step_by(17)
            .filter(|b| TrustedKey::from_base64("op", &BASE64.encode([*b; 32])).is_err())
            .count();
        assert!(refused > 0, "point validation must refuse non-points");
    }

    #[test]
    fn duplicate_identities_are_refused() {
        let key = TrustedKey::from_base64("op", &public_key(7)).unwrap();
        let single = TrustedKeys::new(vec![key.clone()]).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single.identities(), vec!["op"]);
        let dup = TrustedKeys::new(vec![key.clone(), key]).unwrap_err();
        assert!(dup.contains("configured twice"), "{dup}");
        assert!(TrustedKeys::empty().is_empty());
        assert!(TrustedKeys::empty().get("op").is_none());
        assert_eq!(TrustedKeys::empty().len(), 0);
    }

    #[test]
    fn a_wrong_length_signature_is_refused_not_panicking() {
        let key = TrustedKey::from_base64("op", &public_key(7)).unwrap();
        assert!(key.verify(b"payload", &BASE64.encode([0u8; 63])).is_err());
        assert!(key.verify(b"payload", "%%%%").is_err());
    }
}
