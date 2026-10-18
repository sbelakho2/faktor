//! Versioned agent presets and their admission envelope (Rev8 S06/S07/S33).
//!
//! A preset is operator-authored configuration: role instructions, a tool
//! allow/deny declaration, a role model/reasoning default, delegation bounds
//! and a child budget share. It NEVER grants authority by itself. The
//! envelope states exactly what the daemon can enforce at admission (the
//! mutation mode), what remains governed by the persistent permission policy,
//! and which declared tools the daemon cannot resolve.
//!
//! Bindings are durable and immutable per `(session, submission)`: a
//! replayed start with the same submission id returns the SAME envelope; a
//! different preset under the same submission id is a typed conflict.

use serde::{Deserialize, Serialize};

use rusqlite::{params, OptionalExtension};

use crate::config::Scope;
use crate::db::now_ms;
use crate::{hash_hex, FoundationDb, FoundationError};

pub const MAX_PRESETS: usize = 256;
pub const MAX_PRESET_NAME_BYTES: usize = 128;
pub const MAX_ROLE_INSTRUCTIONS_BYTES: usize = 64 * 1024;
pub const MAX_PRESET_TOOLS: usize = 512;
pub const MAX_TOOL_NAME_BYTES: usize = 128;
pub const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
pub const MAX_ROLE_MODELS: usize = 8;
pub const MAX_FALLBACK_CANDIDATES: usize = 16;
pub const MAX_MODEL_ID_BYTES: usize = 256;
pub const MAX_PROVIDER_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetMode {
    Ask,
    Plan,
    Code,
}

