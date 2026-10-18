//! Durable instructions, scoped rules and prompt templates (Rev8 S11/S69).
//!
//! One SQLite authority for the user-editable instruction text the daemon
//! composes into the next request. The store records what the operator wrote
//! (content, origin, activation, version) and never pretends that project
//! text is executable authority: managed policy is carried separately by the
//! permission authority and a managed row written by policy is locked.
//!
//! * Every record is versioned; an update appends a new version and keeps the
//!   content hash.
//! * Preview is a pure read: activation is evaluated against the exact
//!   prompt and touched paths, and the merged bundle reports the source,
//!   token ESTIMATE (labelled with its method) and the conflicts it resolved.
//! * A captured bundle is immutable: the same operation id with different
//!   content is a typed conflict, never a silent rewrite of running work.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::Scope;
use crate::db::now_ms;
use crate::{hash_hex, page_limit_i64, FoundationDb, FoundationError};

pub const MAX_INSTRUCTION_BYTES: usize = 256 * 1024;
pub const MAX_INSTRUCTION_RECORDS: usize = 2_000;
pub const MAX_INSTRUCTION_TITLE_BYTES: usize = 128;
pub const MAX_GLOBS_PER_RECORD: usize = 32;
pub const MAX_GLOB_BYTES: usize = 512;
pub const MAX_BUNDLE_SOURCES: usize = 2_000;
pub const MAX_BINDING_PAGE: usize = 200;
pub const INSTRUCTION_IMPORT_FORMAT: &str = "faktor-instructions/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionKind {
    Instruction,
    Rule,
    Template,
    OutputStyle,
}

impl InstructionKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "instruction" => Some(Self::Instruction),
            "rule" => Some(Self::Rule),
            "template" => Some(Self::Template),
            "output_style" => Some(Self::OutputStyle),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instruction => "instruction",
            Self::Rule => "rule",
            Self::Template => "template",
            Self::OutputStyle => "output_style",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionOrigin {
    /// Enforced organization text; only the managed policy sync writes it.
    Managed,
    Project,
    User,
    Imported,
}

