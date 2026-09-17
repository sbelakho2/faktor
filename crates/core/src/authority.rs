//! Canonical BLAKE3 authority digests.
//!
//! Authority identities in Faktor must never rest on truncated (64-bit FNV)
//! or concatenation-ambiguous hashes: every identity that can authorize a
//! verification, a proof reuse, an integration, a change set or a semantic
//! memory fact goes through [`authority_digest`] — one low-level BLAKE3
//! construction with a documented canonical serialization.
//!
//! # Canonical serialization (v1)
//!
//! ```text
//! digest  = BLAKE3( serialize(domain, version, fields) )
//! serialize(domain, version, fields) =
//!     u64_le(byte_len(domain)) ++ domain
//!     ++ u64_le(version)
//!     ++ field_1 ++ field_2 ++ ... ++ field_n
//! field_i = u64_le(byte_len(bytes_i)) ++ bytes_i
//! ```
//!
//! Properties this construction guarantees:
//!
//! - **Domain separation.** `domain` is length-prefixed and hashed before
//!   `version` and every field, so the same payload under two different
//!   domains (or two versions of one domain) digests differently.
//! - **Ordered, boundary-safe fields.** Fields are ordered and individually
//!   length-prefixed: `["ab", "c"]` and `["a", "bc"]` digest differently,
//!   and no field can be shifted into its neighbour.
//! - **Deterministic, platform-independent.** No `serde_json`, no map
//!   iteration, no locale/pointer/whitespace dependence: only the domain,
//!   the version and the field bytes participate.
//! - **No truncation.** The full 256-bit BLAKE3 output is the identity.
//!   Authority paths must never carry a 16-hex (64-bit) value.
//!
//! # Legacy FNV digests
//!
//! Digests written before this API (`fnv1a64:<16-hex>`, bare 16-hex
//! 64-bit FNV values, `accounting:v1:<16-hex>`) still DECODE for viewing,
//! but they never authorize new work: [`classify_authority_digest`] detects
//! them as [`AuthorityDigestKind::LegacyFnv`] and
//! [`refuse_legacy_authority_digest`] turns that detection into the typed
//! [`LegacyAuthorityDigest`] error, forcing restage/reverification.

/// Hard bound on one canonical field (1 MiB). Callers bound their own
/// inputs; this is the last-resort cap that keeps a hostile durable row
/// from being digested unboundedly.
pub const MAX_AUTHORITY_FIELD_BYTES: usize = 1 << 20;

/// The canonical field sink of [`authority_digest`]. Callers never touch the
/// hasher directly: every value enters as one length-prefixed field.
pub struct CanonicalFieldWriter {
    hasher: blake3::Hasher,
    fields: u64,
}

impl CanonicalFieldWriter {
    /// Append one length-prefixed field. Oversized fields are still
    /// length-prefixed (the length is exact), so callers that know their
    /// bounds never need to pre-check; [`Self::field`] only documents the
    /// [`MAX_AUTHORITY_FIELD_BYTES`] contract.
    pub fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.hasher.update(&(bytes.len() as u64).to_le_bytes());
        self.hasher.update(bytes);
        self.fields += 1;
        self
    }

    /// Append one UTF-8 field.
    pub fn text(&mut self, value: &str) -> &mut Self {
        self.field(value.as_bytes())
    }

    /// Append one unsigned 64-bit field (little-endian, fixed width).
    pub fn uint(&mut self, value: u64) -> &mut Self {
        self.field(&value.to_le_bytes())
    }

    /// Append one optional text field: `[0]` for `None`, `[1] ++ bytes` for
    /// `Some` (one field, so `None` and `Some("")` never alias).
    pub fn opt_text(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(value) => {
                let mut encoded = Vec::with_capacity(1 + value.len());
                encoded.push(1);
                encoded.extend_from_slice(value.as_bytes());
                self.field(&encoded)
            }
            None => self.field(&[0]),
        }
    }

    /// The number of fields written so far.
    pub const fn field_count(&self) -> u64 {
        self.fields
    }
}

/// A value that writes itself into the canonical field stream of
/// [`authority_digest`]. Implementations MUST be deterministic and MUST NOT
/// mutate observable state.
pub trait CanonicalFields {
    /// Write every canonical field of `self`, in order.
    fn write_fields(&self, out: &mut CanonicalFieldWriter);
}