impl PresetMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ask" => Some(Self::Ask),
            "plan" => Some(Self::Plan),
            "code" => Some(Self::Code),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Plan => "plan",
            Self::Code => "code",
        }
    }

    /// Ask/Plan never admit a mutating task. Code does not turn a
    /// mutating request into a grant: the live permission policy still
    /// decides every effectful tool call.
    pub fn admits_mutation(self) -> bool {
        matches!(self, Self::Code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetOrigin {
    Managed,
    User,
    Project,
    Imported,
}

impl PresetOrigin {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "managed" => Some(Self::Managed),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "imported" => Some(Self::Imported),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
            Self::Imported => "imported",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegationPolicy {
    #[serde(default)]
    pub max_depth: u8,
    #[serde(default)]
    pub max_children: u16,
    #[serde(default = "default_share")]
    pub parent_budget_share_ppm: u32,
    #[serde(default)]
    pub allowed_child_presets: Vec<String>,
}

fn default_share() -> u32 {
    500_000
}

/// The four preset roles whose model choice is independently selectable.
/// A role model is a routing preference, never permission to create a child.
pub const PRESET_ROLES: [&str; 4] = ["planner", "coder", "verifier", "summarizer"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleModel {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FallbackCandidate {
    pub provider: String,
    pub model: String,
    /// Why this candidate is ordered here (trigger/price/data boundary).
    #[serde(default)]
    pub reason: Option<String>,
    /// Refuse candidates that cannot meet this quality floor.
    #[serde(default)]
    pub quality_floor_ppm: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetDraft {
    #[serde(default)]
    pub id: Option<String>,
    pub mode: PresetMode,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_origin")]
    pub origin: PresetOrigin,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default)]
    pub role_instructions: String,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub tool_allow: Vec<String>,
    #[serde(default)]
    pub tool_deny: Vec<String>,
    #[serde(default)]
    pub delegation: DelegationPolicy,
    #[serde(default)]
    pub child_concurrency: u8,
    #[serde(default = "default_share")]
    pub budget_share_ppm: u32,
    /// Per-role planner/coder/verifier/summarizer model map (M-05).
    #[serde(default)]
    pub role_models: std::collections::BTreeMap<String, RoleModel>,
    /// Ordered allowed fallback candidates (M-06).
    #[serde(default)]
    pub fallback_chain: Vec<FallbackCandidate>,
}

fn default_origin() -> PresetOrigin {
    PresetOrigin::User
}
fn default_scope() -> Scope {
    Scope::User
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetRecord {
    pub id: String,
    pub mode: PresetMode,
    pub name: String,
    pub description: String,
    pub origin: PresetOrigin,
    pub scope: Scope,
    pub role_instructions: String,
    pub model_provider: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub tool_allow: Vec<String>,
    pub tool_deny: Vec<String>,
    pub delegation: DelegationPolicy,
    pub child_concurrency: u8,
    pub budget_share_ppm: u32,
    pub role_models: std::collections::BTreeMap<String, RoleModel>,
    pub fallback_chain: Vec<FallbackCandidate>,
    pub locked: bool,
    pub version: u64,
    pub role_token_estimate: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleTokenEstimate {
    pub tokens: u64,
    pub method: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResolution {
    pub name: String,
    pub allowed: bool,
    pub denied: bool,
    pub offered: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetTools {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub inherit_managed_denies: bool,
    pub resolved: Vec<ToolResolution>,
    /// The daemon can refuse mutating ADMISSION for Ask/Plan. Tool-call
    /// filtering beyond the persistent permission policy is not claimed.
    pub dispatch_enforcement: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetEnvelope {
    pub preset_id: String,
    pub preset_version: u64,
    pub mode: PresetMode,
    pub name: String,
    pub origin: PresetOrigin,
    pub scope: Scope,
    pub role_instructions: String,
    pub role_estimate: RoleTokenEstimate,
    pub tools: PresetTools,
    pub model_provider: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub delegation: DelegationPolicy,
    pub child_concurrency: u8,
    pub budget_share_ppm: u32,
    pub role_models: std::collections::BTreeMap<String, RoleModel>,
    pub fallback_chain: Vec<FallbackCandidate>,
    pub admission_mutation_allowed: bool,
    pub apply_timing: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetBinding {
    pub session_id: u64,
    pub submission_id: String,
    pub preset_id: String,
    pub preset_version: u64,
    pub envelope: PresetEnvelope,
    pub created_ms: i64,
}

fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

fn scope_allowed(scope: Scope, origin: PresetOrigin) -> bool {
    match origin {
        PresetOrigin::Managed => matches!(scope, Scope::Managed | Scope::Machine),
        PresetOrigin::Project => matches!(scope, Scope::Project),
        PresetOrigin::User => matches!(scope, Scope::User | Scope::Machine),
        PresetOrigin::Imported => matches!(scope, Scope::User | Scope::Project),
    }
}

fn validate_tool_name(name: &str) -> Result<(), FoundationError> {
    if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES || name.contains('\u{0}') {
        return Err(FoundationError::Malformed(format!(
            "tool names must be 1..={MAX_TOOL_NAME_BYTES} bytes without NUL"
        )));
    }
    Ok(())
}

fn validate_draft(draft: &PresetDraft) -> Result<(), FoundationError> {
    if draft.name.trim().is_empty() || draft.name.len() > MAX_PRESET_NAME_BYTES {
        return Err(FoundationError::Malformed(format!(
            "preset name must be 1..={MAX_PRESET_NAME_BYTES} bytes"
        )));
    }
    if draft.name.chars().any(|c| c.is_control()) {
        return Err(FoundationError::Malformed(
            "preset name must not contain control characters".into(),
        ));
    }
    if draft.description.len() > MAX_DESCRIPTION_BYTES {
        return Err(FoundationError::Oversized(
            "preset description is too long".into(),
        ));
    }
    if draft.role_instructions.len() > MAX_ROLE_INSTRUCTIONS_BYTES {
        return Err(FoundationError::Oversized(format!(
            "role instructions exceed {MAX_ROLE_INSTRUCTIONS_BYTES} bytes"
        )));
    }
    if let Some(id) = &draft.id {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(FoundationError::Malformed(
                "preset id must be 1..=128 bytes of ASCII [A-Za-z0-9._-]".into(),
            ));
        }
    }
    if !scope_allowed(draft.scope, draft.origin) {
        return Err(FoundationError::Malformed(format!(
            "origin {} cannot use scope {}",
            draft.origin.as_str(),
            draft.scope.as_str()
        )));
    }
    if draft.tool_allow.len() > MAX_PRESET_TOOLS || draft.tool_deny.len() > MAX_PRESET_TOOLS {
        return Err(FoundationError::Oversized(format!(
            "at most {MAX_PRESET_TOOLS} tools per list"
        )));
    }
    for name in draft.tool_allow.iter().chain(draft.tool_deny.iter()) {
        validate_tool_name(name)?;
    }
    if draft.delegation.max_children > 64 {
        return Err(FoundationError::Malformed(
            "delegation.maxChildren must be <= 64".into(),
        ));
    }
    if draft.delegation.max_depth > 8 {
        return Err(FoundationError::Malformed(
            "delegation.maxDepth must be <= 8".into(),
        ));
    }
    if draft.delegation.parent_budget_share_ppm > 1_000_000 {
        return Err(FoundationError::Malformed(
            "delegation.parentBudgetSharePpm must be <= 1000000".into(),
        ));
    }
    if draft.delegation.allowed_child_presets.len() > 64 {
        return Err(FoundationError::Oversized(
            "at most 64 allowed child presets".into(),
        ));
    }
    if draft.budget_share_ppm > 1_000_000 {
        return Err(FoundationError::Malformed(
            "budgetSharePpm must be <= 1000000".into(),
        ));
    }
    if draft.child_concurrency > 64 {
        return Err(FoundationError::Malformed(
            "childConcurrency must be <= 64".into(),
        ));
    }
    if let Some(effort) = &draft.reasoning_effort {
        if !matches!(effort.as_str(), "low" | "medium" | "high" | "none") {
            return Err(FoundationError::Malformed(
                "reasoningEffort must be none, low, medium or high".into(),
            ));
        }
    }
    if draft.model_provider.is_some() != draft.model_id.is_some() {
        return Err(FoundationError::Malformed(
            "modelProvider and modelId are set together".into(),
        ));
    }
    validate_role_models(&draft.role_models)?;
    validate_fallback_chain(&draft.fallback_chain)?;
    Ok(())
}

fn validate_role_models(
    role_models: &std::collections::BTreeMap<String, RoleModel>,
) -> Result<(), FoundationError> {
    if role_models.len() > MAX_ROLE_MODELS {
        return Err(FoundationError::Oversized(format!(
            "at most {MAX_ROLE_MODELS} role model bindings"
        )));
    }
    for (role, binding) in role_models {
        if !PRESET_ROLES.contains(&role.as_str()) {
            return Err(FoundationError::Malformed(format!(
                "unknown role {role:?}; expected one of {PRESET_ROLES:?}"
            )));
        }
        validate_model_binding(&binding.provider, &binding.model)?;
        if let Some(effort) = &binding.reasoning_effort {
            if !matches!(effort.as_str(), "low" | "medium" | "high" | "none") {
                return Err(FoundationError::Malformed(
                    "role reasoningEffort must be none, low, medium or high".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_fallback_chain(chain: &[FallbackCandidate]) -> Result<(), FoundationError> {
    if chain.len() > MAX_FALLBACK_CANDIDATES {
        return Err(FoundationError::Oversized(format!(
            "at most {MAX_FALLBACK_CANDIDATES} fallback candidates"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for candidate in chain {
        validate_model_binding(&candidate.provider, &candidate.model)?;
        if !seen.insert((candidate.provider.clone(), candidate.model.clone())) {
            return Err(FoundationError::Malformed(
                "fallback chain contains a duplicate provider/model candidate".into(),
            ));
        }
        if candidate
            .quality_floor_ppm
            .is_some_and(|floor| floor > 1_000_000)
        {
            return Err(FoundationError::Malformed(
                "fallback qualityFloorPpm must be <= 1000000".into(),
            ));
        }
    }
    Ok(())
}

fn validate_model_binding(provider: &str, model: &str) -> Result<(), FoundationError> {
    if provider.is_empty()
        || provider.len() > MAX_PROVIDER_ID_BYTES
        || model.is_empty()
        || model.len() > MAX_MODEL_ID_BYTES
        || provider.chars().any(char::is_control)
        || model.chars().any(char::is_control)
    {
        return Err(FoundationError::Malformed(
            "provider/model bindings must be bounded, non-control strings".into(),
        ));
    }
    Ok(())
}

fn derive_id(draft: &PresetDraft) -> String {
    if let Some(id) = &draft.id {
        return id.clone();
    }
    let seed = format!(
        "{}:{}:{}",
        draft.origin.as_str(),
        draft.scope.as_str(),
        draft.name.trim().to_ascii_lowercase()
    );
    format!("preset-{}", &hash_hex(seed.as_bytes())[..16])
}

fn decode_json<T: serde::de::DeserializeOwned>(raw: &str, idx: usize) -> rusqlite::Result<T> {
    serde_json::from_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })
}

const COLUMNS: &str = "id, mode, name, description, origin, scope, role_instructions, model_provider, model_id, reasoning_effort, tool_allow_json, tool_deny_json, delegation_json, child_concurrency, budget_share_ppm, locked, version, created_ms, updated_ms, revision, role_models_json, fallback_chain_json";

fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<PresetRecord> {
    let mode: String = row.get(1)?;
    let origin: String = row.get(4)?;
    let scope_raw: String = row.get(5)?;
    let allow_raw: String = row.get(10)?;
    let deny_raw: String = row.get(11)?;
    let delegation_raw: String = row.get(12)?;
    let mode = PresetMode::parse(&mode).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown preset mode",
            )),
        )
    })?;
    let origin = PresetOrigin::parse(&origin).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown preset origin",
            )),
        )
    })?;
    let scope = Scope::parse(&scope_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown preset scope",
            )),
        )
    })?;
    let allow: Vec<String> = decode_json(&allow_raw, 10)?;
    let deny: Vec<String> = decode_json(&deny_raw, 11)?;
    let delegation: DelegationPolicy = decode_json(&delegation_raw, 12)?;
    let role: String = row.get(6)?;
    let role_models_raw: String = row.get(20)?;
    let fallback_raw: String = row.get(21)?;
    Ok(PresetRecord {
        role_token_estimate: estimate_tokens(&role),
        id: row.get(0)?,
        mode,
        name: row.get(2)?,
        description: row.get(3)?,
        origin,
        scope,
        role_instructions: role,
        model_provider: row.get(7)?,
        model_id: row.get(8)?,
        reasoning_effort: row.get(9)?,
        tool_allow: allow,
        tool_deny: deny,
        delegation,
        child_concurrency: u8::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
        budget_share_ppm: u32::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
        locked: row.get::<_, i64>(15)? != 0,
        version: u64::try_from(row.get::<_, i64>(16)?).unwrap_or(0),
        created_ms: row.get(17)?,
        updated_ms: row.get(18)?,
        revision: u64::try_from(row.get::<_, i64>(19)?).unwrap_or(0),
        role_models: decode_json(&role_models_raw, 20)?,
        fallback_chain: decode_json(&fallback_raw, 21)?,
    })
}

pub struct PresetService {
    db: FoundationDb,
}

impl PresetService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn list(&self) -> Result<Vec<PresetRecord>, FoundationError> {
        self.db.read("preset list", |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM preset_record ORDER BY origin ASC, name ASC LIMIT ?1"
                ))
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_PRESETS as i64], map_record)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<PresetRecord>, FoundationError> {
        self.db.read("preset get", |conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM preset_record WHERE id = ?1"),
                params![id],
                map_record,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn upsert(
        &self,
        draft: &PresetDraft,
        expected_revision: Option<u64>,
    ) -> Result<PresetRecord, FoundationError> {
        validate_draft(draft)?;
        let id = derive_id(draft);
        let now = now_ms();
        self.db.write("preset upsert", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin preset upsert: {e}")))?;
            let existing: Option<(i64, i64, String)> = tx
                .query_row(
                    "SELECT revision, version, origin FROM preset_record WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let (revision, version, locked) = match &existing {
                Some((revision, version, origin)) => {
                    if draft.origin == PresetOrigin::Managed && origin != PresetOrigin::Managed.as_str()
                    {
                        return Err(FoundationError::Conflict(
                            "managed presets are owned by the managed policy sync".into(),
                        ));
                    }
                    if let Some(expected) = expected_revision {
                        if (*revision).max(0) as u64 != expected {
                            return Err(FoundationError::Conflict(format!(
                                "preset {id} revision changed: expected {expected} but found {revision}"
                            )));
                        }
                    }
                    (
                        (*revision).max(0) as u64 + 1,
                        (*version).max(0) as u64 + 1,
                        draft.origin == PresetOrigin::Managed,
                    )
                }
                None => {
                    if draft.origin == PresetOrigin::Managed {
                        return Err(FoundationError::Conflict(
                            "managed presets are created by the managed policy sync".into(),
                        ));
                    }
                    if let Some(expected) = expected_revision {
                        if expected != 0 {
                            return Err(FoundationError::Conflict(format!(
                                "preset {id} does not exist at revision {expected}"
                            )));
                        }
                    }
                    let count: i64 = tx
                        .query_row("SELECT COUNT(*) FROM preset_record", [], |r| r.get(0))
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                    if count as usize >= MAX_PRESETS {
                        return Err(FoundationError::Oversized(format!(
                            "at most {MAX_PRESETS} presets"
                        )));
                    }
                    (1, 1, false)
                }
            };
            let allow = serde_json::to_string(&draft.tool_allow)
                .map_err(|e| FoundationError::Store(format!("encode allow: {e}")))?;
            let deny = serde_json::to_string(&draft.tool_deny)
                .map_err(|e| FoundationError::Store(format!("encode deny: {e}")))?;
            let delegation = serde_json::to_string(&draft.delegation)
                .map_err(|e| FoundationError::Store(format!("encode delegation: {e}")))?;
            let role_models = serde_json::to_string(&draft.role_models)
                .map_err(|e| FoundationError::Store(format!("encode role models: {e}")))?;
            let fallback_chain = serde_json::to_string(&draft.fallback_chain)
                .map_err(|e| FoundationError::Store(format!("encode fallback chain: {e}")))?;
            tx.execute(
                "INSERT INTO preset_record(
                   id, mode, name, description, origin, scope, role_instructions,
                   model_provider, model_id, reasoning_effort, tool_allow_json,
                   tool_deny_json, delegation_json, child_concurrency, budget_share_ppm,
                   locked, version, created_ms, updated_ms, revision,
                   role_models_json, fallback_chain_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
                 ON CONFLICT(id) DO UPDATE SET
                   mode = excluded.mode, name = excluded.name, description = excluded.description,
                   origin = excluded.origin, scope = excluded.scope,
                   role_instructions = excluded.role_instructions,
                   model_provider = excluded.model_provider, model_id = excluded.model_id,
                   reasoning_effort = excluded.reasoning_effort,
                   tool_allow_json = excluded.tool_allow_json,
                   tool_deny_json = excluded.tool_deny_json,
                   delegation_json = excluded.delegation_json,
                   child_concurrency = excluded.child_concurrency,
                   budget_share_ppm = excluded.budget_share_ppm,
                   locked = excluded.locked, version = excluded.version,
                   updated_ms = excluded.updated_ms, revision = excluded.revision,
                   role_models_json = excluded.role_models_json,
                   fallback_chain_json = excluded.fallback_chain_json",
                params![
                    id,
                    draft.mode.as_str(),
                    draft.name,
                    draft.description,
                    draft.origin.as_str(),
                    draft.scope.as_str(),
                    draft.role_instructions,
                    draft.model_provider,
                    draft.model_id,
                    draft.reasoning_effort,
                    allow,
                    deny,
                    delegation,
                    draft.child_concurrency as i64,
                    draft.budget_share_ppm as i64,
                    if locked { 1 } else { 0 },
                    version as i64,
                    now,
                    now,
                    revision as i64,
                    role_models,
                    fallback_chain
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit preset upsert: {e}")))?;
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM preset_record WHERE id = ?1"),
                params![id],
                map_record,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn delete(&self, id: &str, expected_revision: u64) -> Result<u64, FoundationError> {
        self.db.write("preset delete", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin delete: {e}")))?;
            let row: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT revision, locked FROM preset_record WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let Some((revision, locked)) = row else {
                return Err(FoundationError::NotFound(format!("preset {id}")));
            };
            if locked != 0 {
                return Err(FoundationError::Conflict(format!(
                    "preset {id} is locked by managed policy"
                )));
            }
            if revision.max(0) as u64 != expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "preset {id} revision changed: expected {expected_revision} but found {revision}"
                )));
            }
            tx.execute("DELETE FROM preset_record WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit delete: {e}")))?;
            Ok(revision.max(0) as u64 + 1)
        })
    }

    /// Replace the per-role model map atomically (M-05). The revision
    /// fence is the same one the full preset upsert uses; a stale writer is
    /// a typed conflict. Capabilities are resolved by the caller against the
    /// served registry before this write.
    pub fn set_role_models(
        &self,
        id: &str,
        role_models: std::collections::BTreeMap<String, RoleModel>,
        expected_revision: Option<u64>,
    ) -> Result<PresetRecord, FoundationError> {
        validate_role_models(&role_models)?;
        let encoded = serde_json::to_string(&role_models)
            .map_err(|e| FoundationError::Store(format!("encode role models: {e}")))?;
        self.update_preset_columns(
            id,
            expected_revision,
            "role_models_json = ?1",
            rusqlite::types::Value::Text(encoded),
        )
    }

    /// Replace the ordered fallback chain atomically (M-06).
    pub fn set_fallback_chain(
        &self,
        id: &str,
        fallback_chain: Vec<FallbackCandidate>,
        expected_revision: Option<u64>,
    ) -> Result<PresetRecord, FoundationError> {
        validate_fallback_chain(&fallback_chain)?;
        let encoded = serde_json::to_string(&fallback_chain)
            .map_err(|e| FoundationError::Store(format!("encode fallback chain: {e}")))?;
        self.update_preset_columns(
            id,
            expected_revision,
            "fallback_chain_json = ?1",
            rusqlite::types::Value::Text(encoded),
        )
    }

    fn update_preset_columns(
        &self,
        id: &str,
        expected_revision: Option<u64>,
        set_clause: &str,
        value: rusqlite::types::Value,
    ) -> Result<PresetRecord, FoundationError> {
        self.db.write("preset update", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin preset update: {e}")))?;
            let existing: Option<(i64, i64, i64)> = tx
                .query_row(
                    "SELECT revision, version, locked FROM preset_record WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let Some((revision, version, locked)) = existing else {
                return Err(FoundationError::NotFound(format!("preset {id}")));
            };
            if locked != 0 {
                return Err(FoundationError::Conflict(format!(
                    "preset {id} is locked by managed policy"
                )));
            }
            if let Some(expected) = expected_revision {
                if revision.max(0) as u64 != expected {
                    return Err(FoundationError::Conflict(format!(
                        "preset {id} revision changed: expected {expected} but found {revision}"
                    )));
                }
            }
            let revision = revision.max(0) as u64 + 1;
            let version = version.max(0) as u64 + 1;
            tx.execute(
                &format!(
                    "UPDATE preset_record SET {set_clause}, revision = ?2, version = ?3, updated_ms = ?4 WHERE id = ?5"
                ),
                rusqlite::params_from_iter([
                    value,
                    rusqlite::types::Value::Integer(revision as i64),
                    rusqlite::types::Value::Integer(version as i64),
                    rusqlite::types::Value::Integer(now_ms()),
                    rusqlite::types::Value::Text(id.to_string()),
                ]),
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit preset update: {e}")))?;
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM preset_record WHERE id = ?1"),
                params![id],
                map_record,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Resolve the declared tool names against the daemon's registered tool
    /// names. Unknown names are reported (never silently allowed). Denies
    /// always win over an allow entry.
    pub fn envelope_with(
        &self,
        preset_id: &str,
        known_tools: &[String],
    ) -> Result<PresetEnvelope, FoundationError> {
        let record = self
            .get(preset_id)?
            .ok_or_else(|| FoundationError::NotFound(format!("preset {preset_id}")))?;
        let known: std::collections::BTreeSet<&str> =
            known_tools.iter().map(String::as_str).collect();
        let allow: std::collections::BTreeSet<&str> =
            record.tool_allow.iter().map(String::as_str).collect();
        let deny: std::collections::BTreeSet<&str> =
            record.tool_deny.iter().map(String::as_str).collect();
        let mut names: Vec<String> = record
            .tool_allow
            .iter()
            .chain(record.tool_deny.iter())
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        let resolved = names
            .into_iter()
            .map(|name| {
                let denied = deny.contains(name.as_str());
                let allowed = if allow.is_empty() {
                    !denied
                } else {
                    allow.contains(name.as_str()) && !denied
                };
                let known_tool = known.contains(name.as_str());
                ToolResolution {
                    offered: known_tool && allowed,
                    reason: if denied {
                        Some("denied by this preset".into())
                    } else if !known_tool {
                        Some("tool is not registered with this daemon".into())
                    } else if !allowed {
                        Some("not in this preset's allow list".into())
                    } else {
                        None
                    },
                    name,
                    allowed,
                    denied,
                }
            })
            .collect();
        Ok(PresetEnvelope {
            preset_id: record.id,
            preset_version: record.version,
            mode: record.mode,
            name: record.name,
            origin: record.origin,
            scope: record.scope,
            role_instructions: record.role_instructions.clone(),
            role_estimate: RoleTokenEstimate {
                tokens: estimate_tokens(&record.role_instructions),
                method: "byte-quarter-estimator".into(),
                version: "1".into(),
            },
            tools: PresetTools {
                allow: record.tool_allow,
                deny: record.tool_deny,
                inherit_managed_denies: true,
                resolved,
                dispatch_enforcement: "admission_and_permission_policy".into(),
            },
            model_provider: record.model_provider,
            model_id: record.model_id,
            reasoning_effort: record.reasoning_effort,
            delegation: record.delegation,
            child_concurrency: record.child_concurrency,
            budget_share_ppm: record.budget_share_ppm,
            role_models: record.role_models,
            fallback_chain: record.fallback_chain,
            admission_mutation_allowed: record.mode.admits_mutation(),
            apply_timing: "admission".into(),
            locked: record.locked,
        })
    }

    /// Enforce the part of the envelope the daemon owns at admission: Ask
    /// and Plan refuse mutating task starts typed, before any durable row.
    pub fn admission_refusal(
        &self,
        envelope: &PresetEnvelope,
        starts_mutating: bool,
    ) -> Option<FoundationError> {
        if starts_mutating && !envelope.admission_mutation_allowed {
            return Some(FoundationError::Validation(vec![
                crate::config::FieldError {
                    key: "preset".into(),
                    scope: None,
                    code: "read_only_preset".into(),
                    message: format!(
                        "preset {} ({}) does not admit a mutating task; choose a Code preset",
                        envelope.preset_id,
                        envelope.mode.as_str()
                    ),
                },
            ]));
        }
        None
    }

    /// Bind the exact envelope to one submission. Same submission +
    /// identical envelope is idempotent; a different preset conflicts.
    pub fn bind(
        &self,
        session_id: u64,
        submission_id: &str,
        envelope: &PresetEnvelope,
    ) -> Result<PresetBinding, FoundationError> {
        if submission_id.is_empty() || submission_id.len() > 64 {
            return Err(FoundationError::Malformed(
                "submission_id must be 1..=64 bytes".into(),
            ));
        }
        let envelope_json = serde_json::to_string(envelope)
            .map_err(|e| FoundationError::Store(format!("encode envelope: {e}")))?;
        let now = now_ms();
        self.db.write("preset bind", |conn| {
            let existing: Option<(String, i64, String)> = conn
                .query_row(
                    "SELECT preset_id, preset_version, envelope_json FROM preset_binding
                     WHERE session_id = ?1 AND submission_id = ?2",
                    params![session_id as i64, submission_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some((preset_id, version, stored)) = existing {
                if preset_id == envelope.preset_id && stored == envelope_json {
                    return read_binding(conn, session_id, submission_id);
                }
                return Err(FoundationError::Conflict(format!(
                    "submission {submission_id} already captured preset {preset_id} version {version}"
                )));
            }
            conn.execute(
                "INSERT INTO preset_binding(
                   session_id, submission_id, preset_id, preset_version, envelope_json, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id as i64,
                    submission_id,
                    envelope.preset_id,
                    envelope.preset_version as i64,
                    envelope_json,
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            read_binding(conn, session_id, submission_id)
        })
    }

    pub fn binding(
        &self,
        session_id: u64,
        submission_id: &str,
    ) -> Result<Option<PresetBinding>, FoundationError> {
        self.db.read("preset binding", |conn| {
            conn.query_row(
                "SELECT session_id, submission_id, preset_id, preset_version, envelope_json, created_ms
                 FROM preset_binding WHERE session_id = ?1 AND submission_id = ?2",
                params![session_id as i64, submission_id],
                map_binding,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }
}

fn read_binding(
    conn: &rusqlite::Connection,
    session_id: u64,
    submission_id: &str,
) -> Result<PresetBinding, FoundationError> {
    conn.query_row(
        "SELECT session_id, submission_id, preset_id, preset_version, envelope_json, created_ms
         FROM preset_binding WHERE session_id = ?1 AND submission_id = ?2",
        params![session_id as i64, submission_id],
        map_binding,
    )
    .map_err(|e| FoundationError::Store(e.to_string()))
}

fn map_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<PresetBinding> {
    let raw: String = row.get(4)?;
    let envelope: PresetEnvelope = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    Ok(PresetBinding {
        session_id: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
        submission_id: row.get(1)?,
        preset_id: row.get(2)?,
        preset_version: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
        envelope,
        created_ms: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, PresetService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("foundation.db")).unwrap();
        (dir, PresetService::new(db))
    }

    fn draft(name: &str, mode: PresetMode) -> PresetDraft {
        PresetDraft {
            id: None,
            mode,
            name: name.into(),
            description: String::new(),
            origin: PresetOrigin::User,
            scope: Scope::User,
            role_instructions: "You are a careful engineer.".into(),
            model_provider: None,
            model_id: None,
            reasoning_effort: None,
            tool_allow: Vec::new(),
            tool_deny: Vec::new(),
            delegation: DelegationPolicy::default(),
            child_concurrency: 0,
            budget_share_ppm: default_share(),
            role_models: std::collections::BTreeMap::new(),
            fallback_chain: Vec::new(),
        }
    }

    #[test]
    fn envelope_reports_unresolved_tools_and_ask_refuses_mutation() {
        let (_dir, svc) = service();
        let mut d = draft("Ask", PresetMode::Ask);
        d.id = Some("ask".into());
        d.tool_allow = vec!["read".into(), "mcp:docs/search".into()];
        d.tool_deny = vec!["run".into()];
        let record = svc.upsert(&d, Some(0)).unwrap();
        assert!(!record.locked);
        let envelope = svc
            .envelope_with(
                "ask",
                &["read".into(), "mcp:docs/search".into(), "run".into()],
            )
            .unwrap();
        assert!(!envelope.admission_mutation_allowed);
        assert!(envelope
            .tools
            .resolved
            .iter()
            .any(|t| t.name == "read" && t.offered));
        assert!(envelope
            .tools
            .resolved
            .iter()
            .any(|t| t.name == "run" && t.denied));
        let unknown = svc.envelope_with("ask", &["read".into()]).unwrap();
        assert!(unknown
            .tools
            .resolved
            .iter()
            .any(|t| t.name == "mcp:docs/search"
                && !t.offered
                && t.reason.as_deref() == Some("tool is not registered with this daemon")));
        assert!(svc.admission_refusal(&envelope, true).is_some());
        assert!(svc.admission_refusal(&envelope, false).is_none());
        let mut code = draft("Code", PresetMode::Code);
        code.id = Some("code".into());
        svc.upsert(&code, None).unwrap();
        let code_env = svc.envelope_with("code", &[]).unwrap();
        assert!(svc.admission_refusal(&code_env, true).is_none());
    }

    #[test]
    fn revisions_bounds_and_deterministic_envelopes() {
        let (_dir, svc) = service();
        let mut d = draft("Plan", PresetMode::Plan);
        d.id = Some("plan".into());
        let first = svc.upsert(&d, Some(0)).unwrap();
        assert_eq!(first.version, 1);
        d.description = "updated".into();
        let second = svc.upsert(&d, Some(1)).unwrap();
        assert_eq!(second.version, 2);
        assert!(matches!(
            svc.upsert(&d, Some(1)),
            Err(FoundationError::Conflict(_))
        ));
        d.tool_allow = vec!["x".into(); MAX_PRESET_TOOLS + 1];
        assert!(matches!(
            svc.upsert(&d, None),
            Err(FoundationError::Oversized(_))
        ));
        d.tool_allow.clear();
        d.delegation.max_depth = 9;
        assert!(matches!(
            svc.upsert(&d, None),
            Err(FoundationError::Malformed(_))
        ));
        let env_a = svc.envelope_with("plan", &[]).unwrap();
        let env_b = svc.envelope_with("plan", &[]).unwrap();
        assert_eq!(
            serde_json::to_string(&env_a).unwrap(),
            serde_json::to_string(&env_b).unwrap()
        );
    }

    #[test]
    fn bindings_are_immutable_per_submission() {
        let (_dir, svc) = service();
        let mut ask = draft("Ask", PresetMode::Ask);
        ask.id = Some("ask".into());
        svc.upsert(&ask, None).unwrap();
        let env = svc.envelope_with("ask", &[]).unwrap();
        let first = svc.bind(4, "sub-1", &env).unwrap();
        assert_eq!(first.preset_version, 1);
        assert_eq!(svc.bind(4, "sub-1", &env).unwrap().preset_version, 1);
        let mut code = draft("Code", PresetMode::Code);
        code.id = Some("code".into());
        svc.upsert(&code, None).unwrap();
        let code_env = svc.envelope_with("code", &[]).unwrap();
        assert!(matches!(
            svc.bind(4, "sub-1", &code_env),
            Err(FoundationError::Conflict(_))
        ));
        assert!(svc.binding(9, "sub-1").unwrap().is_none());
    }
}