impl InstructionOrigin {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "managed" => Some(Self::Managed),
            "project" => Some(Self::Project),
            "user" => Some(Self::User),
            "imported" => Some(Self::Imported),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Project => "project",
            Self::User => "user",
            Self::Imported => "imported",
        }
    }

    /// Lower wins. Managed constraints always outrank user/project prose.
    pub fn rank(self) -> u8 {
        match self {
            Self::Managed => 0,
            Self::Project => 1,
            Self::User => 2,
            Self::Imported => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    Always,
    Manual,
    Glob,
}

impl Activation {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "always" => Some(Self::Always),
            "manual" => Some(Self::Manual),
            "glob" => Some(Self::Glob),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Manual => "manual",
            Self::Glob => "glob",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionRecord {
    pub id: String,
    pub kind: InstructionKind,
    pub title: String,
    pub content: String,
    pub origin: InstructionOrigin,
    pub scope: Scope,
    pub activation: Activation,
    pub globs: Vec<String>,
    pub enabled: bool,
    pub locked: bool,
    pub version: u64,
    pub content_hash: String,
    pub token_estimate: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionDraft {
    #[serde(default)]
    pub id: Option<String>,
    pub kind: InstructionKind,
    pub title: String,
    pub content: String,
    #[serde(default = "default_origin")]
    pub origin: InstructionOrigin,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default = "default_activation")]
    pub activation: Activation,
    #[serde(default)]
    pub globs: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_origin() -> InstructionOrigin {
    InstructionOrigin::User
}
fn default_scope() -> Scope {
    Scope::User
}
fn default_activation() -> Activation {
    Activation::Always
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleSource {
    pub record_id: String,
    pub title: String,
    pub kind: InstructionKind,
    pub origin: InstructionOrigin,
    pub scope: Scope,
    pub activation: Activation,
    pub version: u64,
    pub content_hash: String,
    pub token_estimate: u64,
    pub active: bool,
    /// Why an inactive record was not included.
    pub inactive_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleConflict {
    pub title: String,
    pub winner: String,
    pub shadowed: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenEstimate {
    pub tokens: u64,
    pub method: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionBundle {
    pub bundle_hash: String,
    pub content: String,
    pub sources: Vec<BundleSource>,
    pub conflicts: Vec<BundleConflict>,
    pub estimate: TokenEstimate,
    /// Managed constraints are NOT instruction text; they are reported so
    /// the preview never claims project text can override them.
    pub managed_policy_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionBinding {
    pub session_id: u64,
    pub revision: u64,
    pub operation_id: String,
    pub bundle_hash: String,
    pub content: String,
    pub sources: Vec<BundleSource>,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub format: String,
    pub imported: usize,
    pub unsupported: Vec<String>,
    pub errors: Vec<String>,
}

fn estimate_tokens(text: &str) -> u64 {
    // Bounded byte estimate, explicitly labelled. This is NOT a measured
    // provider tokenization; every wire shape says so.
    (text.len() as u64).div_ceil(4)
}

fn validate_content(kind: InstructionKind, content: &str) -> Result<(), FoundationError> {
    if content.is_empty() {
        return Err(FoundationError::Malformed(
            "instruction content is empty".into(),
        ));
    }
    if content.len() > MAX_INSTRUCTION_BYTES {
        return Err(FoundationError::Oversized(format!(
            "instruction content is {} bytes; the bound is {MAX_INSTRUCTION_BYTES}",
            content.len()
        )));
    }
    if kind == InstructionKind::Template && content.len() > MAX_INSTRUCTION_BYTES / 4 {
        return Err(FoundationError::Oversized(
            "a prompt template is bounded to 64 KiB".into(),
        ));
    }
    Ok(())
}

fn validate_draft(draft: &InstructionDraft) -> Result<(), FoundationError> {
    if draft.title.trim().is_empty() || draft.title.len() > MAX_INSTRUCTION_TITLE_BYTES {
        return Err(FoundationError::Malformed(format!(
            "instruction title must be 1..={MAX_INSTRUCTION_TITLE_BYTES} bytes"
        )));
    }
    if draft.title.chars().any(|c| c.is_control()) {
        return Err(FoundationError::Malformed(
            "instruction title must not contain control characters".into(),
        ));
    }
    if let Some(id) = &draft.id {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(FoundationError::Malformed(
                "instruction id must be 1..=128 bytes of ASCII [A-Za-z0-9._-]".into(),
            ));
        }
    }
    validate_content(draft.kind, &draft.content)?;
    if draft.globs.len() > MAX_GLOBS_PER_RECORD {
        return Err(FoundationError::Oversized(format!(
            "at most {MAX_GLOBS_PER_RECORD} globs per instruction"
        )));
    }
    for glob in &draft.globs {
        if glob.is_empty() || glob.len() > MAX_GLOB_BYTES || glob.contains('\u{0}') {
            return Err(FoundationError::Malformed(format!(
                "a glob must be 1..={MAX_GLOB_BYTES} bytes without NUL"
            )));
        }
    }
    if draft.activation == Activation::Glob && draft.globs.is_empty() {
        return Err(FoundationError::Malformed(
            "glob activation requires at least one glob".into(),
        ));
    }
    if !scope_allowed(draft.scope, draft.origin) {
        return Err(FoundationError::Malformed(format!(
            "origin {} cannot use scope {}",
            draft.origin.as_str(),
            draft.scope.as_str()
        )));
    }
    Ok(())
}

fn scope_allowed(scope: Scope, origin: InstructionOrigin) -> bool {
    match origin {
        InstructionOrigin::Managed => matches!(scope, Scope::Managed | Scope::Machine),
        InstructionOrigin::Project => matches!(scope, Scope::Project),
        InstructionOrigin::User => matches!(scope, Scope::User | Scope::Machine),
        InstructionOrigin::Imported => matches!(scope, Scope::User | Scope::Project),
    }
}

fn derive_id(draft: &InstructionDraft) -> String {
    if let Some(id) = &draft.id {
        return id.clone();
    }
    let seed = format!(
        "{}:{}:{}:{}",
        draft.kind.as_str(),
        draft.origin.as_str(),
        draft.scope.as_str(),
        draft.title.trim().to_ascii_lowercase()
    );
    format!("ins-{}", &hash_hex(seed.as_bytes())[..16])
}

fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstructionRecord> {
    let kind: String = row.get(1)?;
    let origin: String = row.get(4)?;
    let scope_raw: String = row.get(5)?;
    let activation: String = row.get(6)?;
    let globs_raw: String = row.get(7)?;
    let kind = InstructionKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown instruction kind",
            )),
        )
    })?;
    let origin = InstructionOrigin::parse(&origin).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown instruction origin",
            )),
        )
    })?;
    let scope = Scope::parse(&scope_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown instruction scope",
            )),
        )
    })?;
    let activation = Activation::parse(&activation).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown instruction activation",
            )),
        )
    })?;
    let globs: Vec<String> = serde_json::from_str(&globs_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    let content: String = row.get(3)?;
    Ok(InstructionRecord {
        id: row.get(0)?,
        kind,
        title: row.get(2)?,
        token_estimate: estimate_tokens(&content),
        content,
        origin,
        scope,
        activation,
        globs,
        enabled: row.get::<_, i64>(8)? != 0,
        locked: row.get::<_, i64>(9)? != 0,
        version: u64::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
        content_hash: row.get(11)?,
        created_ms: row.get(12)?,
        updated_ms: row.get(13)?,
        revision: u64::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
    })
}