/// The canonical authority digest: `BLAKE3` over the documented module
/// serialization of `domain`, `version` and `fields`.
pub fn authority_digest(
    domain: &'static [u8],
    version: u64,
    fields: impl CanonicalFields,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update(&version.to_le_bytes());
    let mut out = CanonicalFieldWriter { hasher, fields: 0 };
    fields.write_fields(&mut out);
    out.hasher.finalize()
}

/// The canonical authority digest as BARE lowercase 64-hex (the shape the
/// durable ledger validators require).
pub fn authority_digest_hex(
    domain: &'static [u8],
    version: u64,
    fields: impl CanonicalFields,
) -> String {
    authority_digest(domain, version, fields)
        .to_hex()
        .to_string()
}

/// The canonical authority digest as `blake3:<64-hex>` — the labelled shape
/// used where a digest may sit beside legacy/foreign values and must be
/// unmistakable.
pub fn authority_digest_labeled(
    domain: &'static [u8],
    version: u64,
    fields: impl CanonicalFields,
) -> String {
    format!("blake3:{}", authority_digest_hex(domain, version, fields))
}

impl CanonicalFields for str {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        out.field(self.as_bytes());
    }
}

impl CanonicalFields for String {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        out.field(self.as_bytes());
    }
}

impl CanonicalFields for [String] {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        for item in self {
            out.field(item.as_bytes());
        }
    }
}

impl CanonicalFields for Vec<String> {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        self.as_slice().write_fields(out);
    }
}

impl CanonicalFields for [&str] {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        for item in self {
            out.field(item.as_bytes());
        }
    }
}

impl CanonicalFields for [u8] {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        out.field(self);
    }
}

impl<T: CanonicalFields + ?Sized> CanonicalFields for &T {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        (**self).write_fields(out);
    }
}

impl<T: CanonicalFields> CanonicalFields for Option<T> {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        match self {
            Some(value) => {
                out.uint(1);
                value.write_fields(out);
            }
            None => {
                out.uint(0);
            }
        }
    }
}

impl CanonicalFields for () {
    fn write_fields(&self, _out: &mut CanonicalFieldWriter) {}
}

macro_rules! canonical_uint {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl CanonicalFields for $ty {
                fn write_fields(&self, out: &mut CanonicalFieldWriter) {
                    out.field(&(*self as u64).to_le_bytes());
                }
            }
        )+
    };
}

canonical_uint!(u8, u16, u32, u64, usize);

impl CanonicalFields for i64 {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        out.field(&self.to_le_bytes());
    }
}

impl CanonicalFields for bool {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        out.field(if *self { &[1] } else { &[0] });
    }
}

/// An ordered builder for mixed structured payloads (text, numbers, optional
/// text, string lists) — the ergonomic form of [`CanonicalFields`] for call
/// sites whose fields have different types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    fields: Vec<Vec<u8>>,
}

impl Fields {
    /// An empty field stream.
    pub const fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// Append one text field.
    pub fn text(mut self, value: &str) -> Self {
        self.fields.push(value.as_bytes().to_vec());
        self
    }

    /// Append one optional text field (`[0]` / `[1] ++ bytes`).
    pub fn opt_text(mut self, value: Option<&str>) -> Self {
        match value {
            Some(value) => {
                let mut encoded = Vec::with_capacity(1 + value.len());
                encoded.push(1);
                encoded.extend_from_slice(value.as_bytes());
                self.fields.push(encoded);
            }
            None => self.fields.push(vec![0]),
        }
        self
    }

    /// Append one unsigned 64-bit field.
    pub fn uint(mut self, value: u64) -> Self {
        self.fields.push(value.to_le_bytes().to_vec());
        self
    }

    /// Append one signed 64-bit field.
    pub fn int(mut self, value: i64) -> Self {
        self.fields.push(value.to_le_bytes().to_vec());
        self
    }

    /// Append one raw byte field.
    pub fn raw(mut self, value: &[u8]) -> Self {
        self.fields.push(value.to_vec());
        self
    }

    /// Append every entry of a string list as its own field.
    pub fn list(mut self, items: &[String]) -> Self {
        for item in items {
            self.fields.push(item.as_bytes().to_vec());
        }
        self
    }

    /// Append every raw field of a nested stream.
    pub fn extend(mut self, other: Fields) -> Self {
        self.fields.extend(other.fields);
        self
    }

    /// The number of fields collected.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether no field was collected.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl CanonicalFields for Fields {
    fn write_fields(&self, out: &mut CanonicalFieldWriter) {
        for field in &self.fields {
            out.field(field);
        }
    }
}

