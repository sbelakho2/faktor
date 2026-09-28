//! The first-class durable Task object (audit 25) + the locked-down task
//! state machine (audit P0-7) + the first-class VerificationRecord (audit
//! P0-8).
//!
//! A `Task` is the durable object a session works on: a goal, the
//! acceptance criteria derived from that goal (goal + project checks,
//! seeded once), an append-only ordered step plan, and a durable budget
//! envelope (`max_tokens`/`max_turns` vs crash-safe `spent_tokens`/
//! `spent_turns`). It lives in typed store rows (`task`, schema v10) that
//! survive IDE close, daemon restart, OS restart, provider switch and
//! context compaction — compaction never rewrites them and they are never
//! FIFO-evicted.
//!
//! # Revisions (schema v14)
//!
//! Every effective state/criteria/plan/budget mutation bumps the row's
//! monotonic `revision` exactly once, in the same transaction as the
//! mutation. The revision is the row's optimistic-lock token: the
//! transition and completion APIs take the caller's `expected_revision`
//! and refuse with a typed error when the row moved on.
//!
//! # State machine enforcement (audit P0-7)
//!
//! `VerifiedComplete` (and the states that lead to it, `NeedsVerification`
//! and `Verifying`) can NEVER be assigned through a generic patch:
//! [`SessionHandle::update_task`] rejects any patch carrying a
//! completion-relevant state with [`TaskError::CompletionStateViaPatch`]
//! and any patch that would jump a non-completion machine edge with
//! [`TaskError::IllegalTransition`]. The legal producers are exactly:
//!
//! - [`SessionHandle::transition_task`] — one named [`TaskTransition`]
//!   edge; `VerifiedComplete` has no edge and is unreachable here;
//! - [`SessionHandle::complete_verified_task`] — the ONLY path to
//!   `VerifiedComplete`, validated in ONE store transaction against a
//!   passing [`VerificationRecord`] that certifies the task's current
//!   revision and covers every current acceptance criterion.
//!
//! The store backstops the same rule at the row level: a raw
//! `task` upsert that would move a row INTO a completion-relevant state is
//! refused unless the machine allows the edge (or the row already holds
//! that state), so no generic write — API or raw — can mint completion.
//!
//! Bounds are enforced HERE, before any write, and oversized input is
//! REJECTED with an error — never silently truncated: goal <=
//! [`MAX_TASK_GOAL_BYTES`] bytes, criteria <= [`MAX_TASK_CRITERIA`] entries
//! (each <= [`MAX_TASK_CRITERION_BYTES`]), plan <= [`MAX_TASK_PLAN_STEPS`]
//! steps (each <= [`MAX_TASK_STEP_BYTES`]). A patch with a `None` field
//! keeps the row's current value, so updates are read-modify-write safe
//! under the session command lock.
//!
//! # Error typing
//!
//! The task operations that can fail with machine/proof rejections return
//! the crate's typed [`TaskError`] (an audit requirement: distinct typed
//! causes, never prose-only). The legacy `create_task` keeps returning
//! [`faktor_core::Error`] because the agent runtime performs a direct
//! `return` of its mapped result; its rejections carry stable
//! `create_task refused: ...` messages. Every `TaskError` converts into
//! [`faktor_core::Error`], so `?` interop with core-`Result` callers is
//! unchanged.

use faktor_core::attachment::{AttachmentId, MAX_ATTACHMENTS_PER_TASK};
use faktor_core::authority::{
    authority_digest_hex, authority_digest_labeled, classify_authority_digest,
    refuse_legacy_authority_digest, AuthorityDigestKind, Fields, LegacyAuthorityDigest,
    DOMAIN_ACCOUNTING_BALANCE, DOMAIN_CHECK_BASIS,
};
use faktor_core::completion::{CompletionContract, CompletionStep, CompletionStepOutcome};
use faktor_core::id::{
    SessionId, TaskId, TaskRevision, VerificationRecordId, WorkspaceId, WorktreeId,
};
use faktor_core::state::{
    legacy_binding_for_criterion_text, CandidateProofRef, CheckExecution, CriterionBinding,
    CriterionOrigin, CriterionRequirement, CriterionVerification, EnvironmentFingerprint,
    FileStateEvidence, TaskState, TaskTransition, ToolVersion, VerificationStatus,
};

use crate::handle::SessionHandle;
use crate::ledger::DurableRead;
use crate::SessionError;

/// Hard bound on one task goal (UTF-8 bytes).
pub const MAX_TASK_GOAL_BYTES: usize = 16 * 1024;
/// Hard bound on the acceptance-criteria entry count.
pub const MAX_TASK_CRITERIA: usize = 32;
/// Hard bound on ONE criterion (mirrors the memory-fact value cap).
pub const MAX_TASK_CRITERION_BYTES: usize = 3000;
/// Hard bound on the append-only plan's step count.
pub const MAX_TASK_PLAN_STEPS: usize = 256;
/// Hard bound on ONE plan step.
pub const MAX_TASK_STEP_BYTES: usize = 3000;

// -------------------------------------------------- record bounds (P0-8)

/// Hard bound on the number of criterion verdicts in one record.
pub const MAX_VERIFICATION_RECORD_CRITERIA: usize = 64;
/// Serialized bound of the record's criteria JSON (128 KiB).
pub const MAX_VERIFICATION_CRITERIA_JSON_BYTES: usize = 128 * 1024;
/// Hard bound on the executed checks in one record.
pub const MAX_VERIFICATION_RECORD_CHECKS: usize = 256;
/// Serialized bound of the record's checks JSON (256 KiB).
pub const MAX_VERIFICATION_CHECKS_JSON_BYTES: usize = 256 * 1024;
/// Hard bound on the changed-file evidence entries in one record.
pub const MAX_VERIFICATION_CHANGED_FILES: usize = 4096;
/// Serialized bound of the record's changed-files JSON.
pub const MAX_VERIFICATION_CHANGED_FILES_JSON_BYTES: usize = 128 * 1024;
/// Hard bound on the unrelated-change path entries in one record.
pub const MAX_VERIFICATION_UNRELATED_CHANGES: usize = 4096;
/// Serialized bound of the record's unrelated-changes JSON.
pub const MAX_VERIFICATION_UNRELATED_JSON_BYTES: usize = 128 * 1024;
/// Serialized bound of the opaque reviewer JSON.
pub const MAX_VERIFICATION_REVIEWER_JSON_BYTES: usize = 16 * 1024;
/// Bound on the tree-hash hex text.
pub const MAX_VERIFICATION_TREE_HASH_BYTES: usize = 128;
/// Bound on one criterion key (equal texts live on the task row at
/// <= MAX_TASK_CRITERION_BYTES; the bound must not be tighter than that).
pub const MAX_VERIFICATION_CRITERION_KEY_BYTES: usize = MAX_TASK_CRITERION_BYTES;
/// Bound on one criterion's evidence prose.
pub const MAX_VERIFICATION_EVIDENCE_BYTES: usize = 4096;
/// Bound on one check name.
pub const MAX_VERIFICATION_CHECK_NAME_BYTES: usize = 2048;
/// Bound on one check program.
pub const MAX_VERIFICATION_PROGRAM_BYTES: usize = 4096;
/// Bound on the per-argument count of one check.
pub const MAX_VERIFICATION_CHECK_ARGS: usize = 32;
/// Bound on ONE check argument.
pub const MAX_VERIFICATION_CHECK_ARG_BYTES: usize = 1024;
/// Bound on one check category.
pub const MAX_VERIFICATION_CATEGORY_BYTES: usize = 128;
/// Bound on one check summary prose.
pub const MAX_VERIFICATION_SUMMARY_BYTES: usize = 8192;
/// Bound on one file path (mirrors the message-payload path bound).
pub const MAX_VERIFICATION_PATH_BYTES: usize = 4096;
/// Bound on one file digest hex text.
pub const MAX_VERIFICATION_DIGEST_BYTES: usize = 128;
/// Serialized bound of the optional environment-fingerprint evidence column
/// (schema v20, audits 94/116/117).
pub const MAX_VERIFICATION_FINGERPRINT_JSON_BYTES: usize =
    faktor_core::state::MAX_ENVIRONMENT_FINGERPRINT_JSON_BYTES;
/// Serialized bound of the optional candidate-proof-reference column
/// (schema v20, audits 116/117).
pub const MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES: usize =
    faktor_core::state::MAX_CANDIDATE_PROOF_REF_JSON_BYTES;

/// The in-band marker of a V2 typed-criterion entry in the existing criteria
/// row values (task row `acceptance_criteria` strings). Legacy plain-text
/// entries carry no marker and keep working: they are migrated
/// deterministically on read (see [`Criterion::decode`]).
const CRITERION_V2_PREFIX: &str = "v2:";
/// The V2 envelope version (a different version is a legacy/foreign entry,
/// never a silently re-interpreted criterion).
const CRITERION_V2_VERSION: u8 = 2;

/// Hard bound on the human TEXT of one typed criterion. The V2 JSON
/// envelope must still fit the existing per-entry value bound
/// ([`MAX_TASK_CRITERION_BYTES`]), and it must stay a legal verification
/// criterion key ([`MAX_VERIFICATION_CRITERION_KEY_BYTES`]), so the text cap
/// reserves room for the encoding.
pub const MAX_TASK_CRITERION_TEXT_BYTES: usize = MAX_TASK_CRITERION_BYTES - 512;
/// Hard bound on a derived criterion's source snapshot id.
pub const MAX_CRITERION_SNAPSHOT_BYTES: usize = 256;

/// Typed failure of a public [`CriterionId`] constructor. No public
/// constructor panics: zero and malformed encodings are values, never
/// `assert!` aborts (criterion ids can arrive from durable rows, JSON
/// envelopes and callers, so a panic is a denial-of-service, not an
/// invariant).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CriterionIdError {
    #[error("criterion id cannot be 0")]
    Zero,
    #[error("malformed criterion id: {0}")]
    Malformed(String),
}

/// The opaque, deterministic 128-bit content id of one acceptance
/// criterion.
///
/// The id is derived from the criterion's identity fields (`origin`,
/// `requirement`, `text`, `semantic_snapshot`) with BLAKE3 truncated to the
/// first 128 bits — reproducible across restarts, re-derivations and legacy
/// migrations without a durable counter, with a collision probability far
/// below any practical criteria set. Zero is folded away so an id is never
/// all-zero.
///
/// # Namespaces and compatibility
///
/// The high half `0` is RESERVED for the legacy 64-bit FNV-1a content ids
/// written by pre-v22 rows (`high == 0`, `low == legacy hash`). New content
/// ids always carry `high != 0`, so a legacy id can never alias a derived
/// id and [`Criterion::validate`] accepts both derivations (existing durable
/// rows keep validating; new writes use 128 bits). The textual form of a
/// legacy id is its decimal `low` (byte-identical to the old `Display`);
/// derived ids render as 32 lowercase hex chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CriterionId {
    high: u64,
    low: u64,
}

impl CriterionId {
    /// The reserved high half of legacy 64-bit content ids.
    const LEGACY_HIGH: u64 = 0;

    /// The deterministic 128-bit content id of a criterion identity.
    /// BLAKE3 over the four identity fields (domain-separated); deterministic
    /// across process restarts, insertion orders and legacy migration.
    pub fn for_content(
        origin: CriterionOrigin,
        requirement: CriterionRequirement,
        text: &str,
        semantic_snapshot: Option<&str>,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"criterion:v3\0");
        hasher.update(origin.label().as_bytes());
        hasher.update(b"\0");
        hasher.update(requirement.label().as_bytes());
        hasher.update(b"\0");
        hasher.update(text.as_bytes());
        hasher.update(b"\0");
        hasher.update(semantic_snapshot.unwrap_or("").as_bytes());
        let digest = hasher.finalize();
        let mut high = u64::from_be_bytes(digest.as_bytes()[0..8].try_into().unwrap());
        let low = u64::from_be_bytes(digest.as_bytes()[8..16].try_into().unwrap());
        if high == Self::LEGACY_HIGH {
            // Keep `high == 0` exclusively for the legacy namespace.
            high = 1;
        }
        if high == 0 && low == 0 {
            return Self { high: 1, low: 1 };
        }
        Self { high, low }
    }

    /// The legacy FNV-1a 64 content id of a criterion identity — the exact
    /// hash pre-v22 rows were written with. Kept so durable rows written by
    /// older builds stay valid on read and re-encode byte-identically.
    fn legacy_for_content(
        origin: CriterionOrigin,
        requirement: CriterionRequirement,
        text: &str,
        semantic_snapshot: Option<&str>,
    ) -> Self {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(PRIME);
            }
            hash ^= 0x1f;
            hash = hash.wrapping_mul(PRIME);
        };
        feed(origin.label().as_bytes());
        feed(requirement.label().as_bytes());
        feed(text.as_bytes());
        feed(semantic_snapshot.unwrap_or("").as_bytes());
        if hash == 0 {
            hash = 1;
        }
        Self {
            high: Self::LEGACY_HIGH,
            low: hash,
        }
    }

    /// The two 64-bit halves of the id (`high` is 0 only for legacy ids).
    pub const fn parts(self) -> (u64, u64) {
        (self.high, self.low)
    }

    /// True for a legacy 64-bit (pre-v22) content id.
    pub const fn is_legacy(self) -> bool {
        self.high == Self::LEGACY_HIGH
    }

    /// The legacy 64-bit value, when this is a legacy id.
    pub const fn legacy_raw(self) -> Option<u64> {
        if self.high == Self::LEGACY_HIGH {
            Some(self.low)
        } else {
            None
        }
    }

    /// The canonical textual id: decimal for legacy ids, 32 lowercase hex
    /// chars for derived ids.
    pub fn to_hex(self) -> String {
        format!("{:016x}{:016x}", self.high, self.low)
    }

    /// Parse the canonical string form of a DERIVED id (32 hex chars,
    /// non-zero high half). Legacy ids use the integer form; a zero-high hex
    /// string is rejected so one id never has two encodings.
    pub fn from_hex(hex: &str) -> Result<Self, CriterionIdError> {
        if hex.len() != 32 {
            return Err(CriterionIdError::Malformed(format!(
                "expected 32 hex chars, got {}",
                hex.len()
            )));
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CriterionIdError::Malformed(format!(
                "non-canonical (lowercase hex) id {hex:?}"
            )));
        }
        let parse = |slice: &str| {
            u64::from_str_radix(slice, 16)
                .map_err(|_| CriterionIdError::Malformed(format!("non-hex id {hex:?}")))
        };
        let high = parse(&hex[0..16])?;
        let low = parse(&hex[16..32])?;
        if high == Self::LEGACY_HIGH || (high == 0 && low == 0) {
            return Err(CriterionIdError::Malformed(
                "a hex-form criterion id must carry a non-zero high half".into(),
            ));
        }
        Ok(Self { high, low })
    }
}

impl TryFrom<u64> for CriterionId {
    type Error = CriterionIdError;
    /// The legacy (integer-form) constructor: non-zero by construction.
    fn try_from(raw: u64) -> Result<Self, Self::Error> {
        if raw == 0 {
            return Err(CriterionIdError::Zero);
        }
        Ok(Self {
            high: Self::LEGACY_HIGH,
            low: raw,
        })
    }
}

impl TryFrom<std::num::NonZeroU64> for CriterionId {
    type Error = CriterionIdError;
    /// The infallible-by-construction integer constructor (no panic path).
    fn try_from(raw: std::num::NonZeroU64) -> Result<Self, Self::Error> {
        Ok(Self {
            high: Self::LEGACY_HIGH,
            low: raw.get(),
        })
    }
}

impl std::fmt::Display for CriterionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_legacy() {
            write!(f, "{}", self.low)
        } else {
            write!(f, "{:016x}{:016x}", self.high, self.low)
        }
    }
}

impl serde::Serialize for CriterionId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Legacy ids serialize as the historic integer (byte-stable rows);
        // derived ids serialize as their 32-hex string.
        if self.is_legacy() {
            s.serialize_u64(self.low)
        } else {
            s.serialize_str(&self.to_hex())
        }
    }
}

impl<'de> serde::Deserialize<'de> for CriterionId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct IdVisitor;
        impl serde::de::Visitor<'_> for IdVisitor {
            type Value = CriterionId;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a non-zero criterion id (u64 legacy form or 32-hex derived form)")
            }

            fn visit_u64<E: serde::de::Error>(self, raw: u64) -> Result<Self::Value, E> {
                CriterionId::try_from(raw).map_err(E::custom)
            }

            fn visit_i64<E: serde::de::Error>(self, raw: i64) -> Result<Self::Value, E> {
                u64::try_from(raw)
                    .map_err(|_| E::custom("criterion id cannot be negative"))
                    .and_then(|raw| CriterionId::try_from(raw).map_err(E::custom))
            }

            fn visit_str<E: serde::de::Error>(self, hex: &str) -> Result<Self::Value, E> {
                CriterionId::from_hex(hex).map_err(E::custom)
            }
        }
        d.deserialize_any(IdVisitor)
    }
}

