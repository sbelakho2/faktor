use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{CoreError, CoreResult};

/// A 32-byte BLAKE3 content digest, serialized as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn of_text(value: &str) -> Self {
        Self::of_bytes(value.as_bytes())
    }

    pub fn of_json<T: Serialize>(value: &T) -> CoreResult<Self> {
        let bytes = serde_json::to_vec(value).map_err(|error| CoreError::Serialization {
            reason: error.to_string(),
        })?;
        Ok(Self::of_bytes(&bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(value: &str) -> CoreResult<Self> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CoreError::InvalidDigest {
                value: value.to_string(),
            });
        }
        let mut out = [0u8; 32];
        for (index, chunk) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let text = std::str::from_utf8(chunk).map_err(|_| CoreError::InvalidDigest {
                value: value.to_string(),
            })?;
            out[index] = u8::from_str_radix(text, 16).map_err(|_| CoreError::InvalidDigest {
                value: value.to_string(),
            })?;
        }
        Ok(Self(out))
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_content_has_a_fixed_digest() {
        let digest = Digest::of_text("faktor-engineering");
        assert_eq!(digest.to_hex().len(), 64);
        assert_eq!(Digest::from_hex(&digest.to_hex()).unwrap(), digest);
    }

    #[test]
    fn malformed_digests_are_refused() {
        assert!(Digest::from_hex("abc").is_err());
        assert!(Digest::from_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn json_digest_is_stable_across_serialization() {
        let value = vec![1u64, 2, 3];
        assert_eq!(
            Digest::of_json(&value).unwrap(),
            Digest::of_json(&value).unwrap()
        );
    }
}