/// The algorithm class of one stored authority-digest value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthorityDigestKind {
    /// `blake3:<64-lowercase-hex>`.
    Blake3Labeled,
    /// Bare 64-lowercase-hex BLAKE3.
    Blake3Hex,
    /// A 64-bit FNV-1a value in any of the shapes the pre-BLAKE3 code
    /// wrote: `fnv1a64:<16-hex>`, bare 16-hex, `accounting:v1:<16-hex>`.
    LegacyFnv,
    /// The empty string (an honest absence, never a digest).
    Empty,
    /// Anything else: a labelled foreign digest or a corrupt value. Never
    /// treated as a canonical authority digest.
    Unknown,
}

impl AuthorityDigestKind {
    /// Whether this value may authorize new work.
    pub const fn is_authoritative(self) -> bool {
        matches!(self, Self::Blake3Labeled | Self::Blake3Hex)
    }
}

/// Classify one stored digest value.
pub fn classify_authority_digest(value: &str) -> AuthorityDigestKind {
    if value.is_empty() {
        return AuthorityDigestKind::Empty;
    }
    if let Some(hex) = value.strip_prefix("blake3:") {
        return if is_lower_hex(hex, 64) {
            AuthorityDigestKind::Blake3Labeled
        } else {
            AuthorityDigestKind::Unknown
        };
    }
    if is_lower_hex(value, 64) {
        return AuthorityDigestKind::Blake3Hex;
    }
    if let Some(hex) = value.strip_prefix("fnv1a64:") {
        return if is_lower_hex(hex, 16) {
            AuthorityDigestKind::LegacyFnv
        } else {
            AuthorityDigestKind::Unknown
        };
    }
    if let Some(hex) = value.strip_prefix("accounting:v1:") {
        return if is_lower_hex(hex, 16) {
            AuthorityDigestKind::LegacyFnv
        } else {
            AuthorityDigestKind::Unknown
        };
    }
    if is_lower_hex(value, 16) {
        return AuthorityDigestKind::LegacyFnv;
    }
    AuthorityDigestKind::Unknown
}

/// The typed refusal of a legacy FNV authority digest: such a value may be
/// viewed, but it can never authorize a proof reuse, an integration, a
/// change-set promotion or any other new authority decision — the affected
/// run must be restaged and reverified under canonical BLAKE3 identities.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "legacy FNV authority digest for {what} ({value:?}) cannot authorize new verification/proof reuse; restage and reverify under the canonical BLAKE3 authority digests"
)]
pub struct LegacyAuthorityDigest {
    /// What the legacy digest identified (proof basis field, change set,
    /// base map, accounting snapshot, ...).
    pub what: &'static str,
    /// The legacy value exactly as stored.
    pub value: String,
}