/// One typed acceptance criterion (audits 56/57/105): exactly the
/// `Criterion{id, text, origin, requirement, evidence_source,
/// semantic_snapshot}` shape. Criteria are persisted through the EXISTING
/// criteria row values (a V2 JSON envelope inside each
/// `acceptance_criteria` string); no store schema change.
///
/// `evidence_source` is the durable raw `EvidenceId` (crates/evidence) of
/// the evidence that certifies the criterion; `semantic_snapshot` is the
/// provider snapshot id the criterion was derived from (derived criteria
/// are tied to their source snapshot — a stale snapshot forces
/// re-derivation).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub id: CriterionId,
    pub text: String,
    pub origin: CriterionOrigin,
    pub requirement: CriterionRequirement,
    pub evidence_source: Option<u64>,
    pub semantic_snapshot: Option<String>,
    /// The typed verification binding (P0 criteria mandate). `None` on
    /// criteria that carry no objective binding yet; the evaluator never
    /// passes such a criterion. Decoding a legacy/V2 entry WITHOUT a binding
    /// migrates it deterministically
    /// ([`faktor_core::state::legacy_binding_for_criterion_text`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<CriterionBinding>,
}

/// The on-disk V2 envelope (private: the in-band representation is an
/// implementation detail; absent optional fields are omitted so the encoding
/// is compact and byte-deterministic).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionEnvelope {
    v: u8,
    id: CriterionId,
    text: String,
    origin: CriterionOrigin,
    requirement: CriterionRequirement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_source: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    semantic_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<CriterionBinding>,
}

impl Criterion {
    /// A user criterion (sticky origin, `Required`, no evidence/snapshot).
    pub fn user(text: impl Into<String>) -> Self {
        let text = text.into();
        let id = CriterionId::for_content(
            CriterionOrigin::User,
            CriterionRequirement::Required,
            &text,
            None,
        );
        Self {
            id,
            text,
            origin: CriterionOrigin::User,
            requirement: CriterionRequirement::Required,
            evidence_source: None,
            semantic_snapshot: None,
            binding: None,
        }
    }

    /// A derived criterion tied to its source (non-user origin; the id is
    /// content-addressed over origin/requirement/text/snapshot).
    pub fn derived(
        text: impl Into<String>,
        origin: CriterionOrigin,
        requirement: CriterionRequirement,
        semantic_snapshot: Option<String>,
    ) -> Self {
        let text = text.into();
        let id = CriterionId::for_content(origin, requirement, &text, semantic_snapshot.as_deref());
        Self {
            id,
            text,
            origin,
            requirement,
            evidence_source: None,
            semantic_snapshot,
            binding: None,
        }
    }

    /// Re-bind the criterion's typed verification binding (the criterion's
    /// id does not change: a binding is a verification contract, not
    /// identity).
    pub fn with_binding(mut self, binding: CriterionBinding) -> Self {
        self.binding = Some(binding);
        self
    }

    /// Re-bind the criterion's evidence source (the criterion's id does not
    /// change: evidence is a verification binding, not identity).
    pub fn with_evidence(mut self, evidence_source: u64) -> Self {
        self.evidence_source = Some(evidence_source);
        self
    }

    /// Structural validation: bounded text/snapshot and an id that IS the
    /// deterministic content id (a hostile hand-crafted id can never pass).
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.text.len() > MAX_TASK_CRITERION_TEXT_BYTES {
            return Err(TaskError::Oversized(format!(
                "criterion text of {} bytes exceeds MAX_TASK_CRITERION_TEXT_BYTES ({MAX_TASK_CRITERION_TEXT_BYTES})",
                self.text.len()
            )));
        }
        if let Some(binding) = &self.binding {
            binding.validate().map_err(|violations| {
                TaskError::Malformed(format!(
                    "criterion {:?} carries an invalid binding: {violations:?}",
                    self.text
                ))
            })?;
        }
        if let Some(snapshot) = &self.semantic_snapshot {
            if snapshot.len() > MAX_CRITERION_SNAPSHOT_BYTES {
                return Err(TaskError::Oversized(format!(
                    "criterion snapshot of {} bytes exceeds MAX_CRITERION_SNAPSHOT_BYTES ({MAX_CRITERION_SNAPSHOT_BYTES})",
                    snapshot.len()
                )));
            }
        }
        let expected = CriterionId::for_content(
            self.origin,
            self.requirement,
            &self.text,
            self.semantic_snapshot.as_deref(),
        );
        // Legacy (pre-v22) rows carry the FNV-1a 64 content id; accepting
        // BOTH deterministic derivations keeps them valid while every new
        // write uses the 128-bit id. A hostile hand-crafted id still never
        // passes: it must equal one of the two content derivations.
        let legacy = CriterionId::legacy_for_content(
            self.origin,
            self.requirement,
            &self.text,
            self.semantic_snapshot.as_deref(),
        );
        if self.id != expected && self.id != legacy {
            return Err(TaskError::Malformed(format!(
                "criterion id {} is not the deterministic content id {expected} (legacy {legacy}) of origin={} requirement={} snapshot={:?}",
                self.id, self.origin, self.requirement, self.semantic_snapshot
            )));
        }
        // A text within the text bound can still encode beyond the per-entry
        // bound (escape-heavy content): reject loudly here rather than let
        // `encoded_entry` silently demote the criterion to plain text and
        // drop its typed metadata on write.
        let encoded_len = self.encode().len();
        if encoded_len > MAX_TASK_CRITERION_BYTES {
            return Err(TaskError::Oversized(format!(
                "criterion {} encodes to {encoded_len} bytes, beyond MAX_TASK_CRITERION_BYTES ({MAX_TASK_CRITERION_BYTES})",
                self.id
            )));
        }
        Ok(())
    }

    /// The V2 in-band encoding (`v2:` + compact JSON).
    pub fn encode(&self) -> String {
        let envelope = CriterionEnvelope {
            v: CRITERION_V2_VERSION,
            id: self.id,
            text: self.text.clone(),
            origin: self.origin,
            requirement: self.requirement,
            evidence_source: self.evidence_source,
            semantic_snapshot: self.semantic_snapshot.clone(),
            binding: self.binding.clone(),
        };
        // Never `unwrap_or_default()`: an empty default encodes as the
        // literal "v2:", which decodes as legacy plain text reading "v2:" —
        // silent corruption of a durable entry. The envelope is a flat owned
        // structure (strings/enums/options) whose JSON encoding cannot fail;
        // if a serde contract break ever makes this reachable, log loudly
        // and degrade to the criterion's PLAIN TEXT — the documented legacy
        // representation that keeps the text lossless and the typed metadata
        // an honest absence, exactly like the over-bound path in
        // `encoded_entry`.
        match serde_json::to_string(&envelope) {
            Ok(json) => format!("{CRITERION_V2_PREFIX}{json}"),
            Err(err) => {
                tracing::error!(
                    criterion = %self.id,
                    error = %err,
                    "criterion envelope failed to serialize; persisting its plain legacy text"
                );
                self.text.clone()
            }
        }
    }

    /// The entry to persist for this criterion: the V2 encoding when it fits
    /// the existing per-entry bound, otherwise the plain text (a legacy
    /// over-bound entry stays lossless and un-typed — never truncated).
    pub fn encoded_entry(&self) -> String {
        let encoded = self.encode();
        if encoded.len() <= MAX_TASK_CRITERION_BYTES {
            encoded
        } else {
            self.text.clone()
        }
    }

    /// Decode one persisted entry. `None` means "legacy/foreign plain text"
    /// (no marker, wrong version, or malformed JSON) — never a guessed
    /// criterion.
    pub fn decode(entry: &str) -> Option<Self> {
        let json = entry.strip_prefix(CRITERION_V2_PREFIX)?;
        let envelope: CriterionEnvelope = serde_json::from_str(json).ok()?;
        if envelope.v != CRITERION_V2_VERSION {
            return None;
        }
        Some(Self {
            id: envelope.id,
            text: envelope.text,
            origin: envelope.origin,
            requirement: envelope.requirement,
            evidence_source: envelope.evidence_source,
            semantic_snapshot: envelope.semantic_snapshot,
            binding: envelope.binding,
        })
    }

    /// Migrate one legacy plain-text criterion deterministically. The legacy
    /// writer was always the system derivation, so only the canonical goal
    /// prefix is a sticky user criterion (`goal: ` -> User); any other
    /// legacy text migrates as a replaceable policy derivation
    /// (`ProjectPolicy`). Genuinely user-authored criteria survive
    /// re-derivation by being written through the typed API with
    /// [`CriterionOrigin::User`]. The id is the same content id used for
    /// typed criteria, so the migration is stable across restarts and
    /// repeated reads.
    pub fn legacy(entry: &str) -> Self {
        let origin = if entry.starts_with("goal: ") {
            CriterionOrigin::User
        } else {
            CriterionOrigin::ProjectPolicy
        };
        Self::derived(
            entry.to_string(),
            origin,
            CriterionRequirement::Required,
            None,
        )
        .with_binding(legacy_binding_for_criterion_text(entry))
    }

    /// The human text of one persisted entry (typed entries decode to their
    /// text; legacy entries are already text). Read-only helper for
    /// consumers that must not see the encoding envelope.
    pub fn text_of(entry: &str) -> String {
        Self::decode(entry)
            .map(|c| c.text)
            .unwrap_or_else(|| entry.to_string())
    }

    /// The binding the EVALUATOR must use: an explicit binding wins; a
    /// binding-less criterion (a pre-binding V2 row or a plain legacy entry)
    /// migrates deterministically from its text — `goal: ` rows become
    /// AggregateGoal, check-derived rows bind their command digest, and
    /// anything else is Unavailable. Never a guessed pass mechanism.
    pub fn effective_binding(&self) -> CriterionBinding {
        self.binding
            .clone()
            .unwrap_or_else(|| legacy_binding_for_criterion_text(&self.text))
    }
}

/// Decode a full criteria row: typed V2 entries decode; every other entry
/// migrates deterministically through [`Criterion::legacy`]. Deterministic:
/// repeated reads of the same row yield identical ids.
pub fn decode_criteria(entries: &[String]) -> Vec<Criterion> {
    entries
        .iter()
        .map(|entry| Criterion::decode(entry).unwrap_or_else(|| Criterion::legacy(entry)))
        .collect()
}

/// Encode a full criteria row into the existing per-entry values.
pub fn encode_criteria(criteria: &[Criterion]) -> Vec<String> {
    criteria.iter().map(Criterion::encoded_entry).collect()
}

/// Re-derive a criteria set (audits 56/57/105), deterministically:
///
/// - every existing USER criterion survives verbatim (a re-derivation may
///   never remove a user criterion);
/// - the `derived` set is authoritative for every non-user origin: a
///   derived criterion whose source snapshot moved is replaced by the newly
///   derived one (its content-addressed id changes with the snapshot, so the
///   stale criterion is re-derived, never silently kept);
/// - existing non-user criteria absent from `derived` are dropped
///   (superseded);
/// - identical content is deduplicated by id (and a derived criterion whose
///   text duplicates a user criterion is skipped: the user criterion wins).
pub fn merge_derived_criteria(existing: &[Criterion], derived: &[Criterion]) -> Vec<Criterion> {
    let mut out: Vec<Criterion> = Vec::new();
    for criterion in existing.iter().filter(|c| c.origin.is_user()) {
        if !out.iter().any(|c| c.id == criterion.id) {
            out.push(criterion.clone());
        }
    }
    for criterion in derived {
        if out.iter().any(|c| c.id == criterion.id) {
            continue;
        }
        if out.iter().any(|c| c.text == criterion.text) {
            continue;
        }
        out.push(criterion.clone());
    }
    out
}

/// Validate a whole criteria set: bounds, content ids and id uniqueness.
fn validate_criteria(criteria: &[Criterion]) -> Result<(), TaskError> {
    if criteria.len() > MAX_TASK_CRITERIA {
        return Err(TaskError::Oversized(format!(
            "{} acceptance criteria exceed MAX_TASK_CRITERIA ({MAX_TASK_CRITERIA})",
            criteria.len()
        )));
    }
    let mut ids = std::collections::HashSet::new();
    for criterion in criteria {
        criterion.validate()?;
        if !ids.insert(criterion.id) {
            return Err(TaskError::Malformed(format!(
                "duplicate criterion id {} (content hash collision or a hostile id): the criteria set is ambiguous",
                criterion.id
            )));
        }
    }
    Ok(())
}

/// The durable budget envelope of a Task. `None` max fields mean unlimited;
/// `spent_*` fields grow monotonically from durable sources (provider-call
/// rows + `turn_completed` journal events), so spend survives crashes.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TaskBudget {
    pub max_tokens: Option<u64>,
    pub max_turns: Option<u32>,
    pub spent_tokens: u64,
    pub spent_turns: u32,
}

/// The durable Task object (audit 25). One row per `(session_id, task_id)`;
/// `task_id` is the session's adopted durable task identity.
///
/// The row's `revision` counter is NOT a field of this struct (the agent
/// runtime constructs `Task` literals by field, and adding a field would
/// break every literal site in a crate this wave may not touch); it rides
/// the store row and is read through
/// [`SessionHandle::task_revision`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Task {
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub goal: String,
    /// Goal + project-derived required checks, seeded once when first seen.
    pub acceptance_criteria: Vec<String>,
    /// Ordered steps; append-only durable.
    pub plan: Vec<String>,
    /// Durable typed binary/image attachments (schema v24), SEPARATE from
    /// any workspace path: each entry names CAS bytes by digest plus its
    /// declared mime/filename/size. Rows written before v24 and every
    /// attachment-free task decode with an empty list.
    pub attachments: Vec<AttachmentId>,
    pub budget: TaskBudget,
    pub state: TaskState,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            task_id: TaskId::new(1),
            session_id: SessionId::new(1),
            goal: String::new(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: 0,
            updated_ms: 0,
        }
    }
}

impl From<faktor_store::TaskRow> for Task {
    fn from(r: faktor_store::TaskRow) -> Self {
        Self {
            task_id: r.task_id,
            session_id: r.session_id,
            goal: r.goal,
            acceptance_criteria: r.acceptance_criteria,
            plan: r.plan,
            attachments: r.attachments,
            budget: TaskBudget {
                max_tokens: r.max_tokens,
                max_turns: r.max_turns,
                spent_tokens: r.spent_tokens,
                spent_turns: r.spent_turns,
            },
            state: r.state,
            created_ms: r.created_ms,
            updated_ms: r.updated_ms,
        }
    }
}

/// Build a store row for a caller-constructed revision (private: the
/// revision is never derived from a `Task`, which deliberately does not
/// carry one).
fn task_row(task: Task, revision: TaskRevision) -> faktor_store::TaskRow {
    faktor_store::TaskRow {
        task_id: task.task_id,
        session_id: task.session_id,
        goal: task.goal,
        acceptance_criteria: task.acceptance_criteria,
        plan: task.plan,
        attachments: task.attachments,
        max_tokens: task.budget.max_tokens,
        max_turns: task.budget.max_turns,
        spent_tokens: task.budget.spent_tokens,
        spent_turns: task.budget.spent_turns,
        state: task.state,
        revision,
        created_ms: task.created_ms,
        updated_ms: task.updated_ms,
    }
}

impl Task {
    /// The typed criteria view of this row (audits 56/57): V2 entries decode
    /// to their criterion; legacy plain-text entries deterministically
    /// migrate (stable content ids, inferred origin). Repeated reads of the
    /// same row always yield identical ids.
    pub fn criteria(&self) -> Vec<Criterion> {
        decode_criteria(&self.acceptance_criteria)
    }
}

/// The session-facing view of one durable verification record (audit P0-8).
/// Records are immutable after creation except the single CAS finalize
/// `Running -> Passed|Failed` ([`SessionHandle::finalize_verification_record`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRecord {
    pub record_id: VerificationRecordId,
    pub task_id: TaskId,
    /// The task revision this record certifies (its creation-time task
    /// revision). Completion requires the task to STILL be at this revision.
    pub revision: TaskRevision,
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub tree_hash: Option<String>,
    /// The bounded environment fingerprint the verification ran under
    /// (schema v20, audits 94/116/117). `None` on legacy records and when no
    /// fingerprint was recorded — an honest absence, never a guess.
    pub environment_fingerprint: Option<EnvironmentFingerprint>,
    /// The compact candidate-proof reference of this record (schema v20,
    /// audits 116/117). `None` on legacy records.
    pub candidate_proof_ref: Option<CandidateProofRef>,
    pub criteria: Vec<CriterionVerification>,
    pub checks: Vec<CheckExecution>,
    pub changed_files: Vec<FileStateEvidence>,
    pub unrelated_changes: Vec<String>,
    pub reviewer: Option<serde_json::Value>,
    pub status: VerificationStatus,
    pub started_ms: i64,
    pub completed_ms: Option<i64>,
}