const COLUMNS: &str = "id, kind, title, content, origin, scope, activation, globs_json, enabled, locked, version, content_hash, created_ms, updated_ms, revision";

pub struct InstructionService {
    db: FoundationDb,
}

impl InstructionService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn list(&self) -> Result<Vec<InstructionRecord>, FoundationError> {
        self.db.read("instruction list", |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM instruction_record ORDER BY origin ASC, title ASC, id ASC LIMIT ?1"
                ))
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_INSTRUCTION_RECORDS as i64], map_record)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<InstructionRecord>, FoundationError> {
        self.db.read("instruction get", |conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM instruction_record WHERE id = ?1"),
                params![id],
                map_record,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Create or update one record. `expected_revision` guards concurrent
    /// editors; a managed record is writable only when it is already managed
    /// (the managed sync owns that origin) and keeps its locked flag.
    pub fn upsert(
        &self,
        draft: &InstructionDraft,
        expected_revision: Option<u64>,
    ) -> Result<InstructionRecord, FoundationError> {
        validate_draft(draft)?;
        let id = derive_id(draft);
        let now = now_ms();
        self.db.write("instruction upsert", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin instruction upsert: {e}")))?;
            let existing: Option<(i64, i64, String, String)> = tx
                .query_row(
                    "SELECT revision, version, origin, content_hash FROM instruction_record WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let (revision, version, locked) = match &existing {
                Some((revision, version, origin, _)) => {
                    if draft.origin == InstructionOrigin::Managed
                        && origin != InstructionOrigin::Managed.as_str()
                    {
                        return Err(FoundationError::Conflict(
                            "managed instruction rows are owned by the managed policy sync".into(),
                        ));
                    }
                    if let Some(expected) = expected_revision {
                        if (*revision).max(0) as u64 != expected {
                            return Err(FoundationError::Conflict(format!(
                                "instruction {id} revision changed: expected {expected} but found {revision}"
                            )));
                        }
                    }
                    (
                        (*revision).max(0) as u64 + 1,
                        (*version).max(0) as u64 + 1,
                        draft.origin == InstructionOrigin::Managed,
                    )
                }
                None => {
                    if draft.origin == InstructionOrigin::Managed {
                        return Err(FoundationError::Conflict(
                            "managed instruction rows are created by the managed policy sync".into(),
                        ));
                    }
                    if let Some(expected) = expected_revision {
                        if expected != 0 {
                            return Err(FoundationError::Conflict(format!(
                                "instruction {id} does not exist at revision {expected}"
                            )));
                        }
                    }
                    let count: i64 = tx
                        .query_row("SELECT COUNT(*) FROM instruction_record", [], |r| r.get(0))
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                    if count as usize >= MAX_INSTRUCTION_RECORDS {
                        return Err(FoundationError::Oversized(format!(
                            "at most {MAX_INSTRUCTION_RECORDS} instruction records"
                        )));
                    }
                    (1, 1, false)
                }
            };
            let globs_json = serde_json::to_string(&draft.globs)
                .map_err(|e| FoundationError::Store(format!("encode globs: {e}")))?;
            let hash = hash_hex(draft.content.as_bytes());
            tx.execute(
                "INSERT INTO instruction_record(
                   id, kind, title, content, origin, scope, activation, globs_json,
                   enabled, locked, version, content_hash, created_ms, updated_ms, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(id) DO UPDATE SET
                   kind = excluded.kind,
                   title = excluded.title,
                   content = excluded.content,
                   origin = excluded.origin,
                   scope = excluded.scope,
                   activation = excluded.activation,
                   globs_json = excluded.globs_json,
                   enabled = excluded.enabled,
                   locked = excluded.locked,
                   version = excluded.version,
                   content_hash = excluded.content_hash,
                   updated_ms = excluded.updated_ms,
                   revision = excluded.revision",
                params![
                    id,
                    draft.kind.as_str(),
                    draft.title,
                    draft.content,
                    draft.origin.as_str(),
                    draft.scope.as_str(),
                    draft.activation.as_str(),
                    globs_json,
                    if draft.enabled { 1 } else { 0 },
                    if locked { 1 } else { 0 },
                    version as i64,
                    hash,
                    now,
                    now,
                    revision as i64
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit instruction upsert: {e}")))?;
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM instruction_record WHERE id = ?1"),
                params![id],
                map_record,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn delete(&self, id: &str, expected_revision: u64) -> Result<u64, FoundationError> {
        self.db.write("instruction delete", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin delete: {e}")))?;
            let row: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT revision, locked FROM instruction_record WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let Some((revision, locked)) = row else {
                return Err(FoundationError::NotFound(format!("instruction {id}")));
            };
            if locked != 0 {
                return Err(FoundationError::Conflict(format!(
                    "instruction {id} is locked by managed policy"
                )));
            }
            if revision.max(0) as u64 != expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "instruction {id} revision changed: expected {expected_revision} but found {revision}"
                )));
            }
            tx.execute("DELETE FROM instruction_record WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit delete: {e}")))?;
            Ok(revision.max(0) as u64 + 1)
        })
    }

    /// Evaluate activation and compose the bundle for exactly this input.
    /// Pure read; no record is modified and no authority is inferred.
    pub fn preview(
        &self,
        prompt: &str,
        touched_paths: &[String],
        selected_ids: &[String],
    ) -> Result<InstructionBundle, FoundationError> {
        let records = self.list()?;
        if records.len() > MAX_BUNDLE_SOURCES {
            return Err(FoundationError::Oversized(format!(
                "at most {MAX_BUNDLE_SOURCES} instruction sources"
            )));
        }
        let selected: BTreeSet<&str> = selected_ids.iter().map(String::as_str).collect();
        let mut active: Vec<(&InstructionRecord, bool, Option<String>)> = Vec::new();
        for record in &records {
            let (is_active, reason) = if !record.enabled {
                (false, Some("disabled".to_string()))
            } else {
                match record.activation {
                    Activation::Always => (true, None),
                    Activation::Manual => {
                        if selected.contains(record.id.as_str()) {
                            (true, None)
                        } else {
                            (false, Some("manual selection required".into()))
                        }
                    }
                    Activation::Glob => {
                        let hit = record.globs.iter().any(|glob| {
                            touched_paths
                                .iter()
                                .any(|path| glob_match(glob.as_bytes(), path.as_bytes()))
                                || glob_match(glob.as_bytes(), prompt.as_bytes())
                        });
                        if hit {
                            (true, None)
                        } else {
                            (false, Some("no path matched the glob".into()))
                        }
                    }
                }
            };
            active.push((record, is_active, reason));
        }

        // Precedence is deterministic: managed > project > user > imported,
        // then title, then id. Managed policy locks are reported separately.
        active.sort_by(|a, b| {
            a.0.origin
                .rank()
                .cmp(&b.0.origin.rank())
                .then_with(|| a.0.title.cmp(&b.0.title))
                .then_with(|| a.0.id.cmp(&b.0.id))
        });

        let mut by_title: BTreeMap<String, Vec<&InstructionRecord>> = BTreeMap::new();
        for (record, is_active, _) in &active {
            if *is_active {
                by_title
                    .entry(record.title.trim().to_ascii_lowercase())
                    .or_default()
                    .push(record);
            }
        }
        let mut conflicts = Vec::new();
        for (title, rows) in &by_title {
            if rows.len() > 1 {
                conflicts.push(BundleConflict {
                    title: title.clone(),
                    winner: rows[0].id.clone(),
                    shadowed: rows[1..].iter().map(|r| r.id.clone()).collect(),
                    reason: "scope precedence (managed > project > user > imported)".to_string(),
                });
            }
        }

        let mut content = String::new();
        let mut sources = Vec::new();
        let mut shadowed: BTreeSet<&str> = conflicts
            .iter()
            .flat_map(|c| c.shadowed.iter())
            .map(String::as_str)
            .collect();
        // Only the winner of an identical title enters the merged prose; a
        // shadowed row stays visible in sources with the reason, never
        // silently dropped.
        for (record, is_active, reason) in &active {
            let included = *is_active && !shadowed.remove(record.id.as_str());
            if included {
                if !content.is_empty() {
                    content.push_str("\n\n");
                }
                content.push_str("## ");
                content.push_str(&record.title);
                content.push('\n');
                content.push_str(&record.content);
            }
            sources.push(BundleSource {
                record_id: record.id.clone(),
                title: record.title.clone(),
                kind: record.kind,
                origin: record.origin,
                scope: record.scope,
                activation: record.activation,
                version: record.version,
                content_hash: record.content_hash.clone(),
                token_estimate: record.token_estimate,
                active: included,
                inactive_reason: if included {
                    None
                } else if *is_active {
                    Some("shadowed by higher-precedence instruction".into())
                } else {
                    reason.clone()
                },
            });
        }
        let hash = {
            let mut hasher_input = String::new();
            for source in &sources {
                if source.active {
                    hasher_input.push_str(&source.record_id);
                    hasher_input.push('\u{0}');
                    hasher_input.push_str(&source.version.to_string());
                    hasher_input.push('\u{0}');
                    hasher_input.push_str(&source.content_hash);
                    hasher_input.push('\n');
                }
            }
            hasher_input.push_str(&content);
            hash_hex(hasher_input.as_bytes())
        };
        Ok(InstructionBundle {
            bundle_hash: hash,
            content: content.clone(),
            sources,
            conflicts,
            estimate: TokenEstimate {
                tokens: estimate_tokens(&content),
                method: "byte-quarter-estimator".into(),
                version: "1".into(),
            },
            managed_policy_locked: records.iter().any(|r| r.locked),
        })
    }

    /// Capture the exact bundle for an operation. An identical replay is
    /// idempotent; a different body under the same operation id conflicts.
    pub fn bind(
        &self,
        session_id: u64,
        operation_id: &str,
        bundle: &InstructionBundle,
    ) -> Result<InstructionBinding, FoundationError> {
        if operation_id.is_empty() || operation_id.len() > 256 {
            return Err(FoundationError::Malformed(
                "operation_id must be 1..=256 bytes".into(),
            ));
        }
        let records_json = serde_json::to_string(&bundle.sources)
            .map_err(|e| FoundationError::Store(format!("encode sources: {e}")))?;
        let now = now_ms();
        self.db.write("instruction bind", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin bind: {e}")))?;
            if let Some((revision, hash)) = tx
                .query_row(
                    "SELECT revision, bundle_hash FROM instruction_bundle
                     WHERE session_id = ?1 AND operation_id = ?2
                     ORDER BY revision DESC LIMIT 1",
                    params![session_id as i64, operation_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?
            {
                if hash == bundle.bundle_hash {
                    return read_binding(&tx, session_id, revision);
                }
                return Err(FoundationError::Conflict(format!(
                    "operation {operation_id} already captured a different instruction bundle"
                )));
            }
            let revision: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(revision), 0) + 1 FROM instruction_bundle WHERE session_id = ?1",
                    params![session_id as i64],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.execute(
                "INSERT INTO instruction_bundle(
                   session_id, revision, operation_id, bundle_hash, content, records_json, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id as i64,
                    revision,
                    operation_id,
                    bundle.bundle_hash,
                    bundle.content,
                    records_json,
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit bind: {e}")))?;
            read_binding(conn, session_id, revision)
        })
    }

    pub fn binding(
        &self,
        session_id: u64,
        revision: Option<u64>,
        operation_id: Option<&str>,
    ) -> Result<Option<InstructionBinding>, FoundationError> {
        self.db.read("instruction binding", |conn| {
            let row = match (revision, operation_id) {
                (Some(revision), _) => conn
                    .query_row(
                        "SELECT session_id, revision, operation_id, bundle_hash, content, records_json, created_ms
                         FROM instruction_bundle WHERE session_id = ?1 AND revision = ?2",
                        params![session_id as i64, revision as i64],
                        map_binding,
                    )
                    .optional(),
                (None, Some(operation)) => conn
                    .query_row(
                        "SELECT session_id, revision, operation_id, bundle_hash, content, records_json, created_ms
                         FROM instruction_bundle WHERE session_id = ?1 AND operation_id = ?2
                         ORDER BY revision DESC LIMIT 1",
                        params![session_id as i64, operation],
                        map_binding,
                    )
                    .optional(),
                (None, None) => conn
                    .query_row(
                        "SELECT session_id, revision, operation_id, bundle_hash, content, records_json, created_ms
                         FROM instruction_bundle WHERE session_id = ?1
                         ORDER BY revision DESC LIMIT 1",
                        params![session_id as i64],
                        map_binding,
                    )
                    .optional(),
            };
            row.map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn bindings(
        &self,
        session_id: u64,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<InstructionBinding>, FoundationError> {
        let limit = page_limit_i64(limit.clamp(1, MAX_BINDING_PAGE), "instruction binding")?;
        self.db.read("instruction bindings", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, revision, operation_id, bundle_hash, content, records_json, created_ms
                     FROM instruction_bundle WHERE session_id = ?1 AND revision < ?2
                     ORDER BY revision DESC LIMIT ?3",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![
                        session_id as i64,
                        before
                            .map(|v| i64::try_from(v).unwrap_or(i64::MAX))
                            .unwrap_or(i64::MAX),
                        limit
                    ],
                    map_binding,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    /// Import a native instruction bundle. A foreign format (foreign-editor/Claude
    /// files are NOT native Faktor formats) imports nothing and reports the
    /// unsupported surface instead of silently guessing.
    pub fn import(&self, format: &str, value: &serde_json::Value) -> ImportReport {
        let mut report = ImportReport {
            format: format.to_string(),
            imported: 0,
            unsupported: Vec::new(),
            errors: Vec::new(),
        };
        if format != INSTRUCTION_IMPORT_FORMAT {
            report.unsupported.push(format!(
                "format {format:?} is not {INSTRUCTION_IMPORT_FORMAT}; no instruction was imported"
            ));
            if let Some(object) = value.as_object() {
                for key in object.keys() {
                    if key != "instructions" {
                        report.unsupported.push(key.clone());
                    }
                }
            }
            return report;
        }
        let Some(object) = value.as_object() else {
            report
                .errors
                .push("the bundle must be a JSON object".into());
            return report;
        };
        for key in object.keys() {
            if key != "format" && key != "instructions" {
                report.unsupported.push(key.clone());
            }
        }
        let parsed: Result<Vec<InstructionDraft>, _> = serde_json::from_value(
            object
                .get("instructions")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new())),
        );
        let drafts = match parsed {
            Ok(drafts) => drafts,
            Err(e) => {
                report.errors.push(format!("instructions: {e}"));
                return report;
            }
        };
        for draft in &drafts {
            if draft.origin == InstructionOrigin::Managed {
                report.errors.push(format!(
                    "{}: managed rows are owned by the managed policy sync",
                    draft.title
                ));
                continue;
            }
            if let Err(e) = validate_draft(draft) {
                report.errors.push(format!("{}: {e}", draft.title));
            }
        }
        if !report.errors.is_empty() {
            return report;
        }
        match self.import_atomic(&drafts) {
            Ok(count) => report.imported = count,
            Err(e) => report.errors.push(e.to_string()),
        }
        report
    }

    fn import_atomic(&self, drafts: &[InstructionDraft]) -> Result<usize, FoundationError> {
        let now = now_ms();
        let mut prepared = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let id = derive_id(draft);
            let globs_json = serde_json::to_string(&draft.globs)
                .map_err(|e| FoundationError::Store(format!("encode globs: {e}")))?;
            prepared.push((id, draft, globs_json));
        }
        self.db.write("instruction import", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin import: {e}")))?;
            let count: i64 = tx
                .query_row("SELECT COUNT(*) FROM instruction_record", [], |r| r.get(0))
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if count as usize + prepared.len() > MAX_INSTRUCTION_RECORDS {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_INSTRUCTION_RECORDS} instruction records"
                )));
            }
            let mut imported = 0usize;
            for (id, draft, globs_json) in &prepared {
                let existing: Option<(i64, i64, String)> = tx
                    .query_row(
                        "SELECT revision, version, content_hash FROM instruction_record WHERE id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                let hash = hash_hex(draft.content.as_bytes());
                let (revision, version) = match existing {
                    Some((revision, version, old_hash)) => {
                        if old_hash == hash {
                            continue;
                        }
                        (revision.max(0) as u64 + 1, version.max(0) as u64 + 1)
                    }
                    None => (1, 1),
                };
                tx.execute(
                    "INSERT INTO instruction_record(
                       id, kind, title, content, origin, scope, activation, globs_json,
                       enabled, locked, version, content_hash, created_ms, updated_ms, revision)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12, ?13, ?14)
                     ON CONFLICT(id) DO UPDATE SET
                       kind = excluded.kind, title = excluded.title, content = excluded.content,
                       origin = excluded.origin, scope = excluded.scope,
                       activation = excluded.activation, globs_json = excluded.globs_json,
                       enabled = excluded.enabled, version = excluded.version,
                       content_hash = excluded.content_hash, updated_ms = excluded.updated_ms,
                       revision = excluded.revision",
                    params![
                        id,
                        draft.kind.as_str(),
                        draft.title,
                        draft.content,
                        draft.origin.as_str(),
                        draft.scope.as_str(),
                        draft.activation.as_str(),
                        globs_json,
                        if draft.enabled { 1 } else { 0 },
                        version as i64,
                        hash,
                        now,
                        now,
                        revision as i64
                    ],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
                imported += 1;
            }
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit import: {e}")))?;
            Ok(imported)
        })
    }
}