/// Fail closed on a legacy FNV digest: `Ok(())` for any other class (the
/// caller's own shape validation stays authoritative for unknown values).
pub fn refuse_legacy_authority_digest(
    what: &'static str,
    value: &str,
) -> Result<(), LegacyAuthorityDigest> {
    match classify_authority_digest(value) {
        AuthorityDigestKind::LegacyFnv => Err(LegacyAuthorityDigest {
            what,
            value: value.to_string(),
        }),
        _ => Ok(()),
    }
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ------------------------------------------------------------ domain table

/// `CriterionBinding::content_digest`.
pub const DOMAIN_CRITERION_BINDING: &[u8] = b"faktor.criterion-binding";
/// `command_binding_digest` / `command_binding_digest_parts`.
pub const DOMAIN_COMMAND_BINDING: &[u8] = b"faktor.check-command";
/// `ProofBasis::task_contract_digest` / the fingerprint's
/// `task_contract_hash`.
pub const DOMAIN_TASK_CONTRACT: &[u8] = b"faktor.task-contract";
/// The `ProofBasis::integration_sources_digest` source list.
pub const DOMAIN_INTEGRATION_SOURCES: &[u8] = b"faktor.integration-sources";
/// The `ProofBasis::changed_files_digest` / integration file list.
pub const DOMAIN_CHANGED_FILES: &[u8] = b"faktor.changed-files";
/// `ChangeSet::id`.
pub const DOMAIN_CHANGE_SET: &[u8] = b"faktor.change-set";
/// `base_map_digest`.
pub const DOMAIN_BASE_MAP: &[u8] = b"faktor.base-map";
/// The run-base copy's `manifest_digest`.
pub const DOMAIN_RUN_BASE_MANIFEST: &[u8] = b"faktor.run-base-manifest";
/// The fingerprint's `check_argv_cwd_env_hash`.
pub const DOMAIN_CHECK_BASIS: &[u8] = b"faktor.check-basis";
/// `check_execution_digest`.
pub const DOMAIN_CHECK_EXECUTION: &[u8] = b"faktor.check-execution";
/// The completion-accounting snapshot identity.
pub const DOMAIN_ACCOUNTING_BALANCE: &[u8] = b"faktor.accounting-balance";
/// A deterministic semantic memory-fact id.
pub const DOMAIN_SEMANTIC_FACT: &[u8] = b"faktor.semantic-fact";
/// The single-snapshot candidate/baseline manifest hash of a
/// `CandidateProofRef`.
pub const DOMAIN_CANDIDATE_MANIFEST: &[u8] = b"faktor.candidate-manifest";

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(items: &[&str]) -> Fields {
        let mut out = Fields::new();
        for item in items {
            out = out.text(item);
        }
        out
    }

    // Golden vectors: pinned BLAKE3 values for the documented serialization.
    // Regenerating any of them means the canonical identity of every durable
    // row written before the change is invalidated — never edit one casually.
    const GOLDEN_EMPTY: &str = "292008c54b2ea11cfc4578c93b5d8ab059fb100d33b7c47add6434b5a1f4c6f0";
    const GOLDEN_LIST: &str = "4de1da1d6ad32e56234103acf68a17fea75b340b2d95e3a079912bcd03a0c382";
    const GOLDEN_MIXED: &str = "a3ad04c6278601b0324cac0a11df676b42323a15c6d044f7a9332c90b807258f";
    const GOLDEN_COMMAND: &str = "3572e6d597aab6af7dd16171c4dd356e9eef5da0e53452fef30fadc394449c9b";

    #[test]
    fn golden_vectors_per_domain_are_pinned() {
        let empty = authority_digest_hex(DOMAIN_SEMANTIC_FACT, 1, ());
        assert_eq!(empty, GOLDEN_EMPTY, "empty payload golden vector drifted");
        let list = authority_digest_hex(
            DOMAIN_CHANGED_FILES,
            1,
            vec!["a".to_string(), "b".to_string()],
        );
        assert_eq!(list, GOLDEN_LIST, "list golden vector drifted");
        let mixed = authority_digest_labeled(
            DOMAIN_CRITERION_BINDING,
            1,
            Fields::new()
                .text("file_state")
                .text("src/lib.rs")
                .text("blake3:abc")
                .opt_text(None)
                .uint(7),
        );
        assert_eq!(
            mixed,
            format!("blake3:{GOLDEN_MIXED}"),
            "mixed golden vector drifted"
        );
        let command = authority_digest_labeled(
            DOMAIN_COMMAND_BINDING,
            1,
            Fields::new().text("cargo").text("test").text("--workspace"),
        );
        assert_eq!(
            command,
            format!("blake3:{GOLDEN_COMMAND}"),
            "command golden vector drifted"
        );
    }

    #[test]
    fn golden_vectors_match_an_independent_reserialization() {
        // The same documented spec, serialized by hand instead of through
        // the library writer: any drift between the writer and the spec is
        // a hard failure.
        fn manual(domain: &[u8], version: u64, fields: &[&[u8]]) -> String {
            let mut h = blake3::Hasher::new();
            h.update(&(domain.len() as u64).to_le_bytes());
            h.update(domain);
            h.update(&version.to_le_bytes());
            for field in fields {
                h.update(&(field.len() as u64).to_le_bytes());
                h.update(field);
            }
            h.finalize().to_hex().to_string()
        }
        assert_eq!(
            authority_digest_hex(DOMAIN_SEMANTIC_FACT, 1, ()),
            manual(DOMAIN_SEMANTIC_FACT, 1, &[])
        );
        assert_eq!(
            authority_digest_hex(
                DOMAIN_CHANGED_FILES,
                1,
                vec!["a".to_string(), "b".to_string()]
            ),
            manual(DOMAIN_CHANGED_FILES, 1, &[b"a", b"b"])
        );
        let mixed = authority_digest(
            DOMAIN_CRITERION_BINDING,
            1,
            Fields::new().text("x").uint(9).opt_text(Some("y")),
        );
        assert_eq!(
            mixed.to_hex().to_string(),
            manual(
                DOMAIN_CRITERION_BINDING,
                1,
                &[b"x", &9u64.to_le_bytes(), b"\x01y"]
            )
        );
    }

    #[test]
    fn same_fields_same_digest_regardless_of_path() {
        let a = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["child", "base", "start"]));
        let b = authority_digest_hex(
            DOMAIN_CHANGE_SET,
            1,
            Fields::new().text("child").text("base").text("start"),
        );
        let c = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["child", "base", "start"]));
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn field_boundaries_are_not_confusable() {
        let left = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["ab", "c"]));
        let right = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["a", "bc"]));
        assert_ne!(left, right, "['ab','c'] must never alias ['a','bc']");
        let split = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["a", "b c"]));
        let whole = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["a b", "c"]));
        assert_ne!(split, whole);
        let none = authority_digest_hex(DOMAIN_CHANGE_SET, 1, Fields::new().opt_text(None));
        let empty = authority_digest_hex(DOMAIN_CHANGE_SET, 1, Fields::new().opt_text(Some("")));
        assert_ne!(none, empty, "None must never alias Some(\"\")");
    }

    #[test]
    fn domain_and_version_separate_identities() {
        let payload = fields(&["same", "payload"]);
        let a = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["same", "payload"]));
        let b = authority_digest_hex(DOMAIN_BASE_MAP, 1, fields(&["same", "payload"]));
        let c = authority_digest_hex(DOMAIN_CHANGE_SET, 2, payload);
        assert_ne!(a, b, "the same payload under two domains must differ");
        assert_ne!(a, c, "the same payload under two versions must differ");
        let v1 = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["x"]));
        let moved = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["", "x"]));
        assert_ne!(v1, moved);
    }

    #[test]
    fn large_and_utf8_fields_are_deterministic() {
        let big = "x".repeat(256 * 1024);
        let a = authority_digest_hex(DOMAIN_SEMANTIC_FACT, 1, big.as_str());
        let b = authority_digest_hex(DOMAIN_SEMANTIC_FACT, 1, big.as_str());
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let utf8 = authority_digest_hex(DOMAIN_SEMANTIC_FACT, 1, "héllo\u{1f600}");
        assert_eq!(utf8.len(), 64);
    }

    #[test]
    fn legacy_shapes_classify_and_refuse() {
        for legacy in [
            "fnv1a64:0123456789abcdef",
            "0123456789abcdef",
            "accounting:v1:fedcba9876543210",
        ] {
            assert_eq!(
                classify_authority_digest(legacy),
                AuthorityDigestKind::LegacyFnv,
                "{legacy} must classify as legacy FNV"
            );
            let err = refuse_legacy_authority_digest("proof basis", legacy).unwrap_err();
            assert_eq!(err.what, "proof basis");
            assert_eq!(err.value, legacy);
            assert!(err.to_string().contains("restage and reverify"));
        }
        let blake3_labeled = authority_digest_labeled(DOMAIN_CHANGE_SET, 1, fields(&["x"]));
        assert_eq!(
            classify_authority_digest(&blake3_labeled),
            AuthorityDigestKind::Blake3Labeled
        );
        let bare = authority_digest_hex(DOMAIN_CHANGE_SET, 1, fields(&["x"]));
        assert_eq!(
            classify_authority_digest(&bare),
            AuthorityDigestKind::Blake3Hex
        );
        assert_eq!(classify_authority_digest(""), AuthorityDigestKind::Empty);
        assert_eq!(
            classify_authority_digest("sha256:whatever"),
            AuthorityDigestKind::Unknown
        );
        // A hostile 64-hex value must never be mistaken for a 64-bit FNV
        // value, and a hostile over-long hex for a BLAKE3 digest.
        assert_eq!(
            classify_authority_digest(&"a".repeat(16)),
            AuthorityDigestKind::LegacyFnv
        );
        assert_eq!(
            classify_authority_digest(&"a".repeat(64)),
            AuthorityDigestKind::Blake3Hex
        );
        assert_eq!(
            classify_authority_digest(&"a".repeat(65)),
            AuthorityDigestKind::Unknown
        );
        assert_eq!(
            classify_authority_digest(&"A".repeat(64)),
            AuthorityDigestKind::Unknown,
            "uppercase hex is never a canonical authority digest"
        );
        assert!(refuse_legacy_authority_digest("x", &bare).is_ok());
    }
}