impl From<faktor_store::VerificationRecordRow> for VerificationRecord {
    fn from(r: faktor_store::VerificationRecordRow) -> Self {
        Self {
            record_id: r.id,
            task_id: r.task_id,
            revision: r.revision,
            workspace_id: r.workspace_id,
            worktree_id: r.worktree_id,
            tree_hash: r.tree_hash,
            environment_fingerprint: None,
            candidate_proof_ref: None,
            criteria: r.criteria,
            checks: r.checks,
            changed_files: r.changed_files,
            unrelated_changes: r.unrelated_changes,
            reviewer: r.reviewer,
            status: r.status,
            started_ms: r.started_ms,
            completed_ms: r.completed_ms,
        }
    }
}

/// Typed failure of the task machine / completion-proof operations
/// (audits P0-7/P0-8). Every rejection cause is its own variant, so
/// callers distinguish a missing record from a wrong-revision record from
/// an uncovered criterion without parsing prose. Conversions into
/// [`faktor_core::Error`] keep the crate's `?` interop with core-`Result`
/// callers (the agent runtime) intact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("task {0} not found")]
    NotFound(TaskId),
    #[error("attachment blob not found: {0}")]
    AttachmentBlobNotFound(String),
    #[error("task {task_id} already exists: a task row is created once and mutated through update_task/transition_task/complete_verified_task, never recreated")]
    AlreadyExists { task_id: TaskId },
    #[error("create_task refused: state {state:?} cannot seed a task; only Pending, Planning or Running may (VerifiedComplete requires a passing verification record via complete_verified_task)")]
    IllegalCreateState { state: TaskState },
    #[error("illegal task state transition {from:?} -> {to:?}: {detail}")]
    IllegalTransition {
        from: TaskState,
        to: TaskState,
        detail: String,
    },
    #[error("completion-relevant task state {state:?} cannot be assigned through update_task; use transition_task (NeedsVerification/Verifying) or complete_verified_task (VerifiedComplete)")]
    CompletionStateViaPatch { state: TaskState },
    #[error(
        "task {task_id} is terminal ({state:?}): its row is frozen, no further mutation is legal"
    )]
    TerminalTask { task_id: TaskId, state: TaskState },
    #[error("task {task_id} revision mismatch: expected {expected}, actual {actual} (re-read the row and retry with the current revision)")]
    RevisionMismatch {
        task_id: TaskId,
        expected: TaskRevision,
        actual: TaskRevision,
    },
    #[error("task {task_id} attachments changed concurrently: expected {expected:?}, actual {actual:?} (re-read the row and retry the seed)")]
    AttachmentsChanged {
        task_id: TaskId,
        expected: Vec<AttachmentId>,
        actual: Vec<AttachmentId>,
    },
    #[error("task is not Verifying (actual state {actual:?}); VerifiedComplete requires Verifying plus a passing record via complete_verified_task")]
    NotVerifying { actual: TaskState },
    #[error("verification record {0} does not exist")]
    RecordNotFound(VerificationRecordId),
    #[error(
        "verification record {record} certifies task {record_task}, not task {requested_task}"
    )]
    RecordWrongTask {
        record: VerificationRecordId,
        record_task: TaskId,
        requested_task: TaskId,
    },
    #[error("verification record {record} certifies task revision {record_revision}, but the task is at revision {expected}")]
    RecordWrongRevision {
        record: VerificationRecordId,
        record_revision: TaskRevision,
        expected: TaskRevision,
    },
    #[error(
        "verification record {record} has status {status:?}; only Passed certifies completion"
    )]
    RecordNotPassed {
        record: VerificationRecordId,
        status: VerificationStatus,
    },
    #[error("verification record {record} does not cover {missing:?} (present with passed=true is required for every current acceptance criterion)")]
    CriteriaNotCovered {
        record: VerificationRecordId,
        missing: Vec<String>,
    },
    #[error(
        "verification record {record} is a binding-less LEGACY proof and can never certify \
         task {task_id}: the modern criteria {criteria:?} require verdicts bound through their \
         own manifest, so post-upgrade recovery forces re-verification"
    )]
    LegacyProofRequiresReverification {
        task_id: TaskId,
        record: VerificationRecordId,
        criteria: Vec<String>,
    },
    #[error(
        "mutating completion of task {task_id} refused: verification record {record} carries no \
         canonical tree-manifest hash (`tm1:<64-hex>`), so the changed tree is unbound and the \
         proof can never certify a mutation; re-verify the run through the manifest-bound \
         integration path"
    )]
    ManifestBindingMissing {
        task_id: TaskId,
        record: VerificationRecordId,
    },
    #[error("corrupt durable state ({what}): {detail}")]
    CorruptDurableState { what: String, detail: String },
    #[error("verification record {record} was certified against worktree {record_workspace}/{record_worktree}; the task's base worktree is {task_workspace}/{task_worktree}")]
    WorktreeMismatch {
        record: VerificationRecordId,
        record_workspace: WorkspaceId,
        record_worktree: WorktreeId,
        task_workspace: WorkspaceId,
        task_worktree: WorktreeId,
    },
    #[error("verification record {record} cannot be finalized: it is {current:?}; only a Running record finalizes, exactly once")]
    RecordNotFinalizable {
        record: VerificationRecordId,
        current: VerificationStatus,
    },
    #[error(
        "verification record {record} certifies a final integration snapshot, but task {task_id} carries no integration record for it: only a real staged child integration may certify orchestrated completion"
    )]
    IntegrationRecordMissing {
        task_id: TaskId,
        record: VerificationRecordId,
    },
    #[error(
        "verification record {record} certifies integration snapshot {recorded}, but the current root snapshot is {current} (the owner checkout moved after integration): completion is refused until the run is re-integrated and re-verified"
    )]
    IntegrationSnapshotMismatch {
        task_id: TaskId,
        record: VerificationRecordId,
        recorded: String,
        current: String,
    },
    #[error(
        "verification record {record} cannot be bound for task {task_id}: {detail} (completion is refused until the root is resolvable and re-verified)"
    )]
    IntegrationSnapshotUnavailable {
        task_id: TaskId,
        record: VerificationRecordId,
        detail: String,
    },
    #[error(
        "completion contract step {step:?} of task {task_id} revision {revision} was recorded against integration snapshot {recorded}, but the current root snapshot is {current}: the step outcome is bound to the certified root and a moved root requires re-verification"
    )]
    CompletionStepSnapshotMismatch {
        task_id: TaskId,
        revision: TaskRevision,
        step: CompletionStep,
        recorded: String,
        current: String,
    },
    #[error("root snapshot unavailable: {0}")]
    RootSnapshotUnavailable(String),
    #[error(
        "root snapshot refused: the workspace root contains special file(s) {paths:?}; the \
         canonical tree manifest cannot represent them, so tree equality is unprovable and \
         completion must not proceed on a degraded comparison"
    )]
    RootSnapshotSpecialFile { paths: Vec<String> },
    #[error(
        "completion accounting incomplete for task {task_id}: {open_count} open reservation(s) \
         ({open_micro} micro held; {dispatched_count} already dispatched) and {uncertain_count} \
         UNCERTAIN ({uncertain_micro} micro held) still consume budget; VerifiedComplete requires \
         every reservation settled, refunded or conservatively finalized — the task STAYS Verifying \
         until a later completion pass converges"
    )]
    AccountingIncomplete {
        task_id: TaskId,
        open_count: usize,
        open_micro: u64,
        dispatched_count: usize,
        uncertain_count: usize,
        uncertain_micro: u64,
    },
    #[error("completion accounting failed for task {task_id}: {detail} (nothing was transitioned; the task STAYS Verifying)")]
    AccountingFailure { task_id: TaskId, detail: String },
    #[error("completion contract refused for task {task_id} revision {revision}: step {step:?} has no durable status row; missing is NOT done, and the task STAYS Verifying (record the step outcome via set_completion_step_status and retry completion)")]
    CompletionStepMissing {
        task_id: TaskId,
        revision: TaskRevision,
        step: CompletionStep,
    },
    #[error("completion contract refused for task {task_id} revision {revision}: step {step:?} is {status:?} ({detail}); only a Succeeded step satisfies the contract and the task STAYS Verifying")]
    CompletionStepNotSucceeded {
        task_id: TaskId,
        revision: TaskRevision,
        step: CompletionStep,
        status: CompletionStepOutcome,
        detail: String,
    },
    #[error("completion contract TERMINALLY refused for task {task_id} revision {revision}: step {step:?} FAILED ({detail}); a failed step can never certify under its contract revision")]
    CompletionStepFailed {
        task_id: TaskId,
        revision: TaskRevision,
        step: CompletionStep,
        detail: String,
    },
    #[error("completion contract for task {task_id} revision {revision} is already recorded; a contract is immutable per task revision")]
    CompletionContractImmutable {
        task_id: TaskId,
        revision: TaskRevision,
    },
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("the mutating run left task {task_id}'s change budget: {violations:?}")]
    ChangeBudgetRefused {
        task_id: TaskId,
        violations: Vec<crate::budget::ChangeBudgetViolation>,
    },
    #[error(
        "legacy 64-bit FNV authority digest {digest:?} in {field}: it may be viewed but never \
         authorizes new verification or proof reuse; restage and reverify under the canonical \
         BLAKE3 authority digests"
    )]
    LegacyAuthorityDigest { field: String, digest: String },
    #[error("input exceeds bound: {0}")]
    Oversized(String),
    #[error("malformed input: {0}")]
    Malformed(String),
    #[error("store failure: {0}")]
    Store(String),
    #[error("internal task failure: {0}")]
    Internal(String),
}

impl From<faktor_store::StoreError> for TaskError {
    fn from(e: faktor_store::StoreError) -> Self {
        TaskError::Store(e.to_string())
    }
}

impl From<TaskError> for SessionError {
    fn from(e: TaskError) -> Self {
        match e {
            TaskError::NotFound(m) => SessionError::NotFound(format!("task {m}")),
            TaskError::AttachmentBlobNotFound(m) => SessionError::NotFound(m),
            TaskError::Store(m) => SessionError::Store(faktor_store::StoreError::Conflict(m)),
            TaskError::Oversized(m) => SessionError::Oversized(m),
            TaskError::Malformed(m) => SessionError::Malformed(m),
            other => SessionError::Conflict(other.to_string()),
        }
    }
}

impl From<TaskError> for faktor_core::Error {
    fn from(e: TaskError) -> Self {
        SessionError::from(e).into()
    }
}

/// The outcome of the durable PR/CI-fix completion-contract gate.
/// `Refused` carries the typed refusal (the task stays Verifying); `Err`
/// from [`SessionHandle::completion_contract_gate`] is reserved for
/// store/decode failures, which are corruption and never a semantic
/// refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContractGate {
    /// No contract, a default contract, or every requested step carries a
    /// durable `Succeeded` status row.
    Satisfied,
    /// A requested step is not done; the payload names the first unmet
    /// step in gate order.
    Refused(TaskError),
}

/// Map a ledger/store failure into the typed task error space (the gate
/// helpers never hide a corrupt row behind a semantic refusal).
fn task_error_from_core(e: faktor_core::Error) -> TaskError {
    match e.kind {
        faktor_core::ErrorKind::Malformed => TaskError::Malformed(e.message),
        faktor_core::ErrorKind::Oversized => TaskError::Oversized(e.message),
        faktor_core::ErrorKind::Conflict => TaskError::Conflict(e.message),
        _ => TaskError::Store(e.message),
    }
}

/// Map one atomic-seed refusal onto the typed task error space. Every cause
/// keeps its own variant; nothing here weakens the state-machine or CAS
/// semantics into a prose-only error.
fn seed_refusal_to_task_error(
    task_id: TaskId,
    refusal: faktor_store::SeedTaskAttachmentRefusal,
) -> TaskError {
    match refusal {
        faktor_store::SeedTaskAttachmentRefusal::TaskMissing { .. } => TaskError::NotFound(task_id),
        faktor_store::SeedTaskAttachmentRefusal::TaskExists { task_id } => {
            TaskError::AlreadyExists { task_id }
        }
        faktor_store::SeedTaskAttachmentRefusal::RevisionMismatch { expected, actual } => {
            TaskError::RevisionMismatch {
                task_id,
                expected,
                actual,
            }
        }
        faktor_store::SeedTaskAttachmentRefusal::Terminal { state } => {
            TaskError::TerminalTask { task_id, state }
        }
        faktor_store::SeedTaskAttachmentRefusal::AttachmentsMismatch { expected, actual } => {
            TaskError::AttachmentsChanged {
                task_id,
                expected,
                actual,
            }
        }
        faktor_store::SeedTaskAttachmentRefusal::IllegalCreateState { state } => {
            TaskError::IllegalCreateState { state }
        }
    }
}

/// A bounded update over an existing durable Task. Every field is optional:
/// `None` keeps the row's current value, so `update_task` never clobbers a
/// field its caller did not intend to change (e.g. the runtime preserves a
/// caller-set budget when it patches the gate state).
///
/// `state` assignments are machine-checked (audit P0-7): completion-relevant
/// states (`NeedsVerification`/`Verifying`/`VerifiedComplete`) are rejected
/// with [`TaskError::CompletionStateViaPatch`], and any non-completion
/// assignment that is not a legal edge from the row's current state is
/// rejected with [`TaskError::IllegalTransition`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskPatch {
    pub goal: Option<String>,
    pub acceptance_criteria: Option<Vec<String>>,
    pub plan: Option<Vec<String>>,
    /// Replace the task's attachment set (None = keep the durable set).
    pub attachments: Option<Vec<AttachmentId>>,
    pub budget: Option<TaskBudget>,
    pub state: Option<TaskState>,
}

/// Map a canonical tree-manifest refusal onto the session's typed task error
/// space. The canonical manifest itself lives in `faktor_fs::tree_manifest`
/// (the ONE definition of "the same tree"); this crate only maps its typed
/// refusals so the completion binding keeps distinct causes.
fn task_error_from_tree_manifest(e: faktor_fs::tree_manifest::TreeManifestError) -> TaskError {
    use faktor_fs::tree_manifest::TreeManifestError as E;
    match e {
        E::RootUnavailable(message) => TaskError::RootSnapshotUnavailable(message),
        E::Malformed(message) => TaskError::Malformed(message),
        E::Oversized(message) => TaskError::Oversized(message),
        E::SpecialFile { paths } => TaskError::RootSnapshotSpecialFile { paths },
        E::Io(message) => TaskError::Internal(message),
    }
}

/// The canonical tree-manifest digest of `root` (`tm1:<64-hex>`, see
/// [`faktor_fs::tree_manifest`]): the completion binding's root snapshot.
/// Every caller in this crate goes through the shared fs implementation —
/// there is no second definition of "the same tree".
fn current_manifest_digest(root: &std::path::Path) -> Result<String, TaskError> {
    faktor_fs::tree_manifest::tree_manifest_digest(
        root,
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .map_err(task_error_from_tree_manifest)
}

// ---------------------------------------------------- proof basis (P0)

/// Hard bound on the ordered entry lists of one [`ProofBasis`].
pub const MAX_PROOF_BASIS_ENTRIES: usize = 256;

/// One ordered check of the proof basis: the check id plus its typed
/// (program, argv) basis.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBasisCheck {
    pub check_id: String,
    pub program: String,
    pub args: Vec<String>,
}

/// One criterion of the proof basis: id plus its binding's content digest
/// (`None` = the criterion carries no binding).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBasisCriterion {
    pub criterion_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_digest: Option<String>,
}

/// The exact basis a verification proof is bound to (P0 proof binding): a
/// record may be REUSED only while every component below is identical.
/// Deliberately one fingerprint system: this feeds
/// [`EnvironmentFingerprint::proof_basis_digest`], never a parallel one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBasis {
    pub task_id: u64,
    pub task_revision: u64,
    pub task_contract_digest: String,
    pub candidate_snapshot: String,
    pub integration_sources_digest: String,
    pub changed_files_digest: String,
    pub checks: Vec<ProofBasisCheck>,
    pub verification_impl_version: String,
    pub tool_versions: Vec<ToolVersion>,
    /// The documented verification-relevant environment projection.
    pub env_projection: Vec<(String, String)>,
    pub instruction_epoch: Option<u64>,
    pub criteria: Vec<ProofBasisCriterion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_digest: Option<String>,
    #[serde(default)]
    pub evidence_digests: Vec<String>,
}

/// Monotonic nonce for the (unreachable) proof-basis serialization fault: a
/// poison digest differs on every fault, so a serialization break can never
/// make two different bases compare equal (which would authorize a FALSE
/// proof reuse) — it fails closed instead.
static PROOF_BASIS_SERIALIZE_FAULTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