fn read_binding(
    conn: &rusqlite::Connection,
    session_id: u64,
    revision: i64,
) -> Result<InstructionBinding, FoundationError> {
    conn.query_row(
        "SELECT session_id, revision, operation_id, bundle_hash, content, records_json, created_ms
         FROM instruction_bundle WHERE session_id = ?1 AND revision = ?2",
        params![session_id as i64, revision],
        map_binding,
    )
    .map_err(|e| FoundationError::Store(e.to_string()))
}

fn map_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstructionBinding> {
    let records_raw: String = row.get(5)?;
    let sources: Vec<BundleSource> = serde_json::from_str(&records_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    Ok(InstructionBinding {
        session_id: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
        revision: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        operation_id: row.get(2)?,
        bundle_hash: row.get(3)?,
        content: row.get(4)?,
        sources,
        created_ms: row.get(6)?,
    })
}

/// Whole-string glob matching: `*` matches any byte sequence, `?` one byte.
/// There is no prefix/substring matching. Byte-wise is deliberate: globs
/// match UTF-8 path bytes deterministically.
pub fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = t;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, InstructionService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("foundation.db")).unwrap();
        (dir, InstructionService::new(db))
    }

    fn draft(title: &str, origin: InstructionOrigin, body: &str) -> InstructionDraft {
        InstructionDraft {
            id: None,
            kind: InstructionKind::Instruction,
            title: title.into(),
            content: body.into(),
            origin,
            scope: match origin {
                InstructionOrigin::Project => Scope::Project,
                InstructionOrigin::Managed => Scope::Managed,
                _ => Scope::User,
            },
            activation: Activation::Always,
            globs: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn precedence_conflicts_and_activation_are_exact_and_deterministic() {
        let (_dir, svc) = service();
        let project = svc
            .upsert(
                &draft("Style", InstructionOrigin::Project, "project text"),
                None,
            )
            .unwrap();
        let user = svc
            .upsert(&draft("Style", InstructionOrigin::User, "user text"), None)
            .unwrap();
        let mut globbed = draft("Rust rules", InstructionOrigin::User, "rust only");
        globbed.activation = Activation::Glob;
        globbed.globs = vec!["src/**/*.rs".into()];
        let globbed = svc.upsert(&globbed, None).unwrap();
        let mut manual = draft("Manual", InstructionOrigin::User, "manual only");
        manual.activation = Activation::Manual;
        let manual = svc.upsert(&manual, None).unwrap();

        let bundle = svc
            .preview("fix the bug", &["src/runtime/loop.rs".into()], &[])
            .unwrap();
        assert_eq!(bundle.conflicts.len(), 1);
        assert_eq!(bundle.conflicts[0].winner, project.id);
        assert_eq!(bundle.conflicts[0].shadowed, vec![user.id.clone()]);
        assert!(bundle.content.contains("project text"));
        assert!(!bundle.content.contains("user text"));
        assert!(bundle.content.contains("rust only"));
        assert!(!bundle.content.contains("manual only"));
        assert!(bundle
            .sources
            .iter()
            .any(|s| s.record_id == globbed.id && s.active));
        assert!(bundle.sources.iter().any(|s| s.record_id == manual.id
            && !s.active
            && s.inactive_reason.as_deref() == Some("manual selection required")));
        // Whole-string glob: a substring is not a match.
        assert!(!glob_match(b"src/*.rs", b"prefix/src/lib.rs"));
        assert!(glob_match(b"src/**/*.rs", b"src/runtime/loop.rs"));
    }

    #[test]
    fn versions_bounds_managed_and_disabled_states_fail_loudly() {
        let (_dir, svc) = service();
        let mut first = draft("Rules", InstructionOrigin::User, "a");
        first.id = Some("rules".into());
        let saved = svc.upsert(&first, Some(0)).unwrap();
        assert_eq!(saved.version, 1);
        first.content = "b".into();
        let changed = svc.upsert(&first, Some(1)).unwrap();
        assert_eq!(changed.version, 2);
        assert!(matches!(
            svc.upsert(&first, Some(1)),
            Err(FoundationError::Conflict(_))
        ));
        first.content = "x".repeat(MAX_INSTRUCTION_BYTES + 1);
        assert!(matches!(
            svc.upsert(&first, None),
            Err(FoundationError::Oversized(_))
        ));
        let managed = draft("Managed", InstructionOrigin::Managed, "policy");
        assert!(matches!(
            svc.upsert(&managed, None),
            Err(FoundationError::Conflict(_))
        ));
        first.content = "ok".into();
        first.enabled = false;
        let disabled = svc.upsert(&first, None).unwrap();
        let bundle = svc.preview("", &[], &[]).unwrap();
        assert!(bundle
            .sources
            .iter()
            .any(|s| s.record_id == disabled.id && !s.active));
        assert!(matches!(
            svc.delete(&disabled.id, disabled.revision + 9),
            Err(FoundationError::Conflict(_))
        ));
        assert_eq!(
            svc.delete(&disabled.id, disabled.revision).unwrap(),
            disabled.revision + 1
        );
    }

    #[test]
    fn import_is_atomic_and_reports_foreign_formats_without_guessing() {
        let (_dir, svc) = service();
        let foreign = serde_json::json!({
            "instructions": [{"title":"x"}],
            "mcpServers": {"a": {"command": "x"}},
        });
        let report = svc.import("claude-desktop/v1", &foreign);
        assert_eq!(report.imported, 0);
        assert!(report.unsupported.iter().any(|k| k == "mcpServers"));
        assert!(report
            .unsupported
            .iter()
            .any(|k| k.contains("claude-desktop")));
        assert!(svc.list().unwrap().is_empty());

        let bad = serde_json::json!({
            "format": INSTRUCTION_IMPORT_FORMAT,
            "instructions": [
                {"kind":"instruction","title":"good","content":"a","origin":"user","scope":"user"},
                {"kind":"instruction","title":"bad","content":"b","origin":"project","scope":"user"}
            ]
        });
        let report = svc.import(INSTRUCTION_IMPORT_FORMAT, &bad);
        assert_eq!(report.imported, 0);
        assert!(!report.errors.is_empty());
        assert!(
            svc.list().unwrap().is_empty(),
            "a failed import persists nothing"
        );

        let good = serde_json::json!({
            "format": INSTRUCTION_IMPORT_FORMAT,
            "instructions": [
                {"kind":"instruction","title":"good","content":"a","origin":"user","scope":"user"}
            ]
        });
        assert_eq!(svc.import(INSTRUCTION_IMPORT_FORMAT, &good).imported, 1);
    }

    #[test]
    fn bindings_are_immutable_and_paged() {
        let (_dir, svc) = service();
        svc.upsert(&draft("A", InstructionOrigin::User, "alpha"), None)
            .unwrap();
        let bundle = svc.preview("", &[], &[]).unwrap();
        let first = svc.bind(7, "op-1", &bundle).unwrap();
        assert_eq!(first.revision, 1);
        let replay = svc.bind(7, "op-1", &bundle).unwrap();
        assert_eq!(replay.revision, 1);
        svc.upsert(&draft("B", InstructionOrigin::User, "beta"), None)
            .unwrap();
        let changed = svc.preview("", &[], &[]).unwrap();
        assert_ne!(changed.bundle_hash, bundle.bundle_hash);
        assert!(matches!(
            svc.bind(7, "op-1", &changed),
            Err(FoundationError::Conflict(_))
        ));
        let second = svc.bind(7, "op-2", &changed).unwrap();
        assert_eq!(second.revision, 2);
        let page = svc.bindings(7, None, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].revision, 2);
        assert_eq!(
            svc.binding(7, None, Some("op-1"))
                .unwrap()
                .unwrap()
                .revision,
            1
        );
    }

    #[test]
    fn two_thousand_records_preview_is_linear_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("bench.db")).unwrap();
        let bench = InstructionService::new(db);
        let start = std::time::Instant::now();
        for i in 0..1_000 {
            let mut d = draft(&format!("rule {i:05}"), InstructionOrigin::User, "one line");
            d.id = Some(format!("r{i:05}"));
            bench.upsert(&d, None).unwrap();
        }
        let inserted_half = start.elapsed();
        let first = std::time::Instant::now();
        let one_k = bench.preview("prompt", &["src/a.rs".into()], &[]).unwrap();
        let preview_one_k = first.elapsed();
        for i in 1_000..2_000 {
            let mut d = draft(&format!("rule {i:05}"), InstructionOrigin::User, "one line");
            d.id = Some(format!("r{i:05}"));
            bench.upsert(&d, None).unwrap();
        }
        let second = std::time::Instant::now();
        let two_k = bench.preview("prompt", &["src/a.rs".into()], &[]).unwrap();
        let preview_two_k = second.elapsed();
        assert_eq!(one_k.sources.len(), 1_000);
        assert_eq!(two_k.sources.len(), 2_000);
        // Realistic bound: sqlite writes dominate insertion; preview is one
        // table read plus linear composition. Doubling the corpus must not
        // show a quadratic blow-up.
        assert!(
            inserted_half.as_secs() < 30,
            "insertion took {inserted_half:?}"
        );
        assert!(
            preview_two_k < std::time::Duration::from_secs(2),
            "preview of 2k records took {preview_two_k:?}"
        );
        assert!(
            preview_two_k <= preview_one_k.saturating_mul(3) + std::time::Duration::from_millis(20),
            "preview scaled super-linearly: 1k={preview_one_k:?} 2k={preview_two_k:?}"
        );
    }
}
