//! BOM (bill of materials) requests: bounded, deterministic, restart-stable.
//!
//! A BOM is a bounded list of `(query, quantity)` lines (≤
//! [`MAX_BOM_LINES`]). Every line gets a stable [`BomItem::key`] derived from
//! the normalized query and the quantity, so a job item resumed after a crash
//! is recognized as the same item, and two identical requests produce the
//! same [`Bom::digest`].

use serde::{Deserialize, Serialize};

use crate::quantity::{NonZeroQuantity, MAX_ORDER_QUANTITY};
use crate::query::{canonical_query, RequestError, MAX_QUERY_BYTES};
use crate::text::Text;

pub use crate::query::MAX_BOM_LINES;

/// Domain separator for BOM item keys. Bump when the encoding changes.
const BOM_ITEM_DOMAIN: &[u8] = b"faktor-commerce.bom-item/v1\0";
/// Domain separator for the BOM digest.
const BOM_DIGEST_DOMAIN: &[u8] = b"faktor-commerce.bom/v1\0";

/// One BOM line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BomItem {
    /// The free-text query (≤ 512 bytes).
    pub q: Text<{ MAX_QUERY_BYTES }>,
    /// The requested quantity (1..=1e9).
    pub qty: NonZeroQuantity,
}

impl BomItem {
    /// Validate and construct.
    pub fn new(q: &str, qty: u64) -> Result<Self, RequestError> {
        let q = Text::<MAX_QUERY_BYTES>::new(q)
            .map_err(|error| RequestError::new("items.q", error.to_string()))?;
        let qty = NonZeroQuantity::new(qty).map_err(|error| {
            RequestError::new(
                "items.qty",
                format!("{error} (maximum {MAX_ORDER_QUANTITY})"),
            )
        })?;
        Ok(Self { q, qty })
    }

    /// The stable item key: `BLAKE3(normalized query ‖ quantity)`.
    pub fn key(&self) -> String {
        item_key(self.q.as_str(), self.qty.get())
    }
}