impl ProofBasis {
    /// The canonical digest of this basis (`blake3:` prefixed).
    pub fn digest(&self) -> String {
        match serde_json::to_vec(self) {
            Ok(bytes) => format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            Err(err) => {
                // Never `unwrap_or_default()`: defaulting to empty bytes
                // would collapse EVERY basis to one constant digest and
                // silently authorize reuse across different proofs. The flat
                // owned structure cannot fail to encode; if that invariant
                // ever breaks, log loudly and mint a unique NON-HEX poison
                // digest — it can never equal a real digest, so reuse is
                // refused (fail closed), never falsely allowed.
                tracing::error!(
                    error = %err,
                    "proof basis failed to serialize; minting a non-reusable poison digest"
                );
                let nonce =
                    PROOF_BASIS_SERIALIZE_FAULTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                format!("blake3:encode-error-{nonce:016x}")
            }
        }
    }

    /// The reserved `env_projection` key carrying the layered effective
    /// configuration digest of this basis.
    pub const CONFIG_DIGEST_KEY: &'static str = "faktor.config.effective_digest";

    /// The layered effective-configuration digest bound into this basis,
    /// when one was bound. A record's reuse is refused whenever the current
    /// effective configuration (any layer: system/organization/repository/
    /// user/session/task) no longer digests to the same value, because the
    /// binding rides the basis digest the record was written under.
    pub fn config_digest(&self) -> Option<&str> {
        self.env_projection
            .iter()
            .rev()
            .find(|(key, _)| key == Self::CONFIG_DIGEST_KEY)
            .map(|(_, value)| value.as_str())
    }

    /// Bind the layered effective-configuration digest into this basis.
    /// Bounded and strict: an empty/oversized/whitespace-bearing digest is
    /// refused, and a second bind replaces the previous value (one binding
    /// per basis). Any layer change yields a different digest, so the basis
    /// digest — and with it the record's reuse key — changes.
    pub fn bind_config_digest(mut self, digest: &str) -> Result<Self, TaskError> {
        validate_proof_config_digest(digest)?;
        self.env_projection
            .retain(|(key, _)| key != Self::CONFIG_DIGEST_KEY);
        self.env_projection
            .push((Self::CONFIG_DIGEST_KEY.to_string(), digest.to_string()));
        Ok(self)
    }

    /// Fail closed: whether this basis names the layered effective
    /// configuration it was produced under. A basis without a binding can
    /// never certify a completion proof that must be attributable to a
    /// configuration (the caller refuses before writing the record).
    pub fn require_config_digest(&self) -> Result<&str, TaskError> {
        self.config_digest().ok_or_else(|| {
            TaskError::Malformed(
                "proof basis carries no layered effective-configuration digest; a \
                 configuration-attributable proof must bind one (bind_config_digest)"
                    .into(),
            )
        })
    }

    /// The first legacy 64-bit FNV authority digest carried by this basis,
    /// when any: such a basis may be VIEWED but never authorizes a new
    /// record or a proof reuse — the caller refuses typed and forces a
    /// restage/reverification under the canonical BLAKE3 identities.
    pub fn legacy_authority_digest(&self) -> Option<LegacyAuthorityDigest> {
        let members: [(&'static str, &str); 3] = [
            ("task_contract_digest", &self.task_contract_digest),
            (
                "integration_sources_digest",
                &self.integration_sources_digest,
            ),
            ("changed_files_digest", &self.changed_files_digest),
        ];
        for (what, value) in members {
            if let Err(legacy) = refuse_legacy_authority_digest(what, value) {
                return Some(legacy);
            }
        }
        for criterion in &self.criteria {
            if let Some(digest) = &criterion.binding_digest {
                if let Err(legacy) =
                    refuse_legacy_authority_digest("criterion binding_digest", digest)
                {
                    return Some(legacy);
                }
            }
        }
        if let Some(reviewer) = &self.reviewer_digest {
            if let Err(legacy) = refuse_legacy_authority_digest("reviewer_digest", reviewer) {
                return Some(legacy);
            }
        }
        None
    }
}

/// Hard bound on one layered effective-configuration digest string.
pub const MAX_PROOF_CONFIG_DIGEST_BYTES: usize = 256;
/// Hard bound on the ordered layers of one layered configuration binding.
pub const MAX_PROOF_CONFIG_LAYERS: usize = 6;

/// One configuration layer scope, outermost to innermost (the same order the
/// effective configuration is resolved in). The vocabulary is the proof
/// binding's own; the DIGEST of a layer's effective value is supplied by the
/// configuration authority (never re-derived here).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProofConfigScope {
    System,
    Organization,
    Repository,
    User,
    Session,
    Task,
}

impl ProofConfigScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProofConfigScope::System => "system",
            ProofConfigScope::Organization => "organization",
            ProofConfigScope::Repository => "repository",
            ProofConfigScope::User => "user",
            ProofConfigScope::Session => "session",
            ProofConfigScope::Task => "task",
        }
    }

    /// Scope order rank (outermost = 0).
    pub const fn rank(self) -> u8 {
        match self {
            ProofConfigScope::System => 0,
            ProofConfigScope::Organization => 1,
            ProofConfigScope::Repository => 2,
            ProofConfigScope::User => 3,
            ProofConfigScope::Session => 4,
            ProofConfigScope::Task => 5,
        }
    }
}

/// One ordered stamp of the layered effective configuration as bound into a
/// proof basis: the scope, the layer's monotonic revision, and the digest of
/// the layer's effective values. Any of the three changing is a different
/// layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofConfigLayer {
    pub scope: ProofConfigScope,
    pub revision: u64,
    pub digest: String,
}

impl ProofConfigLayer {
    /// Build one layer stamp from an opaque effective-value digest supplied
    /// by the configuration authority. The digest is validated (bounded,
    /// printable, non-empty) — never trusted blindly into a durable row.
    pub fn new(
        scope: ProofConfigScope,
        revision: u64,
        digest: impl Into<String>,
    ) -> Result<Self, TaskError> {
        let digest = digest.into();
        validate_proof_config_digest(&digest)?;
        Ok(Self {
            scope,
            revision,
            digest,
        })
    }

    /// Build one layer stamp from a layer VALUE by digesting it under the
    /// proof-config domain. Deterministic: the same scope/revision/value is
    /// the same stamp; changing the value changes the digest.
    pub fn of_value(scope: ProofConfigScope, revision: u64, value: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor-proof-config-layer:v1\0");
        hasher.update(scope.as_str().as_bytes());
        hasher.update(&[0]);
        hasher.update(&revision.to_le_bytes());
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
        Self {
            scope,
            revision,
            digest: format!("blake3:{}", hasher.finalize().to_hex()),
        }
    }
}

/// The layered effective-configuration digest: domain-separated over the
/// ordered (outermost-to-innermost) layer stamps, so ANY layer change — a
/// system-layer value, an organization policy revision — yields a different
/// digest and every proof basis bound to the previous digest stops being
/// reusable. The list must be strictly ordered by scope rank (a duplicated
/// or out-of-order layer is refused, never silently sorted).
pub fn layered_effective_config_digest(layers: &[ProofConfigLayer]) -> Result<String, TaskError> {
    if layers.len() > MAX_PROOF_CONFIG_LAYERS {
        return Err(TaskError::Malformed(format!(
            "layered configuration binding carries {} layers (max {MAX_PROOF_CONFIG_LAYERS})",
            layers.len()
        )));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-effective-config:v1\0");
    let mut previous: Option<ProofConfigScope> = None;
    for layer in layers {
        if let Some(previous) = previous {
            if layer.scope.rank() <= previous.rank() {
                return Err(TaskError::Malformed(format!(
                    "layered configuration binding is out of order: {} after {} (strict scope order required)",
                    layer.scope.as_str(),
                    previous.as_str()
                )));
            }
        }
        validate_proof_config_digest(&layer.digest)?;
        hasher.update(&[layer.scope.rank()]);
        hasher.update(layer.scope.as_str().as_bytes());
        hasher.update(&[0]);
        hasher.update(&layer.revision.to_le_bytes());
        hasher.update(&[0]);
        hasher.update(layer.digest.as_bytes());
        hasher.update(&[0]);
        previous = Some(layer.scope);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// The canonical BLAKE3 authority digest of one verification record's
/// check-command basis (the fingerprint's `check_argv_cwd_env_hash`): per
/// check the id, the program and every argv element as its own ordered
/// length-prefixed field — never a joined string. The fingerprint schema
/// requires BARE hex text, so the `blake3:` label stays out of this value.
fn check_basis_digest(checks: &[CheckExecution]) -> String {
    let mut fields = Fields::new().uint(checks.len() as u64);
    for check in checks {
        fields = fields
            .text(&check.check)
            .text(&check.program)
            .list(&check.args);
    }
    authority_digest_hex(DOMAIN_CHECK_BASIS, 1, fields)
}

fn validate_proof_config_digest(digest: &str) -> Result<(), TaskError> {
    if digest.is_empty() || digest.len() > MAX_PROOF_CONFIG_DIGEST_BYTES {
        return Err(TaskError::Malformed(format!(
            "configuration digest must be 1..={MAX_PROOF_CONFIG_DIGEST_BYTES} bytes"
        )));
    }
    if !digest.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(TaskError::Malformed(
            "configuration digest must be printable ASCII without whitespace".into(),
        ));
    }
    Ok(())
}

/// The result of asking whether an existing proof record may be reused under
/// a current basis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofReuse {
    /// The record's basis is byte-identical to the current basis.
    Allowed,
    /// Reuse is refused (the reason names the basis mismatch class).
    Refused { reason: String },
}

impl ProofReuse {
    pub fn is_allowed(&self) -> bool {
        matches!(self, ProofReuse::Allowed)
    }
}

/// The immutable binding of a completion proof, returned by
/// [`SessionHandle::verify_completion_proof_binding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionProofBinding {
    pub record_id: VerificationRecordId,
    pub task_id: TaskId,
    pub revision: TaskRevision,
    pub tree_hash: Option<String>,
    pub criterion_count: usize,
    pub check_count: usize,
}

impl SessionHandle {
    /// The session's durable task identity (the adopted `task_id` on the
    /// session row; standalone sessions default to 1).
    pub fn task_id(&self) -> faktor_core::Result<TaskId> {
        Ok(self.row()?.task_id)
    }

    /// Create ONE durable task row (audit P0-7). Creation is the ONLY place
    /// a row may seed with `Pending`/`Planning`/`Running`; every other
    /// state is refused (creating a "verified" task would mint completion
    /// proof from nothing), and an existing row for `(session_id, task_id)`
    /// is refused — a task row is created once and mutated afterwards.
    /// Fresh rows start at revision 1. Oversized fields are rejected with
    /// an error BEFORE any write — never truncated silently.
    pub fn create_task(&self, task: Task) -> faktor_core::Result<Task> {
        if task.task_id.raw() == 0 || task.session_id.raw() == 0 {
            return Err(
                SessionError::Malformed("task_id and session_id must be non-zero".into()).into(),
            );
        }
        if !task.state.is_creatable() {
            return Err(faktor_core::Error::new(
                faktor_core::ErrorKind::Conflict,
                format!(
                    "create_task refused: state {:?} cannot seed a task; only Pending, Planning or Running may (VerifiedComplete requires a passing verification record via complete_verified_task)",
                    task.state
                ),
            ));
        }
        validate_task_fields(&task)?;
        if task.created_ms == 0 {
            return Err(SessionError::Malformed("created_ms must be set".into()).into());
        }
        let _guard = self.command_guard();
        let store = self.manager.store();
        if store
            .get_task(self.id, task.task_id)
            .map_err(crate::map_store_err)?
            .is_some()
        {
            return Err(faktor_core::Error::new(
                faktor_core::ErrorKind::Conflict,
                format!(
                    "create_task refused: task {} already exists for this session; a task row is created once and mutated through update_task/transition_task/complete_verified_task, never recreated",
                    task.task_id
                ),
            ));
        }
        store
            .upsert_task(&task_row(task.clone(), TaskRevision::new(1)))
            .map_err(crate::map_store_err)?;
        Ok(task)
    }

