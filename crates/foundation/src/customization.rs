//! BK03/BK06 customization lifecycle: bounded MCP stdio specs, versioned
//! trusted skills with invocation policy, and hook specs. Installation is
//! distinct from permission: nothing here widens the runtime capability
//! envelope, and viewing a manifest never starts an executable.

use std::collections::BTreeSet;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{now_ms, FoundationDb};
use crate::FoundationError;

pub const MAX_MCP_SERVERS: usize = 8;
pub const MAX_MCP_NAME_BYTES: usize = 128;
pub const MAX_MCP_COMMAND_BYTES: usize = 512;
pub const MAX_MCP_ARGS: usize = 32;
pub const MAX_MCP_ARG_BYTES: usize = 512;
pub const MAX_MCP_ENV: usize = 32;
pub const MAX_SKILL_BYTES: usize = 64 * 1024;
pub const MAX_SKILLS: usize = 2_000;
pub const MAX_HOOKS: usize = 256;
pub const MAX_HOOK_SPEC_BYTES: usize = 64 * 1024;
pub const ORIGIN_USER: &str = "user";
pub const ORIGIN_PROJECT: &str = "project";
pub const ORIGIN_MANAGED: &str = "managed";
pub const ORIGIN_BUILTIN: &str = "builtin";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerRecord {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub trust_origin: String,
    pub trusted: bool,
    pub enabled: bool,
    /// `stdio` or `streamable_http`. Remote transport remains typed
    /// unsupported by the runtime until a negotiated adapter exists; auth
    /// and bounds are still durable and enforced where the transport is.
    pub transport: String,
    pub endpoint: Option<String>,
    pub auth_state: String,
    pub auth_account: Option<String>,
    pub auth_scopes: Vec<String>,
    /// Opaque `vault://` handle; the token bytes never leave the vault.
    pub auth_token_ref: Option<String>,
    pub timeout_ms: Option<u64>,
    pub output_cap_bytes: Option<u64>,
    pub stderr_cap_bytes: Option<u64>,
    pub revision: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerDraft {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `(name, opaque secret reference)` entries; a raw value is refused.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub trust_origin: Option<String>,
    #[serde(default)]
    pub trusted: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `stdio` (default) or `streamable_http`.
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    /// T-10 per-server bounds (timeouts 100..=600000 ms; byte caps
    /// 1 KiB..=64 MiB).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub output_cap_bytes: Option<u64>,
    #[serde(default)]
    pub stderr_cap_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillRecord {
    pub id: String,
    pub name: String,
    pub origin: String,
    pub source_uri: String,
    pub version: u64,
    pub content_hash: String,
    pub trusted: bool,
    /// `manual` | `agent_selectable` | `disabled`
    pub invocation: String,
    pub required_capabilities: Vec<String>,
    pub text: String,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillDraft {
    pub name: String,
    #[serde(default)]
    pub origin: Option<String>,
    pub source_uri: String,
    pub text: String,
    #[serde(default)]
    pub trusted: Option<bool>,
    #[serde(default)]
    pub invocation: Option<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookRecord {
    pub id: String,
    pub spec: Value,
    pub trusted: bool,
    pub enabled: bool,
    pub revision: u64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDraft {
    pub id: String,
    pub spec: Value,
    #[serde(default)]
    pub trusted: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub struct CustomizationService {
    db: FoundationDb,
}

impl CustomizationService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    // ------------------------------------------------------------- MCP

    pub fn mcp_servers(&self) -> Result<Vec<McpServerRecord>, FoundationError> {
        self.db.read("mcp list", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, command, args_json, env_json, trust_origin,
                            trusted, enabled, created_ms, updated_ms, revision,
                            transport, endpoint, auth_state, auth_account,
                            auth_scopes_json, auth_token_ref, timeout_ms,
                            output_cap_bytes, stderr_cap_bytes
                     FROM mcp_server ORDER BY created_ms ASC, id ASC LIMIT ?1",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_MCP_SERVERS as i64], map_mcp)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn mcp_server(&self, id: &str) -> Result<Option<McpServerRecord>, FoundationError> {
        self.db.read("mcp get", |conn| {
            conn.query_row(
                "SELECT id, name, command, args_json, env_json, trust_origin,
                        trusted, enabled, created_ms, updated_ms, revision,
                        transport, endpoint, auth_state, auth_account,
                        auth_scopes_json, auth_token_ref, timeout_ms,
                        output_cap_bytes, stderr_cap_bytes
                 FROM mcp_server WHERE id = ?1",
                params![id],
                map_mcp,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Validate and save ONE stdio spec. Bounds are in UTF-8 BYTES, exactly
    /// as served; the prior valid config survives any failure because
    /// validation happens before the transaction.
    pub fn save_mcp_server(
        &self,
        draft: &McpServerDraft,
        expected_revision: Option<u64>,
    ) -> Result<McpServerRecord, FoundationError> {
        validate_mcp(draft)?;
        let id = draft.id.clone().unwrap_or_else(|| slug_id(&draft.name));
        if id.is_empty()
            || id.len() > MAX_MCP_NAME_BYTES
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return Err(FoundationError::Malformed(
                "mcp id must be 1..=128 bytes of ASCII letters, digits, '_', '-' or '.'".into(),
            ));
        }
        let transport = draft.transport.as_deref().unwrap_or("stdio");
        if transport != "stdio" && transport != "streamable_http" {
            return Err(FoundationError::Malformed(
                "MCP transport must be stdio or streamable_http".into(),
            ));
        }
        if transport == "stdio" && draft.command.trim().is_empty() {
            return Err(FoundationError::Malformed(
                "stdio MCP servers require a non-empty command".into(),
            ));
        }
        let endpoint = match (transport, draft.endpoint.as_deref()) {
            ("streamable_http", Some(endpoint)) => Some(validate_mcp_endpoint(endpoint)?),
            ("streamable_http", None) => {
                return Err(FoundationError::Malformed(
                    "streamable_http MCP servers require an endpoint".into(),
                ))
            }
            (_, Some(_)) => {
                return Err(FoundationError::Malformed(
                    "endpoint is only valid for streamable_http MCP servers".into(),
                ))
            }
            _ => None,
        };
        let timeout_ms = match draft.timeout_ms {
            None => None,
            Some(v) if (100..=600_000).contains(&v) => Some(v),
            Some(_) => {
                return Err(FoundationError::Malformed(
                    "MCP timeout_ms must be 100..=600000".into(),
                ))
            }
        };
        let cap = |value: Option<u64>, field: &str| -> Result<Option<u64>, FoundationError> {
            match value {
                None => Ok(None),
                Some(v) if (1_024..=64 * 1024 * 1024).contains(&v) => Ok(Some(v)),
                Some(_) => Err(FoundationError::Malformed(format!(
                    "{field} must be 1024..=67108864 bytes"
                ))),
            }
        };
        let output_cap_bytes = cap(draft.output_cap_bytes, "output_cap_bytes")?;
        let stderr_cap_bytes = cap(draft.stderr_cap_bytes, "stderr_cap_bytes")?;
        let origin = draft
            .trust_origin
            .clone()
            .unwrap_or_else(|| ORIGIN_USER.into());
        if !matches!(
            origin.as_str(),
            ORIGIN_USER | ORIGIN_PROJECT | ORIGIN_MANAGED
        ) {
            return Err(FoundationError::Malformed(
                "trust_origin must be user, project or managed".into(),
            ));
        }
        self.db.write("mcp save", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin mcp save: {e}")))?;
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM mcp_server WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some(revision) = existing {
                if let Some(expected) = expected_revision {
                    if revision.max(0) as u64 != expected {
                        return Err(FoundationError::Conflict(format!(
                            "mcp server {id} revision changed: expected {expected} but found {revision}"
                        )));
                    }
                }
            } else {
                let count: i64 = tx
                    .query_row("SELECT COUNT(*) FROM mcp_server", [], |r| r.get(0))
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                if count as usize >= MAX_MCP_SERVERS {
                    return Err(FoundationError::Oversized(format!(
                        "at most {MAX_MCP_SERVERS} MCP servers"
                    )));
                }
            }
            let revision = existing.map(|r| r.max(0) as u64 + 1).unwrap_or(1);
            let args = serde_json::to_string(&draft.args)
                .map_err(|e| FoundationError::Store(format!("encode args: {e}")))?;
            let env = serde_json::to_string(&draft.env)
                .map_err(|e| FoundationError::Store(format!("encode env: {e}")))?;
            let trusted = draft.trusted.unwrap_or(origin != ORIGIN_PROJECT);
            let enabled = draft.enabled.unwrap_or(false);
            let now = now_ms();
            tx.execute(
                "INSERT INTO mcp_server(
                   id, name, command, args_json, env_json, trust_origin, trusted,
                   enabled, created_ms, updated_ms, revision, transport, endpoint,
                   timeout_ms, output_cap_bytes, stderr_cap_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   command = excluded.command,
                   args_json = excluded.args_json,
                   env_json = excluded.env_json,
                   trust_origin = excluded.trust_origin,
                   trusted = excluded.trusted,
                   enabled = excluded.enabled,
                   updated_ms = excluded.updated_ms,
                   revision = excluded.revision,
                   transport = excluded.transport,
                   endpoint = excluded.endpoint,
                   timeout_ms = excluded.timeout_ms,
                   output_cap_bytes = excluded.output_cap_bytes,
                   stderr_cap_bytes = excluded.stderr_cap_bytes",
                params![
                    id,
                    draft.name,
                    draft.command,
                    args,
                    env,
                    origin,
                    if trusted { 1 } else { 0 },
                    if enabled { 1 } else { 0 },
                    now,
                    now,
                    revision as i64,
                    transport,
                    endpoint,
                    timeout_ms.map(|v| v as i64),
                    output_cap_bytes.map(|v| v as i64),
                    stderr_cap_bytes.map(|v| v as i64)
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let record = tx
                .query_row(
                    "SELECT id, name, command, args_json, env_json, trust_origin,
                            trusted, enabled, created_ms, updated_ms, revision,
                            transport, endpoint, auth_state, auth_account,
                            auth_scopes_json, auth_token_ref, timeout_ms,
                            output_cap_bytes, stderr_cap_bytes
                     FROM mcp_server WHERE id = ?1",
                    params![id],
                    map_mcp,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit mcp save: {e}")))?;
            Ok(record)
        })
    }

    /// Bind or clear the OAuth/token lifecycle of one REMOTE MCP server
    /// (T-06). The token itself lives in the vault; this row carries only
    /// the opaque `vault://` handle, the account label and granted scopes.
    /// A failed/absent token never reads as authorized. The durable state is
    /// `authorized` (a stored, unexpired credential) — never a fabricated
    /// "connected" network state.
    pub fn set_mcp_auth(
        &self,
        id: &str,
        token_ref: Option<String>,
        account: Option<String>,
        scopes: Vec<String>,
        expected_revision: Option<u64>,
    ) -> Result<McpServerRecord, FoundationError> {
        if let Some(reference) = &token_ref {
            if !reference.starts_with("vault://") {
                return Err(FoundationError::Malformed(
                    "MCP auth token_ref must be an opaque vault:// reference".into(),
                ));
            }
        }
        if let Some(account) = &account {
            if account.is_empty() || account.len() > 256 || account.chars().any(char::is_control) {
                return Err(FoundationError::Malformed(
                    "MCP auth account must be 1..=256 non-control bytes".into(),
                ));
            }
        }
        if scopes.len() > 32 {
            return Err(FoundationError::Oversized(
                "at most 32 MCP auth scopes".into(),
            ));
        }
        for scope in &scopes {
            if scope.is_empty() || scope.len() > 128 || scope.chars().any(char::is_control) {
                return Err(FoundationError::Malformed(
                    "MCP auth scopes must be 1..=128 non-control bytes".into(),
                ));
            }
        }
        let state = if token_ref.is_some() {
            "authorized"
        } else {
            "signed_out"
        };
        let scopes_json = serde_json::to_string(&scopes)
            .map_err(|e| FoundationError::Store(format!("encode scopes: {e}")))?;
        self.db.write("mcp auth", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin mcp auth: {e}")))?;
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT revision, transport FROM mcp_server WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let Some((revision, transport)) = existing else {
                return Err(FoundationError::NotFound(format!("mcp server {id}")));
            };
            if transport != "streamable_http" {
                return Err(FoundationError::Conflict(format!(
                    "mcp server {id} is a stdio server; OAuth binding applies to streamable_http only"
                )));
            }
            if let Some(expected) = expected_revision {
                if revision.max(0) as u64 != expected {
                    return Err(FoundationError::Conflict(format!(
                        "mcp server {id} revision changed: expected {expected} but found {revision}"
                    )));
                }
            }
            tx.execute(
                "UPDATE mcp_server SET auth_state = ?2, auth_account = ?3,
                   auth_scopes_json = ?4, auth_token_ref = ?5,
                   revision = revision + 1, updated_ms = ?6
                 WHERE id = ?1",
                params![id, state, account, scopes_json, token_ref, now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit mcp auth: {e}")))?;
            conn.query_row(
                "SELECT id, name, command, args_json, env_json, trust_origin,
                        trusted, enabled, created_ms, updated_ms, revision,
                        transport, endpoint, auth_state, auth_account,
                        auth_scopes_json, auth_token_ref, timeout_ms,
                        output_cap_bytes, stderr_cap_bytes
                 FROM mcp_server WHERE id = ?1",
                params![id],
                map_mcp,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn delete_mcp_server(&self, id: &str) -> Result<(), FoundationError> {
        self.db.write("mcp delete", |conn| {
            let changed = conn
                .execute("DELETE FROM mcp_server WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!("mcp server {id}")));
            }
            Ok(())
        })
    }

    // ----------------------------------------------------------- skills

    pub fn skills(&self) -> Result<Vec<SkillRecord>, FoundationError> {
        self.db.read("skill list", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, origin, source_uri, version, content_hash,
                            trusted, invocation, required_capabilities_json, text, updated_ms
                     FROM skill_record ORDER BY name ASC, id ASC LIMIT ?1",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_SKILLS as i64], map_skill)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn skill(&self, id: &str) -> Result<Option<SkillRecord>, FoundationError> {
        self.db.read("skill get", |conn| {
            conn.query_row(
                "SELECT id, name, origin, source_uri, version, content_hash,
                        trusted, invocation, required_capabilities_json, text, updated_ms
                 FROM skill_record WHERE id = ?1 OR name = ?1 ORDER BY id ASC LIMIT 1",
                params![id],
                map_skill,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Install/update one versioned skill. A failed update leaves the prior
    /// row untouched because the whole validation+write is one transaction.
    pub fn save_skill(
        &self,
        draft: &SkillDraft,
        expected_revision: Option<u64>,
    ) -> Result<SkillRecord, FoundationError> {
        validate_skill(draft)?;
        let id = format!("skill-{}", &hash_id(&draft.name)[..24]);
        let origin = draft.origin.clone().unwrap_or_else(|| ORIGIN_USER.into());
        if !matches!(
            origin.as_str(),
            ORIGIN_USER | ORIGIN_PROJECT | ORIGIN_MANAGED | ORIGIN_BUILTIN
        ) {
            return Err(FoundationError::Malformed(
                "origin must be builtin, user, project or managed".into(),
            ));
        }
        let invocation = draft.invocation.clone().unwrap_or_else(|| "manual".into());
        if !matches!(
            invocation.as_str(),
            "manual" | "agent_selectable" | "disabled"
        ) {
            return Err(FoundationError::Malformed(
                "invocation must be manual, agent_selectable or disabled".into(),
            ));
        }
        let content_hash = hex_digest(draft.text.as_bytes());
        self.db.write("skill save", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin skill save: {e}")))?;
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT version FROM skill_record WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some(version) = existing {
                if let Some(expected) = expected_revision {
                    if version.max(0) as u64 != expected {
                        return Err(FoundationError::Conflict(format!(
                            "skill {id} version changed: expected {expected} but found {version}"
                        )));
                    }
                }
            }
            let version = existing.map(|v| v.max(0) as u64 + 1).unwrap_or(1);
            let caps = serde_json::to_string(&draft.required_capabilities)
                .map_err(|e| FoundationError::Store(format!("encode capabilities: {e}")))?;
            let trusted = draft.trusted.unwrap_or(origin != ORIGIN_PROJECT);
            tx.execute(
                "INSERT INTO skill_record(
                   id, name, origin, source_uri, version, content_hash, trusted,
                   invocation, required_capabilities_json, text, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   origin = excluded.origin,
                   source_uri = excluded.source_uri,
                   version = excluded.version,
                   content_hash = excluded.content_hash,
                   trusted = excluded.trusted,
                   invocation = excluded.invocation,
                   required_capabilities_json = excluded.required_capabilities_json,
                   text = excluded.text,
                   updated_ms = excluded.updated_ms",
                params![
                    id,
                    draft.name,
                    origin,
                    draft.source_uri,
                    version as i64,
                    content_hash,
                    if trusted { 1 } else { 0 },
                    invocation,
                    caps,
                    draft.text,
                    now_ms()
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let record = tx
                .query_row(
                    "SELECT id, name, origin, source_uri, version, content_hash,
                            trusted, invocation, required_capabilities_json, text, updated_ms
                     FROM skill_record WHERE id = ?1",
                    params![id],
                    map_skill,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit skill save: {e}")))?;
            Ok(record)
        })
    }

    /// Trust is a separate, explicit action; installing never grants it to a
    /// project-origin skill.
    pub fn trust_skill(&self, id: &str, trusted: bool) -> Result<SkillRecord, FoundationError> {
        self.db.write("skill trust", |conn| {
            let changed = conn
                .execute(
                    "UPDATE skill_record SET trusted = ?2, updated_ms = ?3 WHERE id = ?1",
                    params![id, if trusted { 1 } else { 0 }, now_ms()],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!("skill {id}")));
            }
            conn.query_row(
                "SELECT id, name, origin, source_uri, version, content_hash,
                        trusted, invocation, required_capabilities_json, text, updated_ms
                 FROM skill_record WHERE id = ?1",
                params![id],
                map_skill,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn set_skill_invocation(
        &self,
        id: &str,
        invocation: &str,
    ) -> Result<SkillRecord, FoundationError> {
        if !matches!(invocation, "manual" | "agent_selectable" | "disabled") {
            return Err(FoundationError::Malformed(
                "invocation must be manual, agent_selectable or disabled".into(),
            ));
        }
        self.db.write("skill invocation", |conn| {
            let changed = conn
                .execute(
                    "UPDATE skill_record SET invocation = ?2, updated_ms = ?3 WHERE id = ?1",
                    params![id, invocation, now_ms()],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!("skill {id}")));
            }
            conn.query_row(
                "SELECT id, name, origin, source_uri, version, content_hash,
                        trusted, invocation, required_capabilities_json, text, updated_ms
                 FROM skill_record WHERE id = ?1",
                params![id],
                map_skill,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn delete_skill(&self, id: &str) -> Result<(), FoundationError> {
        self.db.write("skill delete", |conn| {
            let changed = conn
                .execute("DELETE FROM skill_record WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!("skill {id}")));
            }
            Ok(())
        })
    }

    /// Refuse invocation when untrusted/disabled/manual-as-agent or when a
    /// required capability is not granted. Instruction text never grants.
    pub fn may_invoke(
        &self,
        record: &SkillRecord,
        agent_selected: bool,
        granted: &BTreeSet<String>,
    ) -> Result<(), FoundationError> {
        if !record.trusted {
            return Err(FoundationError::Conflict(
                "review the skill source and trust it before invocation".into(),
            ));
        }
        if record.invocation == "disabled" || (agent_selected && record.invocation == "manual") {
            return Err(FoundationError::Conflict(
                "skill invocation policy refuses this caller".into(),
            ));
        }
        let missing: Vec<&String> = record
            .required_capabilities
            .iter()
            .filter(|cap| !granted.contains(*cap))
            .collect();
        if !missing.is_empty() {
            return Err(FoundationError::Conflict(format!(
                "missing required capability grants: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------ hooks

    pub fn hooks(&self) -> Result<Vec<HookRecord>, FoundationError> {
        self.db.read("hook list", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, spec_json, trusted, enabled, revision, updated_ms
                     FROM hook_record ORDER BY id ASC LIMIT ?1",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_HOOKS as i64], map_hook)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn hook(&self, id: &str) -> Result<Option<HookRecord>, FoundationError> {
        self.db.read("hook get", |conn| {
            conn.query_row(
                "SELECT id, spec_json, trusted, enabled, revision, updated_ms
                 FROM hook_record WHERE id = ?1",
                params![id],
                map_hook,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn save_hook(
        &self,
        draft: &HookDraft,
        expected_revision: Option<u64>,
    ) -> Result<HookRecord, FoundationError> {
        if draft.id.is_empty() || draft.id.len() > 128 {
            return Err(FoundationError::Malformed(
                "hook id must be 1..=128 bytes".into(),
            ));
        }
        let encoded = serde_json::to_vec(&draft.spec)
            .map_err(|e| FoundationError::Store(format!("encode hook spec: {e}")))?;
        if encoded.len() > MAX_HOOK_SPEC_BYTES {
            return Err(FoundationError::Oversized(format!(
                "hook spec is {} bytes, over {MAX_HOOK_SPEC_BYTES}",
                encoded.len()
            )));
        }
        self.db.write("hook save", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin hook save: {e}")))?;
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM hook_record WHERE id = ?1",
                    params![draft.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some(revision) = existing {
                if let Some(expected) = expected_revision {
                    if revision.max(0) as u64 != expected {
                        return Err(FoundationError::Conflict(format!(
                            "hook {} revision changed: expected {expected} but found {revision}",
                            draft.id
                        )));
                    }
                }
            } else {
                let count: i64 = tx
                    .query_row("SELECT COUNT(*) FROM hook_record", [], |r| r.get(0))
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                if count as usize >= MAX_HOOKS {
                    return Err(FoundationError::Oversized(format!(
                        "at most {MAX_HOOKS} hooks"
                    )));
                }
            }
            let revision = existing.map(|r| r.max(0) as u64 + 1).unwrap_or(1);
            let raw = String::from_utf8(encoded.clone()).map_err(|_| {
                FoundationError::Malformed("hook spec must be valid UTF-8 JSON".into())
            })?;
            tx.execute(
                "INSERT INTO hook_record(id, spec_json, trusted, enabled, updated_ms, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   spec_json = excluded.spec_json,
                   trusted = excluded.trusted,
                   enabled = excluded.enabled,
                   updated_ms = excluded.updated_ms,
                   revision = excluded.revision",
                params![
                    draft.id,
                    raw,
                    if draft.trusted.unwrap_or(true) { 1 } else { 0 },
                    if draft.enabled.unwrap_or(false) { 1 } else { 0 },
                    now_ms(),
                    revision as i64
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let record = tx
                .query_row(
                    "SELECT id, spec_json, trusted, enabled, revision, updated_ms
                     FROM hook_record WHERE id = ?1",
                    params![draft.id],
                    map_hook,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit hook save: {e}")))?;
            Ok(record)
        })
    }

    pub fn delete_hook(&self, id: &str) -> Result<(), FoundationError> {
        self.db.write("hook delete", |conn| {
            let changed = conn
                .execute("DELETE FROM hook_record WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!("hook {id}")));
            }
            Ok(())
        })
    }
}

fn validate_mcp(draft: &McpServerDraft) -> Result<(), FoundationError> {
    if draft.name.is_empty() || draft.name.len() > MAX_MCP_NAME_BYTES {
        return Err(FoundationError::Validation(vec![
            crate::config::FieldError {
                key: "name".into(),
                scope: None,
                code: "oversized".into(),
                message: format!("MCP name must fit {MAX_MCP_NAME_BYTES} UTF-8 bytes"),
            },
        ]));
    }
    if draft.command.len() > MAX_MCP_COMMAND_BYTES {
        return Err(FoundationError::Validation(vec![
            crate::config::FieldError {
                key: "command".into(),
                scope: None,
                code: "oversized".into(),
                message: format!("MCP command must fit {MAX_MCP_COMMAND_BYTES} bytes"),
            },
        ]));
    }
    if draft.args.len() > MAX_MCP_ARGS {
        return Err(FoundationError::Validation(vec![
            crate::config::FieldError {
                key: "args".into(),
                scope: None,
                code: "oversized".into(),
                message: format!("at most {MAX_MCP_ARGS} arguments"),
            },
        ]));
    }
    for arg in &draft.args {
        if arg.len() > MAX_MCP_ARG_BYTES {
            return Err(FoundationError::Validation(vec![
                crate::config::FieldError {
                    key: "args".into(),
                    scope: None,
                    code: "oversized".into(),
                    message: format!("each argument must fit {MAX_MCP_ARG_BYTES} bytes"),
                },
            ]));
        }
    }
    if draft.env.len() > MAX_MCP_ENV {
        return Err(FoundationError::Oversized(format!(
            "at most {MAX_MCP_ENV} environment references"
        )));
    }
    let mut names = BTreeSet::new();
    for (name, value) in &draft.env {
        let valid_name = !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid_name {
            return Err(FoundationError::Malformed(format!(
                "environment name {name:?} is invalid"
            )));
        }
        if !names.insert(name.as_str()) {
            return Err(FoundationError::Malformed(format!(
                "duplicate environment name {name:?}"
            )));
        }
        if value.is_empty() {
            continue; // inherit from the daemon env
        }
        if !crate::config::valid_secret_reference(value) {
            return Err(FoundationError::Validation(vec![
                crate::config::FieldError {
                    key: "env".into(),
                    scope: None,
                    code: "secret_reference".into(),
                    message: format!(
                        "MCP environment entry {name:?} must be an opaque secret reference"
                    ),
                },
            ]));
        }
    }
    Ok(())
}

fn validate_skill(draft: &SkillDraft) -> Result<(), FoundationError> {
    if draft.name.is_empty() || draft.name.len() > 128 {
        return Err(FoundationError::Malformed(
            "skill name must be 1..=128 bytes".into(),
        ));
    }
    if draft.source_uri.is_empty() || draft.source_uri.len() > 8192 {
        return Err(FoundationError::Malformed(
            "skill source_uri must be 1..=8192 bytes".into(),
        ));
    }
    if draft.text.len() > MAX_SKILL_BYTES {
        return Err(FoundationError::Oversized(format!(
            "skill text is {} bytes, over {MAX_SKILL_BYTES}",
            draft.text.len()
        )));
    }
    let allowed: BTreeSet<&str> = ["read", "write", "execute", "network", "git", "mcp"]
        .into_iter()
        .collect();
    for cap in &draft.required_capabilities {
        if !allowed.contains(cap.as_str()) {
            return Err(FoundationError::Malformed(format!(
                "unknown required capability {cap:?}"
            )));
        }
    }
    Ok(())
}

fn map_mcp(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServerRecord> {
    let args_raw: String = row.get(3)?;
    let env_raw: String = row.get(4)?;
    Ok(McpServerRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        command: row.get(2)?,
        args: serde_json::from_str(&args_raw).unwrap_or_default(),
        env: serde_json::from_str(&env_raw).unwrap_or_default(),
        trust_origin: row.get(5)?,
        trusted: row.get::<_, i64>(6)? != 0,
        enabled: row.get::<_, i64>(7)? != 0,
        created_ms: row.get(8)?,
        updated_ms: row.get(9)?,
        revision: u64::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
        transport: row.get(11)?,
        endpoint: row.get(12)?,
        auth_state: row.get(13)?,
        auth_account: row.get(14)?,
        auth_scopes: serde_json::from_str(&row.get::<_, String>(15)?).unwrap_or_default(),
        auth_token_ref: row.get(16)?,
        timeout_ms: row
            .get::<_, Option<i64>>(17)?
            .and_then(|v| u64::try_from(v).ok()),
        output_cap_bytes: row
            .get::<_, Option<i64>>(18)?
            .and_then(|v| u64::try_from(v).ok()),
        stderr_cap_bytes: row
            .get::<_, Option<i64>>(19)?
            .and_then(|v| u64::try_from(v).ok()),
    })
}

fn map_skill(row: &rusqlite::Row<'_>) -> rusqlite::Result<SkillRecord> {
    let caps_raw: String = row.get(8)?;
    Ok(SkillRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        origin: row.get(2)?,
        source_uri: row.get(3)?,
        version: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
        content_hash: row.get(5)?,
        trusted: row.get::<_, i64>(6)? != 0,
        invocation: row.get(7)?,
        required_capabilities: serde_json::from_str(&caps_raw).unwrap_or_default(),
        text: row.get(9)?,
        updated_ms: row.get(10)?,
    })
}

fn map_hook(row: &rusqlite::Row<'_>) -> rusqlite::Result<HookRecord> {
    let spec_raw: String = row.get(1)?;
    Ok(HookRecord {
        id: row.get(0)?,
        spec: serde_json::from_str(&spec_raw).unwrap_or(Value::Null),
        trusted: row.get::<_, i64>(2)? != 0,
        enabled: row.get::<_, i64>(3)? != 0,
        revision: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
        updated_ms: row.get(5)?,
    })
}

/// Validate a persisted MCP HTTP endpoint before it can be named by any
/// future transport adapter.
///
/// Remote MCP is typed unsupported today, but the endpoint is durable and is
/// surfaced by the tools view. It must therefore already obey the boundary a
/// transport would need: absolute http(s), no embedded userinfo, no
/// query/fragment (credentials belong in the vault reference), and no
/// special-purpose literal destination (loopback, private, link-local,
/// IPv4-mapped/NAT64/6to4/Teredo aliases). Classification is the ONE shared
/// [`faktor_security::network::classify_ip`] authority, never a local table.
fn validate_mcp_endpoint(endpoint: &str) -> Result<String, FoundationError> {
    let parsed = url::Url::parse(endpoint).map_err(|_| {
        FoundationError::Malformed("MCP endpoint must be an absolute http(s) URL".into())
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(FoundationError::Malformed(
            "MCP endpoint must be an absolute http(s) URL".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(FoundationError::Malformed(
            "MCP endpoint must not embed userinfo credentials".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(FoundationError::Malformed(
            "MCP endpoint must not carry a query or fragment; credentials belong in the vault reference".into(),
        ));
    }
    match parsed.host() {
        Some(url::Host::Domain(domain)) => {
            let lower = domain.trim_end_matches('.').to_ascii_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") {
                return Err(FoundationError::Malformed(
                    "MCP endpoint must not target loopback".into(),
                ));
            }
        }
        Some(url::Host::Ipv4(ip)) => {
            if faktor_security::network::classify_ip(std::net::IpAddr::V4(ip))
                != faktor_security::network::AddressClass::Global
            {
                return Err(FoundationError::Malformed(
                    "MCP endpoint must not target a special-purpose address".into(),
                ));
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if faktor_security::network::classify_ip(std::net::IpAddr::V6(ip))
                != faktor_security::network::AddressClass::Global
            {
                return Err(FoundationError::Malformed(
                    "MCP endpoint must not target a special-purpose address".into(),
                ));
            }
        }
        None => {
            return Err(FoundationError::Malformed(
                "MCP endpoint must be an absolute http(s) URL".into(),
            ))
        }
    }
    Ok(parsed.to_string())
}

fn slug_id(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 128 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        format!("mcp-{}", &hash_id(name)[..16])
    } else {
        trimmed
    }
}

fn hash_id(value: &str) -> String {
    hex_digest(value.as_bytes())
}

fn hex_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, CustomizationService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        (dir, CustomizationService::new(db))
    }

    fn mcp(name: &str) -> McpServerDraft {
        McpServerDraft {
            id: None,
            name: name.into(),
            command: "npx".into(),
            args: vec!["server".into()],
            env: vec![],
            trust_origin: None,
            trusted: None,
            enabled: None,
            transport: None,
            endpoint: None,
            timeout_ms: None,
            output_cap_bytes: None,
            stderr_cap_bytes: None,
        }
    }

    #[test]
    fn mcp_bounds_are_bytes_and_keep_the_prior_valid_config() {
        let (_dir, svc) = service();
        let saved = svc.save_mcp_server(&mcp("ok"), None).unwrap();
        assert!(svc.mcp_server(&saved.id).unwrap().is_some());
        let mut over = mcp("too-many");
        over.args = vec!["x".into(); MAX_MCP_ARGS + 1];
        assert!(svc.save_mcp_server(&over, None).is_err());
        let mut over = mcp("long-arg");
        over.args = vec!["a".repeat(MAX_MCP_ARG_BYTES + 1)];
        assert!(svc.save_mcp_server(&over, None).is_err());
        let mut over = mcp("secret");
        over.env = vec![("API_KEY".into(), "sk-raw".into())];
        assert!(svc.save_mcp_server(&over, None).is_err());
        assert_eq!(svc.mcp_servers().unwrap().len(), 1);
    }

    #[test]
    fn mcp_count_and_unicode_name_bounds_are_exact() {
        let (_dir, svc) = service();
        for i in 0..MAX_MCP_SERVERS {
            svc.save_mcp_server(&mcp(&format!("s{i}")), None).unwrap();
        }
        assert!(svc.save_mcp_server(&mcp("overflow"), None).is_err());
        let mut unicode = mcp("é".repeat(64).as_str());
        unicode.command = "x".repeat(MAX_MCP_COMMAND_BYTES + 1);
        assert!(svc.save_mcp_server(&unicode, None).is_err());
    }

    #[test]
    fn mcp_remote_endpoint_and_id_are_hostile_input_safe() {
        let (_dir, svc) = service();
        let remote = |endpoint: &str| {
            let mut draft = mcp("remote");
            draft.command = String::new();
            draft.args = vec![];
            draft.transport = Some("streamable_http".into());
            draft.endpoint = Some(endpoint.into());
            draft
        };
        assert!(svc
            .save_mcp_server(&remote("https://mcp.example.com/rpc"), None)
            .is_ok());
        for hostile in [
            "https://user:pw@mcp.example.com/rpc",
            "https://mcp.example.com/rpc?api_key=sk-secret",
            "https://mcp.example.com/rpc#token",
            "https://127.0.0.1/rpc",
            "https://169.254.169.254/rpc",
            "https://[::1]/rpc",
            "https://[::ffff:169.254.169.254]/rpc",
            "https://[64:ff9b::a9fe:a9fe]/rpc",
            "https://[2002:a9fe:a9fe::1]/rpc",
            "https://localhost/rpc",
            "https://mcp.localhost/rpc",
            "ftp://mcp.example.com/rpc",
        ] {
            assert!(
                svc.save_mcp_server(&remote(hostile), None).is_err(),
                "{hostile} must be refused"
            );
        }
        // The id is a served key: traversal/control bytes cannot cross it.
        for id in ["../evil", "a/b", "a\\b", "a b", "a\nb", "é"] {
            let mut draft = mcp("id");
            draft.id = Some(id.into());
            assert!(
                svc.save_mcp_server(&draft, None).is_err(),
                "id {id:?} must be refused"
            );
        }
    }

    #[test]
    fn project_skill_install_is_untrusted_and_invocation_needs_grants() {
        let (_dir, svc) = service();
        let draft = SkillDraft {
            name: "review".into(),
            origin: Some(ORIGIN_PROJECT.into()),
            source_uri: ".faktor/skills/review/SKILL.md".into(),
            text: "# Review\nBe careful.".into(),
            trusted: None,
            invocation: Some("agent_selectable".into()),
            required_capabilities: vec!["read".into(), "execute".into()],
        };
        let record = svc.save_skill(&draft, None).unwrap();
        assert!(!record.trusted, "installation is not authorization");
        let granted = BTreeSet::from(["read".to_string(), "execute".to_string()]);
        assert!(svc.may_invoke(&record, true, &granted).is_err());
        let trusted = svc.trust_skill(&record.id, true).unwrap();
        assert!(svc.may_invoke(&trusted, true, &granted).is_ok());
        let mut missing = granted.clone();
        missing.remove("execute");
        assert!(svc
            .may_invoke(&trusted, true, &missing)
            .unwrap_err()
            .to_string()
            .contains("execute"));
        let updated = svc.save_skill(&draft, Some(record.version)).unwrap();
        assert_eq!(updated.version, 2);
        assert!(svc.save_skill(&draft, Some(record.version)).is_err());
    }

    #[test]
    fn disabled_skill_is_never_agent_invocable_and_hooks_are_bounded() {
        let (_dir, svc) = service();
        let record = svc
            .save_skill(
                &SkillDraft {
                    name: "off".into(),
                    origin: None,
                    source_uri: "skill://off".into(),
                    text: "x".into(),
                    trusted: Some(true),
                    invocation: Some("disabled".into()),
                    required_capabilities: vec![],
                },
                None,
            )
            .unwrap();
        assert!(svc.may_invoke(&record, true, &BTreeSet::new()).is_err());
        let saved = svc
            .save_hook(
                &HookDraft {
                    id: "h1".into(),
                    spec: serde_json::json!({"events":["pre_tool"]}),
                    trusted: None,
                    enabled: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(saved.revision, 1);
        assert!(svc
            .save_hook(
                &HookDraft {
                    id: "h1".into(),
                    spec: serde_json::json!({"events":["pre_tool"]}),
                    trusted: None,
                    enabled: None,
                },
                Some(0)
            )
            .is_err());
        svc.delete_hook("h1").unwrap();
        assert!(svc.hooks().unwrap().is_empty());
    }
}