/// The stable key of one BOM line. The query uses the NON-LOSSY
/// [`canonical_query`] form: punctuation is identity-bearing, so `AB#123`
/// and `AB123` are different lines (case/whitespace-equivalent spellings
/// still share one key).
pub fn item_key(query: &str, quantity: u64) -> String {
    let normalized = canonical_query(query);
    let mut hasher = blake3::Hasher::new();
    hasher.update(BOM_ITEM_DOMAIN);
    hasher.update(&(normalized.len() as u64).to_le_bytes());
    hasher.update(normalized.as_bytes());
    hasher.update(&quantity.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

/// A validated BOM: 1..=500 lines, each line within its bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bom(Vec<BomItem>);

impl Bom {
    /// Validate and construct.
    pub fn new(items: Vec<BomItem>) -> Result<Self, RequestError> {
        if items.is_empty() {
            return Err(RequestError::new(
                "items",
                "a BOM must have at least one line",
            ));
        }
        if items.len() > MAX_BOM_LINES {
            return Err(RequestError::new(
                "items",
                format!("{} lines exceeds the maximum {MAX_BOM_LINES}", items.len()),
            ));
        }
        Ok(Self(items))
    }

    /// Build from raw `(query, quantity)` pairs.
    pub fn from_pairs(pairs: &[(&str, u64)]) -> Result<Self, RequestError> {
        let items = pairs
            .iter()
            .map(|(q, qty)| BomItem::new(q, *qty))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(items)
    }

    /// The lines, in request order.
    pub fn items(&self) -> &[BomItem] {
        &self.0
    }

    /// The line count.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`: a [`Bom`] is validated non-empty at construction.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Re-validate (also used when loading a durable job request).
    pub fn validate(&self) -> Result<(), RequestError> {
        if self.0.is_empty() || self.0.len() > MAX_BOM_LINES {
            return Err(RequestError::new(
                "items",
                format!("{} lines is outside 1..={MAX_BOM_LINES}", self.0.len()),
            ));
        }
        for item in &self.0 {
            if item.q.len() > MAX_QUERY_BYTES {
                return Err(RequestError::new("items.q", "query exceeds 512 bytes"));
            }
        }
        Ok(())
    }

    /// The stable BOM digest: `BLAKE3` over the ordered item keys.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(BOM_DIGEST_DOMAIN);
        hasher.update(&(self.0.len() as u64).to_le_bytes());
        for item in &self.0 {
            let key = item.key();
            hasher.update(key.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_is_bounded_at_500_lines() {
        let lines: Vec<BomItem> = (0..MAX_BOM_LINES)
            .map(|i| BomItem::new(&format!("PART-{i:04}"), 100).expect("line"))
            .collect();
        assert_eq!(Bom::new(lines).expect("500 lines").len(), MAX_BOM_LINES);

        let too_many: Vec<BomItem> = (0..MAX_BOM_LINES + 1)
            .map(|i| BomItem::new(&format!("PART-{i:04}"), 100).expect("line"))
            .collect();
        let error = Bom::new(too_many).expect_err("501 lines must be refused");
        assert_eq!(error.field, "items");

        assert!(Bom::new(Vec::new()).is_err());
        assert!(BomItem::new("x", 0).is_err());
        assert!(BomItem::new("x", 1_000_000_001).is_err());
        assert!(BomItem::new(&"x".repeat(MAX_QUERY_BYTES + 1), 1).is_err());
    }

    #[test]
    fn item_keys_are_stable_and_quantity_sensitive() {
        let a = BomItem::new("TPS5430DDAR", 100).expect("line");
        let b = BomItem::new("  tps5430ddar ", 100).expect("line");
        assert_eq!(a.key(), b.key());
        let c = BomItem::new("TPS5430DDAR", 101).expect("line");
        assert_ne!(a.key(), c.key());
        assert_eq!(a.key().len(), 64);
    }

    #[test]
    fn item_keys_never_erase_identity_punctuation() {
        let hashed = BomItem::new("AB#123", 10).expect("line");
        let plain = BomItem::new("AB123", 10).expect("line");
        assert_ne!(
            hashed.key(),
            plain.key(),
            "punctuation is identity-bearing: AB#123 and AB123 are different lines"
        );
        // The lossy free-text search normalization stays a separate function.
        assert_eq!(crate::query::normalize_query("AB#123"), "ab123");
        assert_eq!(crate::query::canonical_query("AB#123"), "ab#123");
        assert_eq!(crate::query::canonical_query(" AB#123 "), "ab#123");
    }

    #[test]
    fn bom_line_keys_are_stable_unique_and_keep_the_digest() {
        let first = BomItem::new(" DUP ", 10).expect("line");
        let second = BomItem::new("dup", 10).expect("line");
        assert_eq!(first.key(), second.key(), "content keys still normalize");
        assert_eq!(
            crate::jobs::bom_line_key(0, &first),
            crate::jobs::bom_line_key(0, &second),
            "the same content at the same ordinal is the same line"
        );
        assert_ne!(
            crate::jobs::bom_line_key(0, &first),
            crate::jobs::bom_line_key(1, &first),
            "two duplicates are two distinct durable lines"
        );
        // The BOM digest stays the content digest of the request.
        let forward = Bom::from_pairs(&[("dup", 10), ("DUP", 10)]).expect("bom");
        let again = Bom::from_pairs(&[("dup", 10), ("dup", 10)]).expect("bom");
        assert_eq!(forward.digest(), again.digest());
    }

    #[test]
    fn bom_digest_is_order_sensitive_and_deterministic() {
        let forward = Bom::from_pairs(&[("a", 1), ("b", 2)]).expect("bom");
        let backward = Bom::from_pairs(&[("b", 2), ("a", 1)]).expect("bom");
        let again = Bom::from_pairs(&[("a", 1), ("b", 2)]).expect("bom");
        assert_eq!(forward.digest(), again.digest());
        assert_ne!(forward.digest(), backward.digest());
        assert_eq!(forward.digest().len(), 64);
    }
}