    /// Patch ONE durable task row under the task state machine (audit
    /// P0-7). Fields that validate and are present are applied; the other
    /// fields keep their current values; `created_ms` is preserved by
    /// construction.
    ///
    /// Enforcement, in order:
    /// 1. a patch whose `state` is completion-relevant
    ///    (`NeedsVerification`/`Verifying`/`VerifiedComplete`) is rejected
    ///    with [`TaskError::CompletionStateViaPatch`] — those states are
    ///    produced only by `transition_task`/`complete_verified_task`;
    /// 2. a patch whose `state` would jump an edge the machine does not
    ///    allow from the row's current state is rejected with
    ///    [`TaskError::IllegalTransition`] (self-assignment is an
    ///    idempotent no-op);
    /// 3. a patch that effectively changes the content of a TERMINAL row
    ///    (VerifiedComplete/Failed/Cancelled) is rejected with
    ///    [`TaskError::TerminalTask`].
    ///
    /// Every effective change bumps the row revision exactly once. A no-op
    /// patch (nothing actually changes — e.g. a spend heal that found
    /// nothing to heal) writes nothing and does not bump.
    pub fn update_task(&self, task_id: TaskId, patch: TaskPatch) -> Result<Task, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        let _guard = self.command_guard();
        let store = self.manager.store();
        let row = store
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        let current = Task::from(row.clone());
        let mut next = current.clone();
        if let Some(goal) = patch.goal {
            next.goal = goal;
        }
        if let Some(criteria) = patch.acceptance_criteria {
            next.acceptance_criteria = criteria;
        }
        if let Some(plan) = patch.plan {
            next.plan = plan;
        }
        if let Some(attachments) = patch.attachments {
            next.attachments = attachments;
        }
        if let Some(budget) = patch.budget {
            // The durable budget cap: spent counters only ever move
            // forward. A patch that would rewind spend is a corruption sign
            // (two writers cannot both gate on a rewindable counter).
            next.budget.spent_tokens = budget.spent_tokens.max(next.budget.spent_tokens);
            next.budget.spent_turns = budget.spent_turns.max(next.budget.spent_turns);
            next.budget.max_tokens = budget.max_tokens;
            next.budget.max_turns = budget.max_turns;
        }
        if let Some(state) = patch.state {
            if state.is_completion_relevant() {
                return Err(TaskError::CompletionStateViaPatch { state });
            }
            if state != row.state && !row.state.allowed_transitions().contains(&state) {
                return Err(TaskError::IllegalTransition {
                    from: row.state,
                    to: state,
                    detail: "update_task only drives ordinary machine edges; completion states (NeedsVerification/Verifying/VerifiedComplete) require transition_task/complete_verified_task".into(),
                });
            }
            next.state = state;
        }
        validate_task_fields(&next)?;
        if next == current {
            // Idempotent no-op (replay / heal): nothing to bump, no write.
            return Ok(current);
        }
        if row.state.is_terminal() {
            return Err(TaskError::TerminalTask {
                task_id,
                state: row.state,
            });
        }
        let revision = row
            .revision
            .checked_next()
            .ok_or_else(|| TaskError::Malformed("task revision overflow".into()))?;
        let mut out = task_row(next, revision);
        out.updated_ms = self.manager.now_ms();
        store.upsert_task(&out)?;
        Ok(Task::from(out))
    }

    /// Atomically seed ONE task's durable attachment set (audit finding 2):
    /// every id is structurally validated and every CAS blob is verified by a
    /// streamed re-hash (`Cas::verify_now`, no bytes materialize) BEFORE
    /// anything is written; the reference rows AND the task-row write then
    /// land in ONE store `BEGIN IMMEDIATE` transaction, so the seed either
    /// fully succeeds or leaves the durable state unchanged — reference rows
    /// included.
    ///
    /// `create` is the CREATE template, used only when the caller expects no
    /// task row yet (the orchestrated child): `Some(task)` inserts that row
    /// with revision 1 when none exists and refuses typed when one does
    /// ([`TaskError::AlreadyExists`]). `None` patches the EXISTING row under
    /// the preflight revision, non-terminal state and current attachment set,
    /// all of which the transaction re-verifies (a concurrent change is a
    /// typed refusal with zero writes).
    ///
    /// A terminal task row is frozen: `Ok(None)` with zero writes (no
    /// reference rows, no task patch).
    pub fn seed_task_attachments(
        &self,
        attachments: &[AttachmentId],
        create: Option<Task>,
    ) -> Result<Option<Task>, TaskError> {
        if attachments.is_empty() {
            return Ok(None);
        }
        if attachments.len() > MAX_ATTACHMENTS_PER_TASK {
            return Err(TaskError::Oversized(format!(
                "{} seeded attachments exceed MAX_ATTACHMENTS_PER_TASK ({MAX_ATTACHMENTS_PER_TASK})",
                attachments.len()
            )));
        }
        for id in attachments {
            id.validate().map_err(task_error_from_core)?;
        }
        for id in attachments {
            self.manager
                .cas()
                .verify_now(&id.digest.to_hex())
                .map_err(|e| match e {
                    faktor_cas::CasError::NotFound(hash) => {
                        TaskError::AttachmentBlobNotFound(hash.to_string())
                    }
                    other => TaskError::Store(other.to_string()),
                })?;
        }
        let _guard = self.command_guard();
        let task_id = self.task_id().map_err(|e| TaskError::Store(e.message))?;
        if let Some(template) = &create {
            if template.task_id != task_id || template.session_id != self.id {
                return Err(TaskError::Malformed(
                    "the seed create template must carry this session's own task identity".into(),
                ));
            }
            if !template.state.is_creatable() {
                return Err(TaskError::IllegalCreateState {
                    state: template.state,
                });
            }
        }
        let store = self.manager.store();
        let existing = store.get_task(self.id, task_id)?;
        if existing.as_ref().is_some_and(|row| row.state.is_terminal()) {
            return Ok(None);
        }
        let mut prepared_refs = Vec::with_capacity(attachments.len());
        for id in attachments {
            prepared_refs
                .push(faktor_store::PreparedAttachmentRef::prepare(id).map_err(TaskError::from)?);
        }
        let now = self.manager.now_ms();
        let (target, expected_revision, expected_attachments, expected_nonterminal, write) =
            match create {
                Some(mut template) => {
                    template.attachments = attachments.to_vec();
                    if template.created_ms == 0 {
                        template.created_ms = now;
                    }
                    template.updated_ms = now;
                    validate_task_fields(&template)?;
                    let write = faktor_store::PreparedTaskWrite::prepare(task_row(
                        template,
                        TaskRevision::new(1),
                    ))
                    .map_err(TaskError::from)?;
                    (None, None, None, false, write)
                }
                None => {
                    let Some(row) = existing else {
                        return Err(TaskError::NotFound(task_id));
                    };
                    let mut next = Task::from(row.clone());
                    next.attachments = attachments.to_vec();
                    next.updated_ms = now;
                    validate_task_fields(&next)?;
                    let revision = row
                        .revision
                        .checked_next()
                        .ok_or_else(|| TaskError::Malformed("task revision overflow".into()))?;
                    let write = faktor_store::PreparedTaskWrite::prepare(task_row(next, revision))
                        .map_err(TaskError::from)?;
                    (
                        Some(task_id),
                        Some(row.revision),
                        Some(row.attachments),
                        true,
                        write,
                    )
                }
            };
        let outcome = store
            .seed_task_attachments_txn(
                self.id,
                target,
                expected_revision,
                expected_attachments,
                expected_nonterminal,
                &prepared_refs,
                write,
            )
            .map_err(TaskError::from)?;
        match outcome {
            Ok(row) => Ok(Some(Task::from(row))),
            Err(refusal) => Err(seed_refusal_to_task_error(task_id, refusal)),
        }
    }

    /// Drive ONE legal machine edge (audit P0-7) — the single chokepoint
    /// for `Pending`/`Planning`/`Running`/`Waiting`/`Blocked`/`Verifying`
    /// state changes plus cancellation. `expected_revision` must equal the
    /// row's current revision (typed [`TaskError::RevisionMismatch`]
    /// otherwise); the edge must be legal from the row's current state
    /// (typed [`TaskError::IllegalTransition`]). Success bumps the revision
    /// exactly once. `proof` is refused here with a typed error: only
    /// [`SessionHandle::complete_verified_task`] consumes a verification
    /// record, and `VerifiedComplete` has no `TaskTransition` edge at all.
    pub fn transition_task(
        &self,
        task_id: TaskId,
        expected_revision: TaskRevision,
        transition: TaskTransition,
        proof: Option<VerificationRecordId>,
    ) -> Result<Task, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        if let Some(proof) = proof {
            return Err(TaskError::Malformed(format!(
                "transition_task does not consume a proof ({proof}); a completion proof belongs to complete_verified_task, and VerifiedComplete is unreachable by transition"
            )));
        }
        let _guard = self.command_guard();
        let store = self.manager.store();
        let row = store
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        if row.revision != expected_revision {
            return Err(TaskError::RevisionMismatch {
                task_id,
                expected: expected_revision,
                actual: row.revision,
            });
        }
        if !transition.legal_from(row.state) {
            return Err(TaskError::IllegalTransition {
                from: row.state,
                to: transition.to_state(),
                detail: format!(
                    "transition_task({transition:?}): the task is {:?}; re-read the row and pick a legal edge",
                    row.state
                ),
            });
        }
        let revision = expected_revision
            .checked_next()
            .ok_or_else(|| TaskError::Malformed("task revision overflow".into()))?;
        let mut out = task_row(Task::from(row), revision);
        out.state = transition.to_state();
        out.updated_ms = self.manager.now_ms();
        store.upsert_task(&out)?;
        Ok(Task::from(out))
    }

    /// Complete a verified task (audit P0-7/P0-8 + attempt-accounting
    /// extension): the ONLY path to `VerifiedComplete`. The completion runs
    /// as one ordered, crash-resumable sequence — every step is its own
    /// durable unit, so a crash at any seam leaves the task Verifying and a
    /// later completion pass converges (all monetary steps are idempotent):
    ///
    /// 1. **Proof load + validation** (read-only mirror of the store checks
    ///    (a)-(g) below — a lying proof never triggers a monetary step);
    /// 2. **Accounting-before-completion**: (a) reconcile every UNCERTAIN
    ///    provider attempt whose exact usage is known (a completed durable
    ///    provider-call row of that attempt); (b) conservatively settle any
    ///    remaining UNCERTAIN attempt AT ITS RESERVED ESTIMATE; (c) assert
    ///    ZERO open (reserved/dispatched) and ZERO UNCERTAIN reservations
    ///    remain — the final monetary spend is folded by the settlements
    ///    themselves (the task row's spent columns are written in the same
    ///    store transactions). Any failure is a typed
    ///    [`TaskError::AccountingFailure`]/[`TaskError::AccountingIncomplete`]
    ///    and the task STAYS Verifying;
    /// 3. **Transition** — the whole proof validation runs AGAIN inside the
    ///    ONE store transaction: (a) the task is `Verifying` (a
    ///    `NeedsVerification` task must transition to `Verifying` first —
    ///    completion never skips the verifier), (b) the record exists,
    ///    (c) it certifies THIS task, (d) it certifies exactly
    ///    `expected_revision` == the task's current revision, (e) its status
    ///    is `Passed`, (f) it covers every current acceptance criterion of
    ///    the task (extra record criteria are fine), (g) its
    ///    workspace/worktree equal the task's current base worktree.
    ///    Success writes `VerifiedComplete` and bumps the revision exactly
    ///    once.
    ///
    /// Invariant (locked by fault tests): a row in `VerifiedComplete` has
    /// zero open + zero uncertain reservations and its final monetary
    /// totals folded — the accounting gate ran in the same logical
    /// completion before the transition CAS.
    pub fn complete_verified_task(
        &self,
        task_id: TaskId,
        expected_revision: TaskRevision,
        proof: VerificationRecordId,
    ) -> Result<Task, TaskError> {
        self.complete_verified_task_crashable(task_id, expected_revision, proof, None)
            .map(|t| t.expect("the full completion sequence always reaches the transition"))
    }

    /// Crash-seamed twin of [`SessionHandle::complete_verified_task`]
    /// (adversarial fault tests): `crash` simulates process death at one
    /// seam of the sequence — the steps up to (not including) the seam ran
    /// and committed durably, everything after it did not. `Ok(None)` =
    /// the simulated crash point; the caller reopens the store and asserts
    /// the task still reads `Verifying` and the accounting invariant, then
    /// re-runs the FULL completion (which converges — every monetary step
    /// is idempotent). `None` runs the whole sequence.
    fn complete_verified_task_crashable(
        &self,
        task_id: TaskId,
        expected_revision: TaskRevision,
        proof: VerificationRecordId,
        crash: Option<CompletionCrashPoint>,
    ) -> Result<Option<Task>, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        let _guard = self.command_guard();
        // ---- step 1: verification proof load + validation (read-only) ----
        self.validate_completion_proof(task_id, expected_revision, proof)?;
        if crash == Some(CompletionCrashPoint::AfterProofLoad) {
            return Ok(None);
        }
        // ---- step 1b: the durable PR/CI-fix completion-contract gate
        // (P2). Every requested commit/push/PR step of the run's accepted
        // contract must carry a durable Succeeded step-status row; a missing
        // or non-Succeeded step refuses here, BEFORE any monetary step, and
        // the row stays Verifying. A Failed step is a terminal refusal.
        if let CompletionContractGate::Refused(err) = self.completion_contract_gate(task_id)? {
            return Err(err);
        }
        // ---- step 2: accounting-before-completion (sync, idempotent).
        // A seam inside the pass stops the whole sequence there.
        if self.run_completion_accounting(task_id, crash)? {
            return Ok(None);
        }
        // ---- step 3: the atomic transition CAS (re-validates + writes
        // VerifiedComplete exactly once; the ONLY completion writer) ----
        if crash == Some(CompletionCrashPoint::BeforeTransition) {
            return Ok(None);
        }
        let outcome = self.manager.store().task_complete_verified(
            self.id,
            task_id,
            expected_revision,
            proof,
            self.manager.now_ms(),
        )?;
        match outcome {
            Ok(row) => Ok(Some(Task::from(row))),
            Err(refusal) => Err(match refusal {
                faktor_store::TaskCompletionRefusal::TaskMissing { .. } => {
                    TaskError::NotFound(task_id)
                }
                faktor_store::TaskCompletionRefusal::RevisionMismatch { expected, actual } => {
                    TaskError::RevisionMismatch {
                        task_id,
                        expected,
                        actual,
                    }
                }
                faktor_store::TaskCompletionRefusal::NotVerifying { actual } => {
                    TaskError::NotVerifying { actual }
                }
                faktor_store::TaskCompletionRefusal::RecordMissing { record_id } => {
                    TaskError::RecordNotFound(record_id)
                }
                faktor_store::TaskCompletionRefusal::RecordWrongTask {
                    record_id,
                    record_task,
                    ..
                } => TaskError::RecordWrongTask {
                    record: record_id,
                    record_task,
                    requested_task: task_id,
                },
                faktor_store::TaskCompletionRefusal::RecordWrongRevision {
                    record_id,
                    record_revision,
                    ..
                } => TaskError::RecordWrongRevision {
                    record: record_id,
                    record_revision,
                    expected: expected_revision,
                },
                faktor_store::TaskCompletionRefusal::RecordNotPassed { record_id, status } => {
                    TaskError::RecordNotPassed {
                        record: record_id,
                        status,
                    }
                }
                faktor_store::TaskCompletionRefusal::CriteriaNotCovered { record_id, missing } => {
                    TaskError::CriteriaNotCovered {
                        record: record_id,
                        missing,
                    }
                }
                faktor_store::TaskCompletionRefusal::WorktreeMismatch {
                    record_id,
                    record_workspace,
                    record_worktree,
                    task_workspace,
                    task_worktree,
                } => TaskError::WorktreeMismatch {
                    record: record_id,
                    record_workspace,
                    record_worktree,
                    task_workspace,
                    task_worktree,
                },
                faktor_store::TaskCompletionRefusal::ReservationsHeld {
                    reserved,
                    dispatched,
                    reserved_micro,
                    uncertain,
                    uncertain_micro,
                } => TaskError::AccountingIncomplete {
                    task_id,
                    open_count: reserved.saturating_add(dispatched),
                    open_micro: reserved_micro,
                    dispatched_count: dispatched,
                    uncertain_count: uncertain,
                    uncertain_micro,
                },
            }),
        }
    }

    // ------------------------------------------- completion contract (P2)

    /// Record the accepted PR/CI-fix completion contract of ONE task run at
    /// `revision` (the task row's revision when the run started). Immutable
    /// per task revision: a second set for the same `(task_id, revision)` is
    /// a typed [`TaskError::CompletionContractImmutable`]. The task row must
    /// exist; an all-false contract is refused (it is the default behavior,
    /// expressed by the absence of a row). Record-first: callers append this
    /// BEFORE the run's first model call.
    pub fn set_completion_contract(
        &self,
        task_id: TaskId,
        revision: TaskRevision,
        contract: CompletionContract,
    ) -> Result<Option<i64>, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        if contract.is_default() {
            return Err(TaskError::Malformed(
                "an all-false completion contract is the default behavior; no row is recorded"
                    .into(),
            ));
        }
        let row = self
            .manager
            .store()
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        if row.revision != revision {
            // The contract names the run's START revision; a stale or
            // fabricated revision would record a contract the completion
            // gate could never match to a real run.
            return Err(TaskError::RevisionMismatch {
                task_id,
                expected: revision,
                actual: row.revision,
            });
        }
        match self.ledger_completion_contract_set(task_id.raw(), revision.raw(), &contract) {
            Ok(seq) => Ok(seq),
            Err(e) if e.kind == faktor_core::ErrorKind::Conflict => {
                Err(TaskError::CompletionContractImmutable { task_id, revision })
            }
            Err(e) => Err(task_error_from_core(e)),
        }
    }

    /// The task's ACCEPTED completion contract (the newest durable set row)
    /// and the task revision it was recorded against; `None` when the task
    /// never carried a non-default contract.
    ///
    /// FIX 2: the read is explicitly classified — a genuinely absent
    /// contract is `None` (not-found policy), a PRESENT-but-corrupt contract
    /// row is a typed [`TaskError::CorruptDurableState`], and a failed
    /// durable read is [`TaskError::Store`] (never "no contract").
    pub fn completion_contract(
        &self,
        task_id: TaskId,
    ) -> Result<Option<(TaskRevision, CompletionContract)>, TaskError> {
        let row = match self.ledger_completion_contract_read(task_id.raw()) {
            DurableRead::Missing => return Ok(None),
            DurableRead::PresentValid(row) => row,
            DurableRead::PresentMalformed(detail) => {
                return Err(TaskError::CorruptDurableState {
                    what: format!("completion contract of task {task_id}"),
                    detail,
                })
            }
            DurableRead::StoreFailure(detail) => return Err(TaskError::Store(detail)),
        };
        let revision =
            TaskRevision::try_from(row.revision).map_err(|e| TaskError::CorruptDurableState {
                what: format!("completion contract revision of task {task_id}"),
                detail: format!(
                    "stored completion contract revision {} is invalid: {e}",
                    row.revision
                ),
            })?;
        Ok(Some((revision, row.contract)))
    }

    /// Record one per-step outcome of the task's accepted run (the step
    /// executor's durable seam, follow-up machinery). The row lands against
    /// the contract's run revision so the gate can evaluate it after the
    /// task row itself has moved revisions. A step outcome for a task with
    /// no accepted contract is refused; a `Failed` row is a terminal gate
    /// refusal.
    ///
    /// NOTE (P2 follow-up): commit/push/PR step EXECUTION does not exist in
    /// this tree yet. Callers/tests record the outcomes through THIS setter;
    /// the completion gate is what this change lands.
    pub fn set_completion_step_status(
        &self,
        task_id: TaskId,
        step: CompletionStep,
        status: CompletionStepOutcome,
        detail: &str,
    ) -> Result<i64, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        let Some((revision, _contract)) = self.completion_contract(task_id)? else {
            return Err(TaskError::Malformed(format!(
                "task {task_id} has no accepted completion contract; step outcomes need a contract revision"
            )));
        };
        // P0 binding: when the task carries a finalized integration record,
        // the step outcome references its final root snapshot. A step is
        // then only admissible while the root still digests to that
        // snapshot; the completion gate re-checks it. FIX 2: the read is
        // classified — absent is legal (no integration), a corrupt row
        // refuses typed, and a store failure is an error.
        let integration = match self.ledger_integration_record_for_task_read(task_id.raw()) {
            DurableRead::Missing => None,
            DurableRead::PresentValid(record) => Some(record),
            DurableRead::PresentMalformed(detail) => {
                return Err(TaskError::CorruptDurableState {
                    what: format!("integration record of task {task_id}"),
                    detail,
                })
            }
            DurableRead::StoreFailure(detail) => return Err(TaskError::Store(detail)),
        };
        let snapshot = integration
            .as_ref()
            .map(|r| r.final_snapshot_hash.clone())
            .filter(|h| !h.is_empty());
        let seq = self
            .ledger_completion_step_status_with_snapshot(
                task_id.raw(),
                revision.raw(),
                step,
                status,
                detail,
                snapshot.as_deref(),
                self.now_ms(),
            )
            .map_err(task_error_from_core)?
            .ok_or_else(|| {
                TaskError::Malformed("completion step status append returned no seq".into())
            })?;
        Ok(seq)
    }

    /// The durable completion-contract gate: loads the task's accepted
    /// contract and requires, for every requested step in gate order
    /// (commit, push, pr), a durable `Succeeded` step-status row at the
    /// contract's revision. Missing or non-Succeeded steps are typed
    /// refusals naming the unmet step; a `Failed` row is a terminal
    /// refusal. With no contract (or the default) the gate is satisfied and
    /// reads nothing further: the default completion path is byte-identical.
    pub fn completion_contract_gate(
        &self,
        task_id: TaskId,
    ) -> Result<CompletionContractGate, TaskError> {
        let row = match self.ledger_completion_contract_read(task_id.raw()) {
            DurableRead::Missing => return Ok(CompletionContractGate::Satisfied),
            DurableRead::PresentValid(row) => row,
            // FIX 2: a present-but-corrupt contract NEVER resolves to the
            // satisfied default — it refuses typed.
            DurableRead::PresentMalformed(detail) => {
                return Err(TaskError::CorruptDurableState {
                    what: format!("completion contract of task {task_id}"),
                    detail,
                })
            }
            DurableRead::StoreFailure(detail) => return Err(TaskError::Store(detail)),
        };
        if row.contract.is_default() {
            return Ok(CompletionContractGate::Satisfied);
        }
        let revision =
            TaskRevision::try_from(row.revision).map_err(|e| TaskError::CorruptDurableState {
                what: format!("completion contract revision of task {task_id}"),
                detail: format!(
                    "stored completion contract revision {} is invalid: {e}",
                    row.revision
                ),
            })?;
        let rows = self
            .ledger_completion_step_statuses(task_id.raw(), row.revision)
            .map_err(task_error_from_core)?;
        for step in row.contract.requested_steps() {
            let step_rows: Vec<&crate::ledger::CompletionStepStatusRow> =
                rows.iter().filter(|r| r.step == step).collect();
            if let Some(failed) = step_rows
                .iter()
                .find(|r| r.status == CompletionStepOutcome::Failed)
            {
                // A failed step is terminal: even a later Succeeded row
                // cannot resurrect the run's contract revision.
                return Ok(CompletionContractGate::Refused(
                    TaskError::CompletionStepFailed {
                        task_id,
                        revision,
                        step,
                        detail: failed.detail.clone(),
                    },
                ));
            }
            match step_rows.last() {
                Some(latest) if latest.status == CompletionStepOutcome::Succeeded => {
                    // P0 binding: a step recorded against a final integration
                    // snapshot only satisfies the gate while the root still
                    // digests to it. The completed step is never silently
                    // detached from the root it ran over.
                    if let Some(recorded) = latest.snapshot.as_deref() {
                        let current = match self.current_root_snapshot_digest() {
                            Ok(Some(current)) => current,
                            Ok(None) => {
                                return Ok(CompletionContractGate::Refused(
                                    TaskError::RootSnapshotUnavailable(
                                        "the session workspace root is unresolvable; the step \
                                         outcome cannot be bound to it"
                                            .into(),
                                    ),
                                ))
                            }
                            Err(e @ TaskError::RootSnapshotUnavailable(_)) => {
                                return Ok(CompletionContractGate::Refused(e))
                            }
                            Err(e @ TaskError::RootSnapshotSpecialFile { .. }) => {
                                return Ok(CompletionContractGate::Refused(e))
                            }
                            Err(e) => return Err(e),
                        };
                        if current != recorded {
                            return Ok(CompletionContractGate::Refused(
                                TaskError::CompletionStepSnapshotMismatch {
                                    task_id,
                                    revision,
                                    step,
                                    recorded: recorded.to_string(),
                                    current,
                                },
                            ));
                        }
                    }
                }
                Some(latest) => {
                    return Ok(CompletionContractGate::Refused(
                        TaskError::CompletionStepNotSucceeded {
                            task_id,
                            revision,
                            step,
                            status: latest.status,
                            detail: latest.detail.clone(),
                        },
                    ))
                }
                None => {
                    return Ok(CompletionContractGate::Refused(
                        TaskError::CompletionStepMissing {
                            task_id,
                            revision,
                            step,
                        },
                    ))
                }
            }
        }
        Ok(CompletionContractGate::Satisfied)
    }

    /// Read-only mirror of the store completion transaction's proof checks
    /// (a)-(g) — run BEFORE any monetary step so a lying proof never moves
    /// money. The store re-validates the same checks atomically at the
    /// transition CAS; drift here only changes WHEN accounting runs, never
    /// the final gate (the store stays authoritative).
    fn validate_completion_proof(
        &self,
        task_id: TaskId,
        expected_revision: TaskRevision,
        proof: VerificationRecordId,
    ) -> Result<(), TaskError> {
        let store = self.manager.store();
        let row = store
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        if row.revision != expected_revision {
            return Err(TaskError::RevisionMismatch {
                task_id,
                expected: expected_revision,
                actual: row.revision,
            });
        }
        if row.state != TaskState::Verifying {
            return Err(TaskError::NotVerifying { actual: row.state });
        }
        let rec = store
            .verification_record_get(proof)?
            .ok_or(TaskError::RecordNotFound(proof))?;
        if rec.task_id != task_id {
            return Err(TaskError::RecordWrongTask {
                record: proof,
                record_task: rec.task_id,
                requested_task: task_id,
            });
        }
        if rec.revision != expected_revision {
            return Err(TaskError::RecordWrongRevision {
                record: proof,
                record_revision: rec.revision,
                expected: expected_revision,
            });
        }
        if rec.status != VerificationStatus::Passed {
            return Err(TaskError::RecordNotPassed {
                record: proof,
                status: rec.status,
            });
        }
        let expected: Vec<(String, CriterionBinding, bool)> = row
            .acceptance_criteria
            .iter()
            .map(|entry| {
                // A V2-encoded criterion is MODERN (its binding is stored on
                // the criterion itself and migrated deterministically when
                // absent); a plain-text entry is legacy. The distinction is
                // load-bearing below: a binding-less verdict can never cover
                // a modern criterion.
                let (binding, modern) = match Criterion::decode(entry) {
                    Some(c) => (c.effective_binding(), true),
                    None => (legacy_binding_for_criterion_text(entry), false),
                };
                (entry.clone(), binding, modern)
            })
            .collect();
        let mut missing: Vec<String> = Vec::new();
        let mut legacy_unbound: Vec<String> = Vec::new();
        for (key, binding, modern) in &expected {
            // A criterion is covered only by a PASSED verdict: every MODERN
            // criterion additionally requires the verdict to carry ITS OWN
            // binding (`cv.binding == Some(expected_binding)`). A
            // binding-less verdict can never certify a modern criterion —
            // the legacy record stays viewable but post-upgrade recovery
            // forces re-verification (the typed outcome names the record).
            // Legacy plain-text criteria keep the historical key+passed
            // contract.
            let covered = rec.criteria.iter().any(|cv| {
                cv.passed
                    && &cv.criterion_key == key
                    && match (&cv.binding, modern) {
                        (Some(recorded), _) => recorded == binding,
                        (None, false) => true,
                        (None, true) => false,
                    }
            });
            if covered {
                continue;
            }
            let only_legacy_verdict = *modern
                && rec
                    .criteria
                    .iter()
                    .any(|cv| cv.passed && &cv.criterion_key == key && cv.binding.is_none());
            if only_legacy_verdict {
                legacy_unbound.push(key.clone());
            } else {
                missing.push(key.clone());
            }
        }
        if !legacy_unbound.is_empty() {
            return Err(TaskError::LegacyProofRequiresReverification {
                task_id,
                record: proof,
                criteria: legacy_unbound,
            });
        }
        if !missing.is_empty() {
            return Err(TaskError::CriteriaNotCovered {
                record: proof,
                missing,
            });
        }
        let base = store
            .get_session(self.id)?
            .ok_or(TaskError::NotFound(task_id))?;
        if rec.workspace_id != base.workspace_id || rec.worktree_id != base.worktree_id {
            return Err(TaskError::WorktreeMismatch {
                record: proof,
                record_workspace: rec.workspace_id,
                record_worktree: rec.worktree_id,
                task_workspace: base.workspace_id,
                task_worktree: base.worktree_id,
            });
        }
        // FIX 2 classification: the task's integration record is read with
        // the four-outcome distinction — absent is the not-found policy,
        // present uses the row, malformed is typed corruption, and a failed
        // store read is an error (never "nothing integrated").
        let integration = match self.ledger_integration_record_for_task_read(task_id.raw()) {
            DurableRead::Missing => None,
            DurableRead::PresentValid(record) => Some(record),
            DurableRead::PresentMalformed(detail) => {
                return Err(TaskError::CorruptDurableState {
                    what: format!("integration record of task {task_id}"),
                    detail,
                })
            }
            DurableRead::StoreFailure(detail) => return Err(TaskError::Store(detail)),
        };
        // A managed live shadow whose immutable run base is durably recorded
        // is mutation evidence: the session's effective root is not the
        // owner checkout and a proof without a tree hash can only certify
        // the un-integrated shadow world.
        let live_shadow_run_base = match self
            .manager
            .shadow_row(self.id)
            .map_err(|e| TaskError::Store(e.to_string()))?
        {
            Some(shadow) if shadow.state.is_live() => {
                match self.ledger_run_base_read(&shadow.shadow_id) {
                    DurableRead::Missing => false,
                    DurableRead::PresentValid(_) => true,
                    DurableRead::PresentMalformed(detail) => {
                        return Err(TaskError::CorruptDurableState {
                            what: format!("run base of shadow {}", shadow.shadow_id),
                            detail,
                        })
                    }
                    DurableRead::StoreFailure(detail) => return Err(TaskError::Store(detail)),
                }
            }
            _ => false,
        };
        // FIX 1: a MUTATING completion requires a canonical manifest-bound
        // tree hash. Durable mutation evidence is exactly: the task carries
        // an integration record (its changes landed/staged through the
        // integration pipeline) or a live managed shadow has a recorded run
        // base (its effective root is not the owner checkout). A plain
        // in-session record with no such evidence is the legacy single-
        // session contract: its criteria are still certified through their
        // own bindings above, and the executor's shadow settlement mints the
        // manifest-bound record whenever isolation is in force. The former
        // "records without a tree hash keep legacy behavior" blanket branch
        // is deleted: with durable mutation evidence, a completion without a
        // manifest-bound tree_hash is typed-refused.
        if let Some(recorded) = rec.tree_hash.as_deref() {
            let Some(integration) = integration else {
                return Err(TaskError::IntegrationRecordMissing {
                    task_id,
                    record: proof,
                });
            };
            if integration.final_snapshot_hash.is_empty()
                || integration.final_snapshot_hash != recorded
            {
                return Err(TaskError::IntegrationSnapshotMismatch {
                    task_id,
                    record: proof,
                    recorded: recorded.to_string(),
                    current: integration.final_snapshot_hash,
                });
            }
            match self.current_root_snapshot_digest()? {
                Some(current) if current == recorded => {}
                Some(current) => {
                    return Err(TaskError::IntegrationSnapshotMismatch {
                        task_id,
                        record: proof,
                        recorded: recorded.to_string(),
                        current,
                    })
                }
                None => {
                    return Err(TaskError::IntegrationSnapshotUnavailable {
                        task_id,
                        record: proof,
                        detail: "the session workspace root is unresolvable".into(),
                    })
                }
            }
        } else if integration.is_some() {
            // Durable mutation evidence (an integration record exists) but
            // the proof carries no manifest binding: typed refusal. The
            // record can never certify a mutation it does not bound.
            return Err(TaskError::ManifestBindingMissing {
                task_id,
                record: proof,
            });
        } else if live_shadow_run_base {
            // The existing typed shadow refusal (byte-compatible): the
            // shadow-world proof predates landing and must be re-verified
            // after the executor lands the candidate.
            return Err(TaskError::IntegrationRecordMissing {
                task_id,
                record: proof,
            });
        }
        Ok(())
    }

    /// The accounting-before-completion pass (step 2 of
    /// [`SessionHandle::complete_verified_task`]): reconcile → conservative
    /// finalize → assert ZERO. Sync over the durable ledger (each monetary
    /// step is its own store transaction; all are idempotent, so a crash
    /// anywhere and a later re-run converge). Any failure is typed and the
    /// task row is untouched (it stays Verifying).
    /// `Ok(true)` = the crash seam fired inside the pass (the caller stops
    /// the whole completion sequence there — the simulated process death).
    fn run_completion_accounting(
        &self,
        task_id: TaskId,
        crash: Option<CompletionCrashPoint>,
    ) -> Result<bool, TaskError> {
        let ledger = crate::budget::DurableBudgetLedger::new(self.manager.clone());
        let session_id = self.id;
        let account_err = |e: crate::budget::BudgetError| TaskError::AccountingFailure {
            task_id,
            detail: e.to_string(),
        };
        // (a) reconcile provider attempts whose exact usage is known: every
        // UNCERTAIN reservation whose ATTEMPT has a completed durable
        // provider-call row settles FROM that row's tokens at the
        // reservation's frozen snapshot.
        ledger
            .reconcile_uncertain_now(session_id, task_id)
            .map_err(account_err)?;
        if crash == Some(CompletionCrashPoint::AfterReconcile) {
            return Ok(true);
        }
        // (b) conservatively settle every still-UNCERTAIN attempt AT ITS
        // RESERVED ESTIMATE (the provider may have billed a dispatched
        // attempt whose actual never reconciled) — the final monetary spend
        // folds into the task row in the same transaction.
        ledger
            .finalize_uncertain_now(session_id, task_id)
            .map_err(account_err)?;
        if crash == Some(CompletionCrashPoint::AfterCostFold) {
            return Ok(true);
        }
        // (c) assert ZERO open (reserved/dispatched) and ZERO UNCERTAIN
        // reservations remain. A nonzero balance refuses completion — the
        // row never transitions and stays Verifying.
        let balance = ledger
            .completion_accounting_balance(session_id, task_id)
            .map_err(account_err)?;
        if !balance.is_zero() {
            return Err(TaskError::AccountingIncomplete {
                task_id,
                open_count: balance.open_count,
                open_micro: balance.open_micro,
                dispatched_count: balance.dispatched_count,
                uncertain_count: balance.uncertain_count,
                uncertain_micro: balance.uncertain_micro,
            });
        }
        Ok(false)
    }

    /// The row's current revision — the `expected_revision` a transition
    /// or completion must be called with.
    pub fn task_revision(&self, task_id: TaskId) -> Result<TaskRevision, TaskError> {
        let row = self
            .manager
            .store()
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        Ok(row.revision)
    }

    /// The CURRENT canonical tree-manifest digest of the session's effective
    /// root (the live shadow root while one is live, else the durable
    /// workspace root): the exact value the completion binding compares
    /// against a verification record's integration snapshot. `Ok(None)` when
    /// the session or its workspace row is unknown (no root — the caller
    /// fails closed).
    pub fn current_root_snapshot_digest(&self) -> Result<Option<String>, TaskError> {
        let Some(root) = self
            .manager
            .resolve_workspace_root(self.id)
            .map_err(|e| TaskError::Store(e.to_string()))?
        else {
            return Ok(None);
        };
        current_manifest_digest(&root).map(Some)
    }

    /// The durable task row identified by `task_id` (session-scoped).
    pub fn get_task(&self, task_id: TaskId) -> faktor_core::Result<Option<Task>> {
        self.manager
            .store()
            .get_task(self.id, task_id)
            .map_err(|e| crate::map_store_err(e).into())
            .map(|r| r.map(Task::from))
    }

    /// Every durable task row of this session (oldest-created first).
    pub fn list_tasks(&self) -> faktor_core::Result<Vec<Task>> {
        self.manager
            .store()
            .list_tasks(self.id)
            .map_err(|e| crate::map_store_err(e).into())
            .map(|rows| rows.into_iter().map(Task::from).collect())
    }

    /// The typed acceptance criteria of one durable task row (audits
    /// 56/57/105). Legacy plain-text entries migrate deterministically on
    /// read (stable content ids, inferred origin) and are NEVER rewritten by
    /// this read.
    pub fn task_criteria(&self, task_id: TaskId) -> Result<Vec<Criterion>, TaskError> {
        let row = self
            .manager
            .store()
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        Ok(Task::from(row).criteria())
    }

    /// Replace the task's acceptance criteria with an explicit typed set
    /// (audits 56/57/105). The set is validated (bounds, deterministic
    /// content ids, unique ids) and serialized as V2 JSON into the EXISTING
    /// criteria row values; an effective change bumps the row revision
    /// exactly once through [`SessionHandle::update_task`] — which is what
    /// invalidates any prior verification (its record pins the old revision).
    pub fn set_task_criteria(
        &self,
        task_id: TaskId,
        criteria: Vec<Criterion>,
    ) -> Result<Task, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        validate_criteria(&criteria)?;
        self.update_task(
            task_id,
            TaskPatch {
                acceptance_criteria: Some(encode_criteria(&criteria)),
                ..Default::default()
            },
        )
    }

    /// Re-derive the task's criteria: merge the freshly derived set with the
    /// durable row under the audit-56 rules (user criteria survive
    /// verbatim; derived criteria are authoritative for their origin;
    /// snapshot-stale derived criteria are re-derived), then persist through
    /// [`SessionHandle::set_task_criteria`] — one revision bump on any
    /// effective change.
    pub fn rederive_task_criteria(
        &self,
        task_id: TaskId,
        derived: Vec<Criterion>,
    ) -> Result<Task, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        validate_criteria(&derived)?;
        let row = self
            .manager
            .store()
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        let merged = merge_derived_criteria(&Task::from(row).criteria(), &derived);
        self.set_task_criteria(task_id, merged)
    }

    /// Crash-safe token spend of the session: the durable sum of every
    /// recorded provider call (input + output tokens).
    pub fn spent_tokens(&self) -> faktor_core::Result<u64> {
        self.manager
            .store()
            .session_usage_tokens(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Crash-safe logical-turn count of the session: durable
    /// `turn_completed` journal events.
    pub fn spent_turns(&self) -> faktor_core::Result<u64> {
        self.manager
            .store()
            .turn_completed_count(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// The configured wall-clock budget of one logical turn (ms; 0 =
    /// unbounded). The runtime caps every turn slice with this value.
    pub fn turn_budget_ms(&self) -> u64 {
        self.manager.turn_budget_ms()
    }

    // -------------------------------------------------- verification records

    /// Create one durable verification record certifying `task_id` at its
    /// CURRENT revision (audit P0-8). The record is written once and is
    /// immutable afterwards except the single CAS finalize
    /// (`Running -> Passed|Failed`); a record created directly as `Passed`
    /// is complete from birth. Every bound is enforced BEFORE the write —
    /// oversized criteria/checks/evidence JSON is rejected with a typed
    /// [`TaskError::Oversized`], never truncated.
    #[allow(clippy::too_many_arguments)]
    pub fn create_verification_record(
        &self,
        task_id: TaskId,
        tree_hash: Option<String>,
        criteria: Vec<CriterionVerification>,
        checks: Vec<CheckExecution>,
        changed_files: Vec<FileStateEvidence>,
        unrelated_changes: Vec<String>,
        reviewer: Option<serde_json::Value>,
        status: VerificationStatus,
        started_ms: i64,
    ) -> Result<VerificationRecordId, TaskError> {
        self.create_verification_record_with_evidence(
            task_id,
            tree_hash,
            criteria,
            checks,
            changed_files,
            unrelated_changes,
            reviewer,
            status,
            started_ms,
            None,
            None,
        )
    }

    /// Additive v20 twin of [`SessionHandle::create_verification_record`]
    /// (audits 94/116/117): the one write that lands the bounded environment
    /// fingerprint and the compact candidate-proof reference BESIDE the
    /// record in the same durable row. Legacy callers keep the old signature
    /// and write SQL `NULL` evidence — byte-identical to a pre-v20 record.
    /// The old contract holds unchanged (all bounds before ANY write, the
    /// task's current revision, exactly one row); both evidence payloads are
    /// validated here so oversized input is a typed
    /// [`TaskError::Oversized`] and malformed input a typed
    /// [`TaskError::Malformed`] — never a truncation.
    #[allow(clippy::too_many_arguments)]
    pub fn create_verification_record_with_evidence(
        &self,
        task_id: TaskId,
        tree_hash: Option<String>,
        criteria: Vec<CriterionVerification>,
        checks: Vec<CheckExecution>,
        changed_files: Vec<FileStateEvidence>,
        unrelated_changes: Vec<String>,
        reviewer: Option<serde_json::Value>,
        status: VerificationStatus,
        started_ms: i64,
        environment_fingerprint: Option<EnvironmentFingerprint>,
        candidate_proof_ref: Option<CandidateProofRef>,
    ) -> Result<VerificationRecordId, TaskError> {
        if task_id.raw() == 0 {
            return Err(TaskError::Malformed("task_id must be non-zero".into()));
        }
        validate_verification_record(
            &criteria,
            &checks,
            &changed_files,
            &unrelated_changes,
            reviewer.as_ref(),
            tree_hash.as_deref(),
            environment_fingerprint.as_ref(),
            candidate_proof_ref.as_ref(),
        )?;
        let fingerprint_json = match &environment_fingerprint {
            Some(fp) => Some(
                serde_json::to_string(fp)
                    .map_err(|e| TaskError::Malformed(format!("fingerprint json: {e}")))?,
            ),
            None => None,
        };
        let candidate_json = match &candidate_proof_ref {
            Some(c) => Some(
                serde_json::to_string(c)
                    .map_err(|e| TaskError::Malformed(format!("candidate ref json: {e}")))?,
            ),
            None => None,
        };
        let _guard = self.command_guard();
        let store = self.manager.store();
        let task = store
            .get_task(self.id, task_id)?
            .ok_or(TaskError::NotFound(task_id))?;
        // A candidate reference must certify the SAME revision the record
        // certifies: a race that moved the row between the caller's read and
        // this write refuses loudly (the caller re-reads and retries), never
        // persists a reference to a different candidate.
        if let Some(cref) = &candidate_proof_ref {
            if cref.task_revision != task.revision {
                return Err(TaskError::Malformed(format!(
                    "candidate-proof reference certifies task revision {} but the record certifies {}",
                    cref.task_revision, task.revision
                )));
            }
        }
        let session = store
            .get_session(self.id)?
            .ok_or(TaskError::NotFound(task_id))?;
        let rec = faktor_store::VerificationRecordRow {
            id: VerificationRecordId::new(1), // ignored by put; a fresh id is minted
            task_id,
            revision: task.revision,
            workspace_id: session.workspace_id,
            worktree_id: session.worktree_id,
            tree_hash,
            criteria,
            checks,
            changed_files,
            unrelated_changes,
            reviewer,
            status,
            started_ms,
            completed_ms: None,
        };
        Ok(store.verification_record_put_with_evidence(
            &rec,
            fingerprint_json.as_deref(),
            candidate_json.as_deref(),
        )?)
    }

    /// Create one verification record whose environment fingerprint EMBEDS
    /// the supplied proof basis (schema v20 twin, additive): the row's
    /// `proof_basis_digest` is `basis.digest()`, which includes the basis'
    /// bound layered effective-configuration digest — so any layer change
    /// alters the recorded digest and the record stops being reusable
    /// ([`SessionHandle::verification_record_reusable`]). Fail closed: a
    /// basis without a configuration binding is refused
    /// ([`ProofBasis::require_config_digest`]), never silently written.
    #[allow(clippy::too_many_arguments)]
    pub fn create_verification_record_bound_to_basis(
        &self,
        task_id: TaskId,
        tree_hash: Option<String>,
        criteria: Vec<CriterionVerification>,
        checks: Vec<CheckExecution>,
        changed_files: Vec<FileStateEvidence>,
        unrelated_changes: Vec<String>,
        reviewer: Option<serde_json::Value>,
        status: VerificationStatus,
        started_ms: i64,
        basis: &ProofBasis,
        candidate_proof_ref: Option<CandidateProofRef>,
    ) -> Result<VerificationRecordId, TaskError> {
        // Fail closed: the proof must name the configuration it verified
        // under; an unbound basis can never produce a configuration-
        // attributable record.
        basis.require_config_digest()?;
        // Fail closed on legacy identities: a basis carrying a 64-bit FNV
        // member can be viewed but can never mint a new record — the typed
        // refusal forces a restage/reverification under canonical BLAKE3.
        if let Some(legacy) = basis.legacy_authority_digest() {
            return Err(TaskError::LegacyAuthorityDigest {
                field: legacy.what.to_string(),
                digest: legacy.value,
            });
        }
        let fingerprint = EnvironmentFingerprint {
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            toolchain_versions: Vec::new(),
            manifest_hashes: Vec::new(),
            lockfile_hashes: Vec::new(),
            instruction_epoch: basis.instruction_epoch,
            // The verified tree is the record's own `tree_hash`; the
            // fingerprint's base-tree field stays an honest absence (the
            // session layer cannot re-derive a hex tree digest here).
            base_tree_hash: None,
            task_contract_hash: basis.task_contract_digest.clone(),
            check_argv_cwd_env_hash: check_basis_digest(&checks),
            verification_impl_version: basis.verification_impl_version.clone(),
            proof_basis_digest: Some(basis.digest()),
        };
        self.create_verification_record_with_evidence(
            task_id,
            tree_hash,
            criteria,
            checks,
            changed_files,
            unrelated_changes,
            reviewer,
            status,
            started_ms,
            Some(fingerprint),
            candidate_proof_ref,
        )
    }

    /// One verification record by id, or `None`.
    pub fn get_verification_record(
        &self,
        record_id: VerificationRecordId,
    ) -> Result<Option<VerificationRecord>, TaskError> {
        match self
            .manager
            .store()
            .verification_record_get_with_evidence(record_id)?
        {
            Some((row, fingerprint_json, candidate_json)) => Ok(Some(
                verification_record_from_row_with_evidence(row, fingerprint_json, candidate_json)?,
            )),
            None => Ok(None),
        }
    }

    /// Every verification record of `task_id`, in deterministic creation
    /// order (record id ascending).
    ///
    /// NOTE: `verification_record` rows are keyed by the NUMERIC task id
    /// (the approved schema carries no session column), so records of a
    /// standalone session (task id 1) of another session in the same
    /// workspace are visible here. Completion itself is protected by the
    /// record's workspace/worktree content check against the completing
    /// session's base worktree.
    pub fn list_verification_records(
        &self,
        task_id: TaskId,
    ) -> Result<Vec<VerificationRecord>, TaskError> {
        self.manager
            .store()
            .verification_record_list_by_task_with_evidence(task_id)?
            .into_iter()
            .map(|(row, fingerprint_json, candidate_json)| {
                verification_record_from_row_with_evidence(row, fingerprint_json, candidate_json)
            })
            .collect()
    }

    /// Whether one immutable proof record may be REUSED under the CURRENT
    /// proof basis (P0 proof binding): only an identical basis is reusable.
    /// A legacy record without a recorded basis is never reusable (an absent
    /// basis is an honest unknown, not a license).
    pub fn verification_record_reusable(
        &self,
        record_id: VerificationRecordId,
        basis: &ProofBasis,
    ) -> Result<ProofReuse, TaskError> {
        // Fail closed on legacy identities on BOTH sides: a current basis or
        // a recorded fingerprint carrying a 64-bit FNV digest may be viewed
        // but never authorizes a reuse — the typed error forces a
        // restage/reverification under the canonical BLAKE3 authority
        // digests.
        if let Some(legacy) = basis.legacy_authority_digest() {
            return Err(TaskError::LegacyAuthorityDigest {
                field: legacy.what.to_string(),
                digest: legacy.value,
            });
        }
        let Some(record) = self.get_verification_record(record_id)? else {
            return Ok(ProofReuse::Refused {
                reason: format!("record {record_id} does not exist"),
            });
        };
        if let Some(fingerprint) = record.environment_fingerprint.as_ref() {
            for (field, value) in [
                ("task_contract_hash", &fingerprint.task_contract_hash),
                (
                    "check_argv_cwd_env_hash",
                    &fingerprint.check_argv_cwd_env_hash,
                ),
            ] {
                if classify_authority_digest(value) == AuthorityDigestKind::LegacyFnv {
                    return Err(TaskError::LegacyAuthorityDigest {
                        field: format!("verification record {record_id} {field}"),
                        digest: value.clone(),
                    });
                }
            }
        }
        let Some(recorded) = record
            .environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref())
        else {
            return Ok(ProofReuse::Refused {
                reason: format!("record {record_id} carries no proof basis; it is never reusable"),
            });
        };
        let current = basis.digest();
        if recorded == current {
            Ok(ProofReuse::Allowed)
        } else {
            Ok(ProofReuse::Refused {
                reason: format!(
                    "record {record_id} is bound to proof basis {recorded}, but the current basis is {current}; reuse requires an identical basis"
                ),
            })
        }
    }

    /// Validate one completion proof as the immutable completion basis of
    /// `task_id` at its CURRENT revision, and — when the record carries a
    /// final tree hash and a candidate root is supplied — require the
    /// candidate root to digest to the SAME tree hash. Read-only: the
    /// atomic completion gate in [`SessionHandle::complete_verified_task`]
    /// remains the only completion authority.
    pub fn verify_completion_proof_binding(
        &self,
        task_id: TaskId,
        proof: VerificationRecordId,
        candidate_root: Option<&std::path::Path>,
    ) -> Result<CompletionProofBinding, TaskError> {
        let revision = self.task_revision(task_id)?;
        self.validate_completion_proof(task_id, revision, proof)?;
        let record = self
            .get_verification_record(proof)?
            .ok_or(TaskError::RecordNotFound(proof))?;
        if let (Some(recorded), Some(root)) = (record.tree_hash.as_deref(), candidate_root) {
            let current = current_manifest_digest(root)?;
            if current != recorded {
                return Err(TaskError::IntegrationSnapshotMismatch {
                    task_id,
                    record: proof,
                    recorded: recorded.to_string(),
                    current,
                });
            }
        }
        Ok(CompletionProofBinding {
            record_id: proof,
            task_id,
            revision,
            tree_hash: record.tree_hash,
            criterion_count: record.criteria.len(),
            check_count: record.checks.len(),
        })
    }

    /// The deterministic digest of the task's durable completion-accounting
    /// picture (audits 116/117): the counts and reserved-micro sums of every
    /// reservation still holding budget plus the durable settled spend,
    /// folded through the canonical BLAKE3 authority digest into
    /// `accounting:v1:blake3:{64-hex}` (the `accounting:v1:` envelope is
    /// preserved for prefix consumers; a legacy `accounting:v1:{16-hex}` FNV
    /// value classifies as legacy and never authorizes a completion
    /// binding). The candidate-proof reference records this digest at record
    /// build; the completion transaction reconciles and conservatively
    /// finalizes every reservation before it asserts the balance zero — so
    /// an already-settled task recomputes the SAME digest before and after
    /// completion.
    pub fn accounting_snapshot_digest(&self, task_id: TaskId) -> Result<String, TaskError> {
        let ledger = crate::budget::DurableBudgetLedger::new(self.manager.clone());
        let balance = ledger
            .completion_accounting_balance(self.id, task_id)
            .map_err(|e| TaskError::AccountingFailure {
                task_id,
                detail: e.to_string(),
            })?;
        Ok(accounting_digest_of(&balance))
    }

    /// The record's single allowed status write (audit P0-8): a CAS from
    /// `Running` to `Passed` or `Failed`, written exactly once. A second
    /// completion attempt on an already-final record is a typed
    /// [`TaskError::RecordNotFinalizable`] carrying the current status.
    pub fn finalize_verification_record(
        &self,
        record_id: VerificationRecordId,
        status: VerificationStatus,
        completed_ms: i64,
    ) -> Result<(), TaskError> {
        if !matches!(
            status,
            VerificationStatus::Passed | VerificationStatus::Failed
        ) {
            return Err(TaskError::Malformed(format!(
                "finalize status must be Passed or Failed, got {status:?}"
            )));
        }
        let _guard = self.command_guard();
        match self
            .manager
            .store()
            .verification_record_finalize(record_id, status, completed_ms)?
        {
            Ok(()) => Ok(()),
            Err(faktor_store::RecordFinalizeRefusal::Missing { .. }) => {
                Err(TaskError::RecordNotFound(record_id))
            }
            Err(faktor_store::RecordFinalizeRefusal::NotRunning { record_id, current }) => {
                Err(TaskError::RecordNotFinalizable {
                    record: record_id,
                    current,
                })
            }
        }
    }
}

/// Crash seams of the completion sequence (adversarial fault tests): each
/// seam sits between two durable steps of
/// [`SessionHandle::complete_verified_task`]. A crash AT a seam means every
/// step before it committed and nothing after it ran — the store reopens
/// with exactly that prefix, and a re-run of the full completion converges
/// (every monetary step is idempotent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionCrashPoint {
    /// After proof load + validation, before the accounting pass.
    AfterProofLoad,
    /// After the exact-usage reconcile, before the conservative finalize.
    AfterReconcile,
    /// After the conservative cost fold, before the ZERO-open assertion.
    AfterCostFold,
    /// After the ZERO-open assertion, before the transition CAS.
    BeforeTransition,
}

fn validate_task_fields(t: &Task) -> Result<(), TaskError> {
    if t.goal.len() > MAX_TASK_GOAL_BYTES {
        return Err(TaskError::Oversized(format!(
            "task goal of {} bytes exceeds MAX_TASK_GOAL_BYTES ({MAX_TASK_GOAL_BYTES})",
            t.goal.len()
        )));
    }
    if t.acceptance_criteria.len() > MAX_TASK_CRITERIA {
        return Err(TaskError::Oversized(format!(
            "{} acceptance criteria exceed MAX_TASK_CRITERIA ({MAX_TASK_CRITERIA})",
            t.acceptance_criteria.len()
        )));
    }
    let mut typed_ids = std::collections::HashSet::new();
    for c in &t.acceptance_criteria {
        if c.len() > MAX_TASK_CRITERION_BYTES {
            return Err(TaskError::Oversized(format!(
                "a criterion of {} bytes exceeds MAX_TASK_CRITERION_BYTES ({MAX_TASK_CRITERION_BYTES})",
                c.len()
            )));
        }
        // A V2 typed criterion is validated structurally (bounds +
        // deterministic content id + uniqueness); a legacy plain-text entry
        // keeps the historical per-entry bound only.
        if let Some(typed) = Criterion::decode(c) {
            typed.validate()?;
            if !typed_ids.insert(typed.id) {
                return Err(TaskError::Malformed(format!(
                    "duplicate criterion id {} in the acceptance criteria row",
                    typed.id
                )));
            }
        }
    }
    if t.plan.len() > MAX_TASK_PLAN_STEPS {
        return Err(TaskError::Oversized(format!(
            "{} plan steps exceed MAX_TASK_PLAN_STEPS ({MAX_TASK_PLAN_STEPS})",
            t.plan.len()
        )));
    }
    for s in &t.plan {
        if s.len() > MAX_TASK_STEP_BYTES {
            return Err(TaskError::Oversized(format!(
                "a plan step of {} bytes exceeds MAX_TASK_STEP_BYTES ({MAX_TASK_STEP_BYTES})",
                s.len()
            )));
        }
    }
    // Attachments are typed bytes references, SEPARATE from the plan/files
    // vocabulary: bounded count, canonical mime, traversal-free filename and
    // the payload ceiling — validated before ANY durable write.
    if t.attachments.len() > MAX_ATTACHMENTS_PER_TASK {
        return Err(TaskError::Oversized(format!(
            "{} attachments exceed MAX_ATTACHMENTS_PER_TASK ({MAX_ATTACHMENTS_PER_TASK})",
            t.attachments.len()
        )));
    }
    for a in &t.attachments {
        a.validate().map_err(task_error_from_core)?;
    }
    Ok(())
}

/// Bounded-field contract of one verification record (audit P0-8): every
/// bound is enforced before ANY write; oversized input is rejected with a
/// typed error, never truncated.
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn validate_verification_record(
    criteria: &[CriterionVerification],
    checks: &[CheckExecution],
    changed_files: &[FileStateEvidence],
    unrelated_changes: &[String],
    reviewer: Option<&serde_json::Value>,
    tree_hash: Option<&str>,
    environment_fingerprint: Option<&EnvironmentFingerprint>,
    candidate_proof_ref: Option<&CandidateProofRef>,
) -> Result<(), TaskError> {
    let reject = |what: &str| TaskError::Oversized(what.to_string());
    let malformed = |what: String| TaskError::Malformed(what);
    if criteria.len() > MAX_VERIFICATION_RECORD_CRITERIA {
        return Err(reject(&format!(
            "{} criterion verdicts exceed MAX_VERIFICATION_RECORD_CRITERIA ({MAX_VERIFICATION_RECORD_CRITERIA})",
            criteria.len()
        )));
    }
    for c in criteria {
        if c.criterion_key.is_empty() {
            return Err(malformed(
                "a criterion verdict has an empty criterion_key".into(),
            ));
        }
        if c.criterion_key.len() > MAX_VERIFICATION_CRITERION_KEY_BYTES {
            return Err(reject(&format!(
                "a criterion key of {} bytes exceeds MAX_VERIFICATION_CRITERION_KEY_BYTES ({MAX_VERIFICATION_CRITERION_KEY_BYTES})",
                c.criterion_key.len()
            )));
        }
        if let Some(evidence) = &c.evidence {
            if evidence.len() > MAX_VERIFICATION_EVIDENCE_BYTES {
                return Err(reject(&format!(
                    "criterion evidence of {} bytes exceeds MAX_VERIFICATION_EVIDENCE_BYTES ({MAX_VERIFICATION_EVIDENCE_BYTES})",
                    evidence.len()
                )));
            }
        }
    }
    let criteria_json =
        serde_json::to_vec(criteria).map_err(|e| malformed(format!("criteria json: {e}")))?;
    if criteria_json.len() > MAX_VERIFICATION_CRITERIA_JSON_BYTES {
        return Err(reject(&format!(
            "criteria JSON of {} bytes exceeds MAX_VERIFICATION_CRITERIA_JSON_BYTES ({MAX_VERIFICATION_CRITERIA_JSON_BYTES})",
            criteria_json.len()
        )));
    }
    if checks.len() > MAX_VERIFICATION_RECORD_CHECKS {
        return Err(reject(&format!(
            "{} checks exceed MAX_VERIFICATION_RECORD_CHECKS ({MAX_VERIFICATION_RECORD_CHECKS})",
            checks.len()
        )));
    }
    for ch in checks {
        if ch.check.is_empty() || ch.check.len() > MAX_VERIFICATION_CHECK_NAME_BYTES {
            return Err(reject(&format!(
                "check name {} exceeds MAX_VERIFICATION_CHECK_NAME_BYTES ({MAX_VERIFICATION_CHECK_NAME_BYTES})",
                ch.check.len()
            )));
        }
        if ch.program.is_empty() || ch.program.len() > MAX_VERIFICATION_PROGRAM_BYTES {
            return Err(reject(&format!(
                "check program {} exceeds MAX_VERIFICATION_PROGRAM_BYTES ({MAX_VERIFICATION_PROGRAM_BYTES})",
                ch.program.len()
            )));
        }
        if ch.args.len() > MAX_VERIFICATION_CHECK_ARGS {
            return Err(reject(&format!(
                "{} check args exceed MAX_VERIFICATION_CHECK_ARGS ({MAX_VERIFICATION_CHECK_ARGS})",
                ch.args.len()
            )));
        }
        for a in &ch.args {
            if a.len() > MAX_VERIFICATION_CHECK_ARG_BYTES {
                return Err(reject(&format!(
                    "a check arg of {} bytes exceeds MAX_VERIFICATION_CHECK_ARG_BYTES ({MAX_VERIFICATION_CHECK_ARG_BYTES})",
                    a.len()
                )));
            }
        }
        if ch.category.len() > MAX_VERIFICATION_CATEGORY_BYTES {
            return Err(reject(&format!(
                "check category {} exceeds MAX_VERIFICATION_CATEGORY_BYTES ({MAX_VERIFICATION_CATEGORY_BYTES})",
                ch.category.len()
            )));
        }
        if let Some(summary) = &ch.summary {
            if summary.len() > MAX_VERIFICATION_SUMMARY_BYTES {
                return Err(reject(&format!(
                    "check summary of {} bytes exceeds MAX_VERIFICATION_SUMMARY_BYTES ({MAX_VERIFICATION_SUMMARY_BYTES})",
                    summary.len()
                )));
            }
        }
    }
    let checks_json =
        serde_json::to_vec(checks).map_err(|e| malformed(format!("checks json: {e}")))?;
    if checks_json.len() > MAX_VERIFICATION_CHECKS_JSON_BYTES {
        return Err(reject(&format!(
            "checks JSON of {} bytes exceeds MAX_VERIFICATION_CHECKS_JSON_BYTES ({MAX_VERIFICATION_CHECKS_JSON_BYTES})",
            checks_json.len()
        )));
    }
    if changed_files.len() > MAX_VERIFICATION_CHANGED_FILES {
        return Err(reject(&format!(
            "{} changed files exceed MAX_VERIFICATION_CHANGED_FILES ({MAX_VERIFICATION_CHANGED_FILES})",
            changed_files.len()
        )));
    }
    for f in changed_files {
        if f.path.is_empty() || f.path.len() > MAX_VERIFICATION_PATH_BYTES {
            return Err(reject(&format!(
                "file path of {} bytes exceeds MAX_VERIFICATION_PATH_BYTES ({MAX_VERIFICATION_PATH_BYTES})",
                f.path.len()
            )));
        }
        if f.digest_hex.is_empty()
            || f.digest_hex.len() > MAX_VERIFICATION_DIGEST_BYTES
            || !f.digest_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(malformed(format!(
                "file {} digest {:?} must be non-empty lowercase/uppercase hex of at most {MAX_VERIFICATION_DIGEST_BYTES} chars",
                f.path, f.digest_hex
            )));
        }
    }
    let files_json = serde_json::to_vec(changed_files)
        .map_err(|e| malformed(format!("changed_files json: {e}")))?;
    if files_json.len() > MAX_VERIFICATION_CHANGED_FILES_JSON_BYTES {
        return Err(reject(&format!(
            "changed_files JSON of {} bytes exceeds MAX_VERIFICATION_CHANGED_FILES_JSON_BYTES ({MAX_VERIFICATION_CHANGED_FILES_JSON_BYTES})",
            files_json.len()
        )));
    }
    if unrelated_changes.len() > MAX_VERIFICATION_UNRELATED_CHANGES {
        return Err(reject(&format!(
            "{} unrelated changes exceed MAX_VERIFICATION_UNRELATED_CHANGES ({MAX_VERIFICATION_UNRELATED_CHANGES})",
            unrelated_changes.len()
        )));
    }
    for u in unrelated_changes {
        if u.is_empty() || u.len() > MAX_VERIFICATION_PATH_BYTES {
            return Err(reject(&format!(
                "an unrelated-change path of {} bytes exceeds MAX_VERIFICATION_PATH_BYTES ({MAX_VERIFICATION_PATH_BYTES})",
                u.len()
            )));
        }
    }
    let unrelated_json = serde_json::to_vec(unrelated_changes)
        .map_err(|e| malformed(format!("unrelated_changes json: {e}")))?;
    if unrelated_json.len() > MAX_VERIFICATION_UNRELATED_JSON_BYTES {
        return Err(reject(&format!(
            "unrelated_changes JSON of {} bytes exceeds MAX_VERIFICATION_UNRELATED_JSON_BYTES ({MAX_VERIFICATION_UNRELATED_JSON_BYTES})",
            unrelated_json.len()
        )));
    }
    if let Some(reviewer) = reviewer {
        let len = serde_json::to_vec(reviewer)
            .map_err(|e| malformed(format!("reviewer json: {e}")))?
            .len();
        if len > MAX_VERIFICATION_REVIEWER_JSON_BYTES {
            return Err(reject(&format!(
                "reviewer JSON of {len} bytes exceeds MAX_VERIFICATION_REVIEWER_JSON_BYTES ({MAX_VERIFICATION_REVIEWER_JSON_BYTES})"
            )));
        }
    }
    if let Some(hash) = tree_hash {
        // Two shapes stay WRITABLE: the versioned canonical tree-manifest
        // digest (`tm1:` + 64 hex) and the legacy 64-hex content-only digest
        // (accepted so records written before the canonical manifest still
        // decode). The two shapes never compare equal — a legacy record
        // fails the completion binding loudly instead of silently matching.
        let legacy = !hash.is_empty()
            && hash.len() <= MAX_VERIFICATION_TREE_HASH_BYTES
            && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if !legacy && !faktor_fs::tree_manifest::is_tree_manifest_digest(hash) {
            return Err(malformed(format!(
                "tree_hash {:?} must be a canonical `tm1:<64-hex>` tree-manifest digest or a legacy hex digest of at most {MAX_VERIFICATION_TREE_HASH_BYTES} chars",
                hash
            )));
        }
    }
    // Schema v20 evidence (audits 94/116/117): the fingerprint and the
    // candidate reference are bounded fields AND bounded serialized
    // payloads. Both checks run before ANY write; the core types own the
    // per-field contract and this layer owns the durable column bound.
    if let Some(fp) = environment_fingerprint {
        evidence_violations("environment fingerprint".to_string(), fp.validate())?;
        let len = fp
            .json_size()
            .map_err(|e| malformed(format!("environment fingerprint json: {e}")))?;
        if len > MAX_VERIFICATION_FINGERPRINT_JSON_BYTES {
            return Err(reject(&format!(
                "environment fingerprint JSON of {len} bytes exceeds MAX_VERIFICATION_FINGERPRINT_JSON_BYTES ({MAX_VERIFICATION_FINGERPRINT_JSON_BYTES})"
            )));
        }
    }
    if let Some(cref) = candidate_proof_ref {
        evidence_violations("candidate-proof reference".to_string(), cref.validate())?;
        let len = cref
            .json_size()
            .map_err(|e| malformed(format!("candidate-proof reference json: {e}")))?;
        if len > MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES {
            return Err(reject(&format!(
                "candidate-proof reference JSON of {len} bytes exceeds MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES ({MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES})"
            )));
        }
    }
    Ok(())
}

/// Fold core fingerprint/candidate violations into ONE typed task error:
/// any oversized violation makes the whole payload oversized; otherwise it
/// is malformed. Used identically on write (pre-write validation) and on
/// read (a corrupt stored row is loudly rejected, never dropped).
fn evidence_violations(
    what: String,
    result: Result<(), Vec<faktor_core::state::FingerprintViolation>>,
) -> Result<(), TaskError> {
    match result {
        Ok(()) => Ok(()),
        Err(violations) => {
            let oversized = violations.iter().any(|v| v.oversized);
            let detail = violations
                .iter()
                .map(|v| format!("{}: {}", v.field, v.detail))
                .collect::<Vec<_>>()
                .join("; ");
            if oversized {
                Err(TaskError::Oversized(format!("{what}: {detail}")))
            } else {
                Err(TaskError::Malformed(format!("{what}: {detail}")))
            }
        }
    }
}

/// Build the session view of one store row together with its raw v20
/// evidence columns. A non-NULL column must decode to the exact typed shape
/// and pass its own bounds — anything else is a loud typed error (a hostile
/// or corrupt injected row can never read as a different record).
pub(crate) fn verification_record_from_row_with_evidence(
    row: faktor_store::VerificationRecordRow,
    environment_fingerprint_json: Option<String>,
    candidate_proof_ref_json: Option<String>,
) -> Result<VerificationRecord, TaskError> {
    let record_id = row.id;
    let environment_fingerprint =
        parse_environment_fingerprint(record_id, environment_fingerprint_json)?;
    let candidate_proof_ref = parse_candidate_proof_ref(record_id, candidate_proof_ref_json)?;
    let mut record = VerificationRecord::from(row);
    record.environment_fingerprint = environment_fingerprint;
    record.candidate_proof_ref = candidate_proof_ref;
    Ok(record)
}

fn parse_environment_fingerprint(
    record_id: VerificationRecordId,
    raw: Option<String>,
) -> Result<Option<EnvironmentFingerprint>, TaskError> {
    let Some(raw) = raw else {
        return Ok(None); // pre-v20 row: honest absence
    };
    if raw.len() > MAX_VERIFICATION_FINGERPRINT_JSON_BYTES {
        return Err(TaskError::Oversized(format!(
            "verification record {record_id} environment fingerprint column of {} bytes exceeds MAX_VERIFICATION_FINGERPRINT_JSON_BYTES ({MAX_VERIFICATION_FINGERPRINT_JSON_BYTES})",
            raw.len()
        )));
    }
    let fingerprint: EnvironmentFingerprint = serde_json::from_str(&raw).map_err(|e| {
        TaskError::Malformed(format!(
            "verification record {record_id} environment fingerprint is corrupt: {e}"
        ))
    })?;
    evidence_violations(
        format!("verification record {record_id} environment fingerprint"),
        fingerprint.validate(),
    )?;
    Ok(Some(fingerprint))
}

fn parse_candidate_proof_ref(
    record_id: VerificationRecordId,
    raw: Option<String>,
) -> Result<Option<CandidateProofRef>, TaskError> {
    let Some(raw) = raw else {
        return Ok(None); // pre-v20 row: honest absence
    };
    if raw.len() > MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES {
        return Err(TaskError::Oversized(format!(
            "verification record {record_id} candidate-proof reference column of {} bytes exceeds MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES ({MAX_VERIFICATION_CANDIDATE_REF_JSON_BYTES})",
            raw.len()
        )));
    }
    let reference: CandidateProofRef = serde_json::from_str(&raw).map_err(|e| {
        TaskError::Malformed(format!(
            "verification record {record_id} candidate-proof reference is corrupt: {e}"
        ))
    })?;
    evidence_violations(
        format!("verification record {record_id} candidate-proof reference"),
        reference.validate(),
    )?;
    Ok(Some(reference))
}

/// The canonical BLAKE3 authority digest of one completion-accounting
/// balance, over the fixed field order (counts and micro-amounts as
/// length-prefixed little-endian fields). The `accounting:v1:` envelope is
/// preserved so pre-existing prefix consumers keep working; `v1:blake3:`
/// marks the BLAKE3 body — a legacy `accounting:v1:<16-hex>` value still
/// classifies as legacy FNV and never authorizes a completion binding.
fn accounting_digest_of(balance: &crate::budget::TaskCompletionBalance) -> String {
    let fields = Fields::new()
        .uint(balance.open_count as u64)
        .uint(balance.open_micro)
        .uint(balance.dispatched_count as u64)
        .uint(balance.uncertain_count as u64)
        .uint(balance.uncertain_micro)
        .uint(balance.spent_cost_micro);
    format!(
        "accounting:v1:{}",
        authority_digest_labeled(DOMAIN_ACCOUNTING_BALANCE, 1, fields)
    )
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
