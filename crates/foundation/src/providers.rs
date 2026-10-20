//! BK02 provider lifecycle: stable connection ids, validated family/endpoint/
//! credential references, bounded staged probes (reachability, authentication,
//! model listing, capability) and truthful health.
//!
//! A saved connection is NOT "connected": health is only `Ready` after an
//! observed handshake/model listing, authentication cancellation is never
//! Ready, and a loopback opt-in never authorizes a private LAN destination.

use std::time::{Duration, Instant};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{now_ms, FoundationDb};
use crate::vault::SecretVault;
use crate::FoundationError;

pub const MAX_CONNECTIONS: usize = 64;
pub const MAX_MODELS_PER_CONNECTION: usize = 1_024;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_ENDPOINT_BYTES: usize = 2_048;
pub const MAX_PROBE_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const PROBE_TIMEOUT_MS: u64 = 3_000;
/// Transport-duration bounds (P-11): a positive deadline within the
/// adapters' supported range.
pub const MIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const MAX_KEEP_ALIVE_BYTES: usize = 64;
pub const MAX_CA_PEM_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFamily {
    Ollama,
    OpenAi,
    Anthropic,
    Google,
    DeepSeek,
    Gateway,
}

impl ProviderFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderFamily::Ollama => "ollama",
            ProviderFamily::OpenAi => "openai",
            ProviderFamily::Anthropic => "anthropic",
            ProviderFamily::Google => "google",
            ProviderFamily::DeepSeek => "deepseek",
            ProviderFamily::Gateway => "gateway",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "ollama" => ProviderFamily::Ollama,
            "openai" | "open_ai" => ProviderFamily::OpenAi,
            "anthropic" => ProviderFamily::Anthropic,
            "google" => ProviderFamily::Google,
            "deepseek" => ProviderFamily::DeepSeek,
            "gateway" => ProviderFamily::Gateway,
            _ => return None,
        })
    }

    fn default_endpoint(self) -> Option<&'static str> {
        match self {
            ProviderFamily::Ollama => Some("http://127.0.0.1:11434"),
            ProviderFamily::OpenAi => Some("https://api.openai.com/v1"),
            ProviderFamily::Anthropic => Some("https://api.anthropic.com"),
            ProviderFamily::Google => None,
            ProviderFamily::DeepSeek => Some("https://api.deepseek.com/v1"),
            ProviderFamily::Gateway => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConnection {
    pub id: String,
    pub family: ProviderFamily,
    pub label: String,
    pub endpoint: Option<String>,
    pub credential_ref: Option<String>,
    pub models: Vec<String>,
    pub allow_loopback: bool,
    pub api_flavor: Option<String>,
    pub trust_origin: String,
    pub revision: u64,
    /// `restart_required` on save in this daemon generation; never a fake
    /// "effective".
    pub apply_state: String,
    /// P-11: connect / first-byte / total deadlines. Absent = the adapter
    /// policy. These are persisted and used by the live probe; the runtime
    /// provider graph acknowledges them at boot (`apply_state`).
    pub connect_timeout_ms: Option<u64>,
    pub first_byte_timeout_ms: Option<u64>,
    pub total_timeout_ms: Option<u64>,
    /// P-15: adapter keep-alive (Ollama residency) or a TCP idle duration.
    pub keep_alive: Option<String>,
    /// P-12: one explicitly configured proxy (already validated: no
    /// userinfo, http(s), allowlisted as a destination for the probe).
    pub proxy_url: Option<String>,
    /// P-12: `system` or `pinned_ca` (never an insecure-verification mode).
    pub tls_mode: String,
    /// P-12: opaque `vault://` reference to the added PEM trust root; the
    /// certificate bytes never leave the vault.
    pub ca_cert_ref: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Stopped,
    Connecting,
    /// The endpoint answered the discovery handshake AND returned a
    /// recognized, non-empty model list. Never set optimistically.
    Ready,
    AuthenticationRequired,
    Failed,
    Stale,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthState::Stopped => "stopped",
            HealthState::Connecting => "connecting",
            HealthState::Ready => "ready",
            HealthState::AuthenticationRequired => "authentication_required",
            HealthState::Failed => "failed",
            HealthState::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeStage {
    /// `reachability` | `authentication` | `model_list` | `capability`
    pub stage: String,
    /// `ok` | `failed` | `skipped` | `unsupported`
    pub state: String,
    pub detail: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub connection_id: String,
    pub state: HealthState,
    pub stages: Vec<ProbeStage>,
    pub observed_ms: i64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionDraft {
    pub id: String,
    pub family: String,
    pub label: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub allow_loopback: Option<bool>,
    #[serde(default)]
    pub api_flavor: Option<String>,
    #[serde(default)]
    pub trust_origin: Option<String>,
    /// P-11 transport deadlines (milliseconds, 100..=600000).
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub first_byte_timeout_ms: Option<u64>,
    #[serde(default)]
    pub total_timeout_ms: Option<u64>,
    /// P-15 adapter keep-alive (`forever`, `-1`, or duration like `30m`).
    #[serde(default)]
    pub keep_alive: Option<String>,
    /// P-12 managed proxy URL (http/https, no userinfo).
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// P-12 `system` or `pinned_ca`.
    #[serde(default)]
    pub tls_mode: Option<String>,
    /// P-12 opaque `vault://` reference to a PEM CA certificate.
    #[serde(default)]
    pub ca_cert_ref: Option<String>,
}

struct ValidatedTransport {
    connect_timeout_ms: Option<u64>,
    first_byte_timeout_ms: Option<u64>,
    total_timeout_ms: Option<u64>,
    keep_alive: Option<String>,
    proxy_url: Option<String>,
    tls_mode: String,
    ca_cert_ref: Option<String>,
}

pub struct ProviderService {
    db: FoundationDb,
    vault: SecretVault,
}

impl ProviderService {
    pub fn new(db: FoundationDb, vault: SecretVault) -> Self {
        Self { db, vault }
    }

    pub fn list(&self) -> Result<Vec<ProviderConnection>, FoundationError> {
        self.db.read("provider list", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, family, label, endpoint, credential_ref, models_json,
                            allow_loopback, api_flavor, trust_origin, revision,
                            apply_state, connect_timeout_ms, first_byte_timeout_ms,
                            total_timeout_ms, keep_alive, proxy_url, tls_mode,
                            ca_cert_ref, created_ms, updated_ms
                     FROM provider_connection ORDER BY created_ms ASC, id ASC LIMIT ?1",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_CONNECTIONS as i64], map_connection)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<ProviderConnection>, FoundationError> {
        self.db.read("provider get", |conn| {
            conn.query_row(
                "SELECT id, family, label, endpoint, credential_ref, models_json,
                        allow_loopback, api_flavor, trust_origin, revision,
                        apply_state, connect_timeout_ms, first_byte_timeout_ms,
                        total_timeout_ms, keep_alive, proxy_url, tls_mode,
                        ca_cert_ref, created_ms, updated_ms
                 FROM provider_connection WHERE id = ?1",
                params![id],
                map_connection,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Acknowledge at boot that this connection's adapter was materialized
    /// into the runtime provider graph. The row stays durable; its apply
    /// state becomes `applied` (F037/F042). Unknown ids are a typed NotFound.
    pub fn mark_applied(&self, id: &str) -> Result<(), FoundationError> {
        self.db.write("provider apply", |conn| {
            let changed = conn
                .execute(
                    "UPDATE provider_connection SET apply_state = 'applied', updated_ms = ?2
                     WHERE id = ?1",
                    params![id, now_ms()],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!(
                    "provider connection {id}"
                )));
            }
            Ok(())
        })
    }

    /// Save one validated connection. Saving NEVER starts a probe and NEVER
    /// claims `Ready`: the apply state is `restart_required` because this
    /// daemon's provider graph is built at boot.
    pub fn save(
        &self,
        draft: &ConnectionDraft,
        expected_revision: Option<u64>,
    ) -> Result<ProviderConnection, FoundationError> {
        let family = ProviderFamily::parse(&draft.family).ok_or_else(|| {
            FoundationError::Validation(vec![crate::config::FieldError {
                key: "family".into(),
                scope: None,
                code: "invalid_value".into(),
                message: format!(
                    "unknown provider family {:?} (ollama, openai, anthropic, google, deepseek, gateway)",
                    draft.family
                ),
            }])
        })?;
        validate_id(&draft.id)?;
        if draft.label.is_empty() || draft.label.len() > 128 {
            return Err(FoundationError::Malformed(
                "connection label must be 1..=128 bytes".into(),
            ));
        }
        if crate::contains_secret_material(&draft.label) {
            return Err(FoundationError::Malformed(
                "connection label must not contain secret material; store the value in the vault"
                    .into(),
            ));
        }
        if draft.models.len() > MAX_MODELS_PER_CONNECTION {
            return Err(FoundationError::Oversized(format!(
                "at most {MAX_MODELS_PER_CONNECTION} model ids per connection"
            )));
        }
        for model in &draft.models {
            if model.is_empty() || model.len() > 256 {
                return Err(FoundationError::Malformed(
                    "model ids must be 1..=256 bytes".into(),
                ));
            }
        }
        let endpoint = draft
            .endpoint
            .clone()
            .or_else(|| family.default_endpoint().map(str::to_string));
        let loopback_default = family == ProviderFamily::Ollama;
        let allow_loopback = draft.allow_loopback.unwrap_or(loopback_default);
        if let Some(endpoint) = &endpoint {
            validate_endpoint(endpoint, allow_loopback, &draft.id)?;
        } else if family != ProviderFamily::Google && family != ProviderFamily::Gateway {
            return Err(FoundationError::Malformed(
                "this family requires an explicit base URL".into(),
            ));
        }
        if let Some(reference) = &draft.credential_ref {
            if !crate::config::valid_secret_reference(reference) {
                return Err(FoundationError::Malformed(
                    "credential_ref must be an opaque vault:// or env: reference".into(),
                ));
            }
            if let Some(rest) = reference.strip_prefix("vault://") {
                let handle = format!("vault://{rest}");
                self.vault.get(&handle).map_err(|_| {
                    FoundationError::Malformed(
                        "credential_ref does not resolve in this machine's vault".into(),
                    )
                })?;
            }
        }
        let transport = self.validate_transport(draft, family)?;
        let now = now_ms();
        self.db.write("provider save", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin save: {e}")))?;
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT revision, family FROM provider_connection WHERE id = ?1",
                    params![draft.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some((revision, _)) = &existing {
                if let Some(expected) = expected_revision {
                    if (*revision).max(0) as u64 != expected {
                        return Err(FoundationError::Conflict(format!(
                            "connection {} revision changed: expected {expected} but found {revision}",
                            draft.id
                        )));
                    }
                }
            } else {
                let count: i64 = tx
                    .query_row("SELECT COUNT(*) FROM provider_connection", [], |r| r.get(0))
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                if count as usize >= MAX_CONNECTIONS {
                    return Err(FoundationError::Oversized(format!(
                        "at most {MAX_CONNECTIONS} provider connections"
                    )));
                }
            }
            let revision = existing.map(|(r, _)| r.max(0) as u64 + 1).unwrap_or(1);
            let models = serde_json::to_string(&draft.models)
                .map_err(|e| FoundationError::Store(format!("encode models: {e}")))?;
            let trust = draft
                .trust_origin
                .clone()
                .unwrap_or_else(|| "user".into());
            if trust != "user" && trust != "machine" && trust != "managed" {
                return Err(FoundationError::Malformed(
                    "trust_origin must be user, machine or managed".into(),
                ));
            }
            tx.execute(
                "INSERT INTO provider_connection(
                   id, family, label, endpoint, credential_ref, models_json,
                   allow_loopback, api_flavor, created_ms, updated_ms, revision,
                   apply_state, trust_origin, connect_timeout_ms,
                   first_byte_timeout_ms, total_timeout_ms, keep_alive,
                   proxy_url, tls_mode, ca_cert_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                   'restart_required', ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(id) DO UPDATE SET
                   family = excluded.family,
                   label = excluded.label,
                   endpoint = excluded.endpoint,
                   credential_ref = excluded.credential_ref,
                   models_json = excluded.models_json,
                   allow_loopback = excluded.allow_loopback,
                   api_flavor = excluded.api_flavor,
                   updated_ms = excluded.updated_ms,
                   revision = excluded.revision,
                   apply_state = 'restart_required',
                   trust_origin = excluded.trust_origin,
                   connect_timeout_ms = excluded.connect_timeout_ms,
                   first_byte_timeout_ms = excluded.first_byte_timeout_ms,
                   total_timeout_ms = excluded.total_timeout_ms,
                   keep_alive = excluded.keep_alive,
                   proxy_url = excluded.proxy_url,
                   tls_mode = excluded.tls_mode,
                   ca_cert_ref = excluded.ca_cert_ref",
                params![
                    draft.id,
                    family.as_str(),
                    draft.label,
                    endpoint,
                    draft.credential_ref,
                    models,
                    if allow_loopback { 1 } else { 0 },
                    draft.api_flavor,
                    now,
                    now,
                    revision as i64,
                    trust,
                    transport.connect_timeout_ms.map(|v| v as i64),
                    transport.first_byte_timeout_ms.map(|v| v as i64),
                    transport.total_timeout_ms.map(|v| v as i64),
                    transport.keep_alive,
                    transport.proxy_url,
                    transport.tls_mode,
                    transport.ca_cert_ref
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            // Saving a connection invalidates any prior observed health:
            // the new endpoint/credential has not been probed yet.
            tx.execute(
                "DELETE FROM provider_health WHERE connection_id = ?1",
                params![draft.id],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let connection = tx
                .query_row(
                    "SELECT id, family, label, endpoint, credential_ref, models_json,
                            allow_loopback, api_flavor, trust_origin, revision,
                            apply_state, connect_timeout_ms, first_byte_timeout_ms,
                            total_timeout_ms, keep_alive, proxy_url, tls_mode,
                            ca_cert_ref, created_ms, updated_ms
                     FROM provider_connection WHERE id = ?1",
                    params![draft.id],
                    map_connection,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit save: {e}")))?;
            Ok(connection)
        })
    }

    /// Validate and normalize the P-11/P-12/P-15 transport options. The
    /// returned values are the ONLY shape persisted; a TLS verification
    /// toggle is structurally absent (only `system` or an ADDED pinned CA).
    fn validate_transport(
        &self,
        draft: &ConnectionDraft,
        family: ProviderFamily,
    ) -> Result<ValidatedTransport, FoundationError> {
        let timeout = |value: Option<u64>, field: &str| -> Result<Option<u64>, FoundationError> {
            match value {
                None => Ok(None),
                Some(v) if (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&v) => Ok(Some(v)),
                Some(v) => Err(FoundationError::Validation(vec![
                    crate::config::FieldError {
                        key: field.into(),
                        scope: None,
                        code: "out_of_range".into(),
                        message: format!(
                            "{field} must be {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS} ms, got {v}"
                        ),
                    },
                ])),
            }
        };
        let connect_timeout_ms = timeout(draft.connect_timeout_ms, "connect_timeout_ms")?;
        let first_byte_timeout_ms = timeout(draft.first_byte_timeout_ms, "first_byte_timeout_ms")?;
        let total_timeout_ms = timeout(draft.total_timeout_ms, "total_timeout_ms")?;
        if let (Some(first), Some(total)) = (first_byte_timeout_ms, total_timeout_ms) {
            if first > total {
                return Err(FoundationError::Validation(vec![
                    crate::config::FieldError {
                        key: "first_byte_timeout_ms".into(),
                        scope: None,
                        code: "out_of_range".into(),
                        message: "first_byte_timeout_ms cannot exceed total_timeout_ms".into(),
                    },
                ]));
            }
        }
        let keep_alive = match draft.keep_alive.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(value) => {
                if family != ProviderFamily::Ollama {
                    return Err(FoundationError::Validation(vec![
                        crate::config::FieldError {
                            key: "keep_alive".into(),
                            scope: None,
                            code: "unsupported".into(),
                            message: "keep_alive is an Ollama adapter control only".into(),
                        },
                    ]));
                }
                if value.len() > MAX_KEEP_ALIVE_BYTES || !valid_keep_alive(value) {
                    return Err(FoundationError::Validation(vec![
                        crate::config::FieldError {
                            key: "keep_alive".into(),
                            scope: None,
                            code: "invalid_value".into(),
                            message: "keep_alive must be forever, -1, 0 or a duration like 30m"
                                .into(),
                        },
                    ]));
                }
                Some(value.to_string())
            }
        };
        let proxy_url = match draft.proxy_url.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => {
                let parsed = url::Url::parse(raw).map_err(|_| {
                    FoundationError::Validation(vec![crate::config::FieldError {
                        key: "proxy_url".into(),
                        scope: None,
                        code: "invalid_url".into(),
                        message: "proxy_url must be an absolute http(s) URL".into(),
                    }])
                })?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                    || crate::contains_secret_material(raw)
                {
                    return Err(FoundationError::Validation(vec![crate::config::FieldError {
                        key: "proxy_url".into(),
                        scope: None,
                        code: "invalid_url".into(),
                        message: "proxy_url must be http(s) with a host and no credentials, query or fragment"
                            .into(),
                    }]));
                }
                Some(parsed.to_string())
            }
        };
        let tls_mode = draft.tls_mode.as_deref().unwrap_or("system");
        if tls_mode != "system" && tls_mode != "pinned_ca" {
            return Err(FoundationError::Validation(vec![crate::config::FieldError {
                key: "tls_mode".into(),
                scope: None,
                code: "invalid_value".into(),
                message: "tls_mode must be system or pinned_ca (insecure verification is not a setting)"
                    .into(),
            }]));
        }
        let ca_cert_ref = match (tls_mode, draft.ca_cert_ref.as_deref().map(str::trim)) {
            ("system", None) | ("system", Some("")) => None,
            ("system", Some(_)) => {
                return Err(FoundationError::Validation(vec![
                    crate::config::FieldError {
                        key: "ca_cert_ref".into(),
                        scope: None,
                        code: "invalid_value".into(),
                        message: "ca_cert_ref is only valid with tls_mode=pinned_ca".into(),
                    },
                ]))
            }
            ("pinned_ca", Some(reference)) if reference.starts_with("vault://") => {
                let pem = self.vault.reveal(reference).map_err(|_| {
                    FoundationError::Validation(vec![crate::config::FieldError {
                        key: "ca_cert_ref".into(),
                        scope: None,
                        code: "unresolved_reference".into(),
                        message: "ca_cert_ref does not resolve in this machine's vault".into(),
                    }])
                })?;
                if pem.len() > MAX_CA_PEM_BYTES
                    || !pem.windows(27).any(|w| w == b"-----BEGIN CERTIFICATE-----")
                {
                    return Err(FoundationError::Validation(vec![
                        crate::config::FieldError {
                            key: "ca_cert_ref".into(),
                            scope: None,
                            code: "invalid_value".into(),
                            message: "ca_cert_ref must hold a bounded PEM certificate".into(),
                        },
                    ]));
                }
                Some(reference.to_string())
            }
            _ => {
                return Err(FoundationError::Validation(vec![
                    crate::config::FieldError {
                        key: "ca_cert_ref".into(),
                        scope: None,
                        code: "missing".into(),
                        message: "tls_mode=pinned_ca requires a vault:// ca_cert_ref".into(),
                    },
                ]))
            }
        };
        Ok(ValidatedTransport {
            connect_timeout_ms,
            first_byte_timeout_ms,
            total_timeout_ms,
            keep_alive,
            proxy_url,
            tls_mode: tls_mode.to_string(),
            ca_cert_ref,
        })
    }

    /// Delete a connection record and its health. Local deletion is not a
    /// remote revocation; the caller must not claim remote state changed.
    pub fn delete(&self, id: &str) -> Result<(), FoundationError> {
        self.db.write("provider delete", |conn| {
            let changed = conn
                .execute("DELETE FROM provider_connection WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!(
                    "provider connection {id}"
                )));
            }
            conn.execute(
                "DELETE FROM provider_health WHERE connection_id = ?1",
                params![id],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(())
        })
    }

    pub fn health(&self, id: &str) -> Result<Option<ProviderHealth>, FoundationError> {
        self.db.read("provider health", |conn| {
            conn.query_row(
                "SELECT connection_id, state, stages_json, observed_ms, detail
                 FROM provider_health WHERE connection_id = ?1",
                params![id],
                map_health,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Every persisted health row in ONE read (diagnostics used to open one
    /// reader connection and run one point query per saved connection).
    pub fn health_all(
        &self,
    ) -> Result<std::collections::HashMap<String, ProviderHealth>, FoundationError> {
        self.db.read("provider health all", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT connection_id, state, stages_json, observed_ms, detail
                     FROM provider_health",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map([], map_health)
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let mut out = std::collections::HashMap::new();
            for row in rows {
                let health = row.map_err(|e| FoundationError::Store(e.to_string()))?;
                out.insert(health.connection_id.clone(), health);
            }
            Ok(out)
        })
    }

    /// Run the bounded staged probe and persist the observed health. The
    /// stages are independent: a failed authentication is NOT Ready, a
    /// failed discovery keeps manually declared model ids, and remote/OAuth
    /// transports are reported `unsupported`, never offered as idle.
    pub async fn probe(&self, id: &str) -> Result<ProviderHealth, FoundationError> {
        let connection = self
            .get(id)?
            .ok_or_else(|| FoundationError::NotFound(format!("provider connection {id}")))?;
        let credential = match &connection.credential_ref {
            Some(reference) if reference.starts_with("vault://") => {
                Some(self.vault.reveal(reference)?)
            }
            Some(reference) => std::env::var(reference.trim_start_matches("env:"))
                .ok()
                .map(|v| v.into_bytes()),
            None => None,
        };
        let mut stages = Vec::new();
        let state: HealthState;
        let mut detail = None;
        let base = connection
            .endpoint
            .clone()
            .unwrap_or_else(|| "http://127.0.0.1:11434".into());
        // The probe runs through the daemon's checked egress transport:
        // the configured endpoint is the ONLY permitted destination, and the
        // central resolver refuses private/link-local/metadata answers. A
        // loopback endpoint is permitted only when this connection carries
        // the explicit allow_loopback flag.
        let parsed = match url::Url::parse(&base) {
            Ok(parsed) => parsed,
            Err(e) => {
                let health = ProviderHealth {
                    connection_id: connection.id.clone(),
                    state: HealthState::Failed,
                    stages: vec![ProbeStage {
                        stage: "reachability".into(),
                        state: "failed".into(),
                        detail: format!("endpoint is not a URL: {e}"),
                        duration_ms: 0,
                    }],
                    observed_ms: now_ms(),
                    detail: Some("probe endpoint invalid".into()),
                };
                self.record_health(&health)?;
                return Ok(health);
            }
        };
        let Some(host) = parsed.host_str() else {
            let health = ProviderHealth {
                connection_id: connection.id.clone(),
                state: HealthState::Failed,
                stages: vec![ProbeStage {
                    stage: "reachability".into(),
                    state: "failed".into(),
                    detail: "endpoint has no host".into(),
                    duration_ms: 0,
                }],
                observed_ms: now_ms(),
                detail: Some("probe endpoint invalid".into()),
            };
            self.record_health(&health)?;
            return Ok(health);
        };
        let port = parsed
            .port_or_known_default()
            .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
        let rule = format!("{}://{host}:{port}", parsed.scheme());
        // P-12: an explicitly configured proxy is itself a vetted
        // destination. It is allowlisted alongside the endpoint, and the
        // checked client refuses a proxy that does not pass the same parsed
        // policy + address-class gate before it is ever dialed.
        let proxy = match connection.proxy_url.as_deref() {
            Some(raw) => match url::Url::parse(raw) {
                Ok(proxy) => match (proxy.host_str(), proxy.port_or_known_default()) {
                    (Some(proxy_host), Some(proxy_port)) => Some((
                        format!("{}://{proxy_host}:{proxy_port}", proxy.scheme()),
                        proxy,
                    )),
                    _ => None,
                },
                Err(_) => None,
            },
            None => None,
        };
        let mut rules = vec![rule];
        if let Some((proxy_rule, _)) = &proxy {
            rules.push(proxy_rule.clone());
        }
        let policy = match faktor_security::destination::DestinationPolicy::parse_lines(
            rules.iter().map(String::as_str),
        ) {
            Ok(policy) => policy,
            Err(e) => {
                let health = ProviderHealth {
                    connection_id: connection.id.clone(),
                    state: HealthState::Failed,
                    stages: vec![ProbeStage {
                        stage: "reachability".into(),
                        state: "failed".into(),
                        detail: format!("destination policy for the endpoint is invalid: {e}"),
                        duration_ms: 0,
                    }],
                    observed_ms: now_ms(),
                    detail: Some("probe destination refused".into()),
                };
                self.record_health(&health)?;
                return Ok(health);
            }
        };
        let ca_pem = match connection.ca_cert_ref.as_deref() {
            Some(reference) => self.vault.reveal(reference).ok(),
            None => None,
        };
        let options = faktor_provider::egress::EgressClientOptions {
            connect_timeout: connection
                .connect_timeout_ms
                .map(Duration::from_millis)
                .or(Some(Duration::from_millis(PROBE_TIMEOUT_MS.min(10_000)))),
            proxy: proxy.as_ref().map(|(_, parsed)| parsed.clone()),
            extra_root_pem: ca_pem,
            tcp_keep_alive: connection
                .keep_alive
                .as_deref()
                .and_then(parse_tcp_keep_alive),
        };
        let client = match faktor_provider::egress::CheckedHttpClient::try_with_policy_scan_addresses_and_options(
            policy,
            None,
            faktor_provider::egress::EgressAddressPolicy::from_allow_loopback(
                connection.allow_loopback,
            ),
            options,
        ) {
                Ok(client) => client,
                Err(e) => {
                    let health = ProviderHealth {
                        connection_id: connection.id.clone(),
                        state: HealthState::Failed,
                        stages: vec![ProbeStage {
                            stage: "reachability".into(),
                            state: "failed".into(),
                            detail: format!("checked transport construction failed: {e}"),
                            duration_ms: 0,
                        }],
                        observed_ms: now_ms(),
                        detail: Some("probe client unavailable".into()),
                    };
                    self.record_health(&health)?;
                    return Ok(health);
                }
            };
        // Reachability: a bounded GET of the family's discovery surface.
        let (path, auth_header) = match connection.family {
            ProviderFamily::Ollama => ("/api/tags", None),
            ProviderFamily::OpenAi | ProviderFamily::DeepSeek | ProviderFamily::Gateway => (
                "/models",
                credential.as_ref().map(|c| {
                    (
                        "authorization",
                        format!("Bearer {}", String::from_utf8_lossy(c)),
                    )
                }),
            ),
            ProviderFamily::Anthropic => (
                "/v1/models",
                credential
                    .as_ref()
                    .map(|c| ("x-api-key", String::from_utf8_lossy(c).into_owned())),
            ),
            ProviderFamily::Google => {
                let health = ProviderHealth {
                    connection_id: connection.id.clone(),
                    state: HealthState::Stopped,
                    stages: vec![ProbeStage {
                        stage: "reachability".into(),
                        state: "unsupported".into(),
                        detail: "this daemon build has no Google discovery adapter".into(),
                        duration_ms: 0,
                    }],
                    observed_ms: now_ms(),
                    detail: Some("probe unsupported for this family".into()),
                };
                self.record_health(&health)?;
                return Ok(health);
            }
        };
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        let started = Instant::now();
        let mut request = match client.get(&url) {
            Ok(request) => request,
            Err(e) => {
                let health = ProviderHealth {
                    connection_id: connection.id.clone(),
                    state: HealthState::Failed,
                    stages: vec![ProbeStage {
                        stage: "reachability".into(),
                        state: "failed".into(),
                        detail: format!("probe request refused: {e}"),
                        duration_ms: 0,
                    }],
                    observed_ms: now_ms(),
                    detail: Some("probe destination refused".into()),
                };
                self.record_health(&health)?;
                return Ok(health);
            }
        };
        if let Some((name, value)) = auth_header {
            request = if name == "authorization" {
                request.header("authorization", value)
            } else {
                request
                    .header(name, value)
                    .header("anthropic-version", "2023-06-01")
            };
        }
        let total_timeout = connection
            .total_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(PROBE_TIMEOUT_MS));
        let response = match tokio::time::timeout(total_timeout, client.send_checked(request)).await
        {
            Ok(response) => response,
            Err(_) => {
                let health = ProviderHealth {
                    connection_id: connection.id.clone(),
                    state: HealthState::Failed,
                    stages: vec![ProbeStage {
                        stage: "reachability".into(),
                        state: "failed".into(),
                        detail: format!(
                            "the configured total deadline ({} ms) elapsed before a response head",
                            total_timeout.as_millis()
                        ),
                        duration_ms: started.elapsed().as_millis() as u64,
                    }],
                    observed_ms: now_ms(),
                    detail: Some("probe timed out".into()),
                };
                self.record_health(&health)?;
                return Ok(health);
            }
        };
        let elapsed = started.elapsed().as_millis() as u64;
        let mut model_ids: Vec<String> = Vec::new();
        match response {
            Ok(response) => {
                let status = response.status().as_u16();
                if status == 401 || status == 403 {
                    stages.push(ProbeStage {
                        stage: "reachability".into(),
                        state: "ok".into(),
                        detail: format!("endpoint answered HTTP {status}"),
                        duration_ms: elapsed,
                    });
                    stages.push(ProbeStage {
                        stage: "authentication".into(),
                        state: "failed".into(),
                        detail: "the endpoint rejected the credential".into(),
                        duration_ms: elapsed,
                    });
                    state = HealthState::AuthenticationRequired;
                    detail = Some("authentication failed".into());
                    if let Some(reference) = &connection.credential_ref {
                        if reference.starts_with("vault://") {
                            let _ = self.vault.mark_verified(reference, false);
                        }
                    }
                } else if !(200..300).contains(&status) {
                    stages.push(ProbeStage {
                        stage: "reachability".into(),
                        state: "failed".into(),
                        detail: format!("endpoint answered HTTP {status}"),
                        duration_ms: elapsed,
                    });
                    state = HealthState::Failed;
                    detail = Some(format!("discovery failed with HTTP {status}"));
                } else {
                    stages.push(ProbeStage {
                        stage: "reachability".into(),
                        state: "ok".into(),
                        detail: format!("endpoint answered HTTP {status}"),
                        duration_ms: elapsed,
                    });
                    stages.push(ProbeStage {
                        stage: "authentication".into(),
                        state: "ok".into(),
                        detail: if connection.credential_ref.is_some() {
                            "credential accepted".into()
                        } else {
                            "no credential configured".into()
                        },
                        duration_ms: elapsed,
                    });
                    // Bounded body read through the checked transport.
                    let first_byte = connection
                        .first_byte_timeout_ms
                        .map(Duration::from_millis)
                        .unwrap_or(total_timeout);
                    let body_deadline = total_timeout.saturating_sub(started.elapsed());
                    let verification = match response
                        .read_bytes(&faktor_provider::egress::ResponseBudget::new(
                            first_byte,
                            first_byte,
                            body_deadline.max(Duration::from_millis(1)),
                            MAX_PROBE_BODY_BYTES as u64,
                            None,
                        ))
                        .await
                    {
                        Ok(bytes) => verify_model_list(connection.family, &bytes),
                        Err(e) => {
                            ModelListVerification::Failed(ModelListFailure::BodyRead(e.to_string()))
                        }
                    };
                    match verification {
                        ModelListVerification::Verified(models) => {
                            model_ids = models;
                            stages.push(ProbeStage {
                                stage: "model_list".into(),
                                state: "ok".into(),
                                detail: format!("{} models listed", model_ids.len()),
                                duration_ms: started.elapsed().as_millis() as u64,
                            });
                            if let Some(reference) = &connection.credential_ref {
                                if reference.starts_with("vault://") {
                                    let _ = self.vault.mark_verified(reference, true);
                                }
                            }
                            state = HealthState::Ready;
                        }
                        ModelListVerification::Failed(failure) => {
                            stages.push(ProbeStage {
                                stage: "model_list".into(),
                                state: "failed".into(),
                                detail: failure.detail().to_string(),
                                duration_ms: started.elapsed().as_millis() as u64,
                            });
                            state = HealthState::Failed;
                            detail = Some("model list verification failed".into());
                        }
                    }
                    stages.push(ProbeStage {
                        stage: "capability".into(),
                        state: "skipped".into(),
                        detail:
                            "exact capability probing requires an adapter-specific probe; model ids are not treated as certified capabilities"
                                .into(),
                        duration_ms: 0,
                    });
                }
            }
            Err(e) => {
                stages.push(ProbeStage {
                    stage: "reachability".into(),
                    state: "failed".into(),
                    detail: format!("endpoint unreachable: {e}"),
                    duration_ms: elapsed,
                });
                state = HealthState::Failed;
                detail = Some("endpoint unreachable".into());
            }
        }
        let health = ProviderHealth {
            connection_id: connection.id.clone(),
            state,
            stages,
            observed_ms: now_ms(),
            detail,
        };
        self.record_health(&health)?;
        // Discovery may extend the declared model ids; keep manually declared
        // ids when discovery fails (never clear the prior valid set).
        if !model_ids.is_empty() {
            self.merge_models(&connection.id, &model_ids)?;
        }
        Ok(health)
    }

    pub fn record_health(&self, health: &ProviderHealth) -> Result<(), FoundationError> {
        let stages = serde_json::to_string(&health.stages)
            .map_err(|e| FoundationError::Store(format!("encode stages: {e}")))?;
        self.db.write("provider health save", |conn| {
            conn.execute(
                "INSERT INTO provider_health(connection_id, state, stages_json, observed_ms, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(connection_id) DO UPDATE SET
                   state = excluded.state,
                   stages_json = excluded.stages_json,
                   observed_ms = excluded.observed_ms,
                   detail = excluded.detail",
                params![
                    health.connection_id,
                    health.state.as_str(),
                    stages,
                    health.observed_ms,
                    health.detail
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(())
        })
    }

    fn merge_models(&self, id: &str, discovered: &[String]) -> Result<(), FoundationError> {
        self.db.write("provider models merge", |conn| {
            let raw: String = conn
                .query_row(
                    "SELECT models_json FROM provider_connection WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let mut models: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
            let mut seen: std::collections::BTreeSet<String> = models.iter().cloned().collect();
            for model in discovered {
                if seen.insert(model.clone()) {
                    models.push(model.clone());
                }
                if models.len() >= MAX_MODELS_PER_CONNECTION {
                    break;
                }
            }
            let encoded = serde_json::to_string(&models)
                .map_err(|e| FoundationError::Store(format!("encode models: {e}")))?;
            conn.execute(
                "UPDATE provider_connection SET models_json = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id, encoded, now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(())
        })
    }
}

/// The typed outcome of verifying a discovery response. `Ready` is only
/// reachable from [`ModelListVerification::Verified`]; every read, parse,
/// schema or emptiness failure stays explicitly unverified.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelListVerification {
    Verified(Vec<String>),
    Failed(ModelListFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelListFailure {
    BodyRead(String),
    EmptyBody,
    InvalidJson,
    WrongSchema,
    EmptyList,
}

impl ModelListFailure {
    fn detail(&self) -> &str {
        match self {
            ModelListFailure::BodyRead(detail) => detail,
            ModelListFailure::EmptyBody => "the discovery response body was empty",
            ModelListFailure::InvalidJson => "the discovery response body is not valid JSON",
            ModelListFailure::WrongSchema => {
                "the discovery response has no recognized model entries"
            }
            ModelListFailure::EmptyList => "the discovery response carried an empty model list",
        }
    }
}

/// Verify the family's discovery schema. A recognized, non-empty model list
/// is the ONLY verified outcome; an empty body, malformed JSON, a wrong
/// schema and an empty list are typed failures rather than an empty success.
/// Every loop is bounded by [`MAX_MODELS_PER_CONNECTION`].
fn verify_model_list(family: ProviderFamily, bytes: &[u8]) -> ModelListVerification {
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return ModelListVerification::Failed(ModelListFailure::EmptyBody);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return ModelListVerification::Failed(ModelListFailure::InvalidJson);
    };
    let entries = match family {
        ProviderFamily::Ollama => value.get("models").and_then(|v| v.as_array()),
        _ => value.get("data").and_then(|v| v.as_array()),
    };
    let Some(entries) = entries else {
        return ModelListVerification::Failed(ModelListFailure::WrongSchema);
    };
    if entries.is_empty() {
        return ModelListVerification::Failed(ModelListFailure::EmptyList);
    }
    let mut out = Vec::new();
    for entry in entries.iter().take(MAX_MODELS_PER_CONNECTION) {
        let id = entry
            .get("name")
            .or_else(|| entry.get("id"))
            .or_else(|| entry.get("model"))
            .and_then(|v| v.as_str());
        if let Some(id) = id {
            if !id.is_empty() && id.len() <= 256 {
                out.push(id.to_string());
            }
        }
    }
    if out.is_empty() {
        return ModelListVerification::Failed(ModelListFailure::WrongSchema);
    }
    out.sort();
    out.dedup();
    ModelListVerification::Verified(out)
}

fn map_connection(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderConnection> {
    let family_raw: String = row.get(1)?;
    let family = ProviderFamily::parse(&family_raw).unwrap_or(ProviderFamily::OpenAi);
    let models_raw: String = row.get(5)?;
    Ok(ProviderConnection {
        id: row.get(0)?,
        family,
        label: row.get(2)?,
        endpoint: row.get(3)?,
        credential_ref: row.get(4)?,
        models: serde_json::from_str(&models_raw).unwrap_or_default(),
        allow_loopback: row.get::<_, i64>(6)? != 0,
        api_flavor: row.get(7)?,
        trust_origin: row.get(8)?,
        revision: u64::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
        apply_state: row.get(10)?,
        connect_timeout_ms: row
            .get::<_, Option<i64>>(11)?
            .and_then(|v| u64::try_from(v).ok()),
        first_byte_timeout_ms: row
            .get::<_, Option<i64>>(12)?
            .and_then(|v| u64::try_from(v).ok()),
        total_timeout_ms: row
            .get::<_, Option<i64>>(13)?
            .and_then(|v| u64::try_from(v).ok()),
        keep_alive: row.get(14)?,
        proxy_url: row.get(15)?,
        tls_mode: row
            .get::<_, Option<String>>(16)?
            .unwrap_or_else(|| "system".into()),
        ca_cert_ref: row.get(17)?,
        created_ms: row.get(18)?,
        updated_ms: row.get(19)?,
    })
}

fn map_health(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderHealth> {
    let state_raw: String = row.get(1)?;
    let state = match state_raw.as_str() {
        "stopped" => HealthState::Stopped,
        "connecting" => HealthState::Connecting,
        "ready" => HealthState::Ready,
        "authentication_required" => HealthState::AuthenticationRequired,
        "failed" => HealthState::Failed,
        "stale" => HealthState::Stale,
        _ => HealthState::Failed,
    };
    let stages_raw: String = row.get(2)?;
    Ok(ProviderHealth {
        connection_id: row.get(0)?,
        state,
        stages: serde_json::from_str(&stages_raw).unwrap_or_default(),
        observed_ms: row.get(3)?,
        detail: row.get(4)?,
    })
}

fn validate_id(id: &str) -> Result<(), FoundationError> {
    if id.is_empty()
        || id.len() > MAX_ID_BYTES
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(FoundationError::Malformed(format!(
            "connection id must be 1..={MAX_ID_BYTES} ASCII [A-Za-z0-9_.-]"
        )));
    }
    Ok(())
}

/// Endpoint classification. Only loopback can be opted into; every other
/// special class stays refused under the current policy (`allow_loopback` is
/// never a LAN/metadata/tunnel bypass).
///
/// This validator consumes the ONE egress address authority
/// ([`faktor_security::network::classify_ip`]) instead of re-deriving ranges.
/// The historical local table classified IPv4-mapped/NAT64/6to4/Teredo IPv6
/// spellings as `Public`, so `http://[::ffff:169.254.169.254]` and
/// `http://[64:ff9b::a9fe:a9fe]` were persisted and then connected as
/// link-local literals (the resolver never sees literal hosts). The shared
/// authority classifies every transition form as `Reserved` and refuses the
/// whole set, so those aliases now fail closed at save time.
pub fn validate_endpoint(
    endpoint: &str,
    allow_loopback: bool,
    id: &str,
) -> Result<(), FoundationError> {
    if endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err(FoundationError::Oversized(
            "endpoint URL exceeds the served bound".into(),
        ));
    }
    let url = url::Url::parse(endpoint)
        .map_err(|e| FoundationError::Malformed(format!("endpoint is not a valid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(FoundationError::Malformed(
            "endpoint scheme must be http or https".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FoundationError::Malformed(
            "endpoint must not embed userinfo credentials".into(),
        ));
    }
    if crate::contains_secret_material(endpoint) {
        return Err(FoundationError::Malformed(
            "endpoint must not embed secret material; store the value in the vault and reference it with credential_ref"
                .into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(FoundationError::Malformed(
            "endpoint must not carry a query or fragment; credentials belong in the vault reference".into(),
        ));
    }
    let Some(host) = url.host() else {
        return Err(FoundationError::Malformed("endpoint has no host".into()));
    };
    let host_text = host.to_string();
    let class = match host {
        url::Host::Domain(domain) => {
            let lower = domain.trim_end_matches('.').to_ascii_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") {
                EndpointClass::Loopback
            } else {
                EndpointClass::Public
            }
        }
        url::Host::Ipv4(ip) => endpoint_class(faktor_security::network::classify_ip(
            std::net::IpAddr::V4(ip),
        )),
        url::Host::Ipv6(ip) => endpoint_class(faktor_security::network::classify_ip(
            std::net::IpAddr::V6(ip),
        )),
    };
    match class {
        EndpointClass::Loopback => {
            if !allow_loopback {
                return Err(FoundationError::Malformed(format!(
                    "endpoint {host_text} is loopback; provider {id} must opt in with allow_loopback"
                )));
            }
        }
        EndpointClass::Special(address_class) => {
            return Err(FoundationError::Malformed(format!(
                "endpoint {host_text} is a {class} destination; current provider policy refuses it (allow_loopback is not a LAN/metadata bypass)",
                class = AddressClass::as_str(address_class)
            )))
        }
        EndpointClass::Public => {}
    }
    Ok(())
}

use faktor_security::network::AddressClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointClass {
    Loopback,
    Special(AddressClass),
    Public,
}

fn endpoint_class(class: AddressClass) -> EndpointClass {
    match class {
        AddressClass::Global => EndpointClass::Public,
        AddressClass::Loopback => EndpointClass::Loopback,
        other => EndpointClass::Special(other),
    }
}

/// P-15 keep-alive vocabulary: Ollama accepts `forever`/`-1` or a duration
/// (`30m`, `2h`, `90s`, `1500ms`) and `0` for immediate unload. Hex, spaces,
/// signs and arbitrary text are refused.
fn valid_keep_alive(value: &str) -> bool {
    if matches!(value, "forever" | "-1" | "0") {
        return true;
    }
    let mut chars = value.chars().peekable();
    let mut digits = 0usize;
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            digits += 1;
            chars.next();
        } else {
            break;
        }
    }
    if digits == 0 {
        return false;
    }
    let rest: String = chars.collect();
    matches!(rest.as_str(), "" | "ms" | "s" | "m" | "h" | "d")
}

/// Interpret a P-15 keep-alive as a TCP idle duration where it is a plain
/// duration. Model-residency values (`forever`, `-1`, `0`) are persisted and
/// honored by the Ollama adapter configuration, not by the HTTP connector.
fn parse_tcp_keep_alive(value: &str) -> Option<Duration> {
    if matches!(value, "forever" | "-1" | "0") {
        return None;
    }
    let digits_len = value
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .count();
    if digits_len == 0 {
        return None;
    }
    let (digits, unit) = value.split_at(digits_len);
    let amount: u64 = digits.parse().ok()?;
    let scale = match unit {
        "ms" => 1,
        "s" | "" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(Duration::from_millis(amount.saturating_mul(scale)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(family: &str, endpoint: &str, allow_loopback: Option<bool>) -> ConnectionDraft {
        ConnectionDraft {
            id: "conn-1".into(),
            family: family.into(),
            label: "Connection".into(),
            endpoint: Some(endpoint.into()),
            credential_ref: None,
            models: Vec::new(),
            allow_loopback,
            api_flavor: None,
            trust_origin: None,
            connect_timeout_ms: None,
            first_byte_timeout_ms: None,
            total_timeout_ms: None,
            keep_alive: None,
            proxy_url: None,
            tls_mode: None,
            ca_cert_ref: None,
        }
    }

    /// One live HTTP fixture: answer the probe request with `body`, declaring
    /// `declared_len` (a value larger than the body simulates a truncated
    /// response). The connection then closes, so an incomplete body is a
    /// transport failure rather than a hang.
    fn serve_once(
        body: Vec<u8>,
        declared_len: Option<usize>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let len = declared_len.unwrap_or(body.len());
            let mut response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n"
            )
            .into_bytes();
            response.extend_from_slice(&body);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        });
        (format!("http://{addr}"), handle)
    }

    async fn probe_openai_against(
        body: Vec<u8>,
        declared_len: Option<usize>,
    ) -> (ProviderHealth, ProviderHealth) {
        let (endpoint, server) = serve_once(body, declared_len);
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db.clone(), dir.path());
        let service = ProviderService::new(db, vault);
        let saved = service
            .save(&draft("openai", &endpoint, Some(true)), None)
            .unwrap();
        let health = service.probe(&saved.id).await.unwrap();
        server.join().unwrap();
        let persisted = service.health(&saved.id).unwrap().unwrap();
        (health, persisted)
    }

    #[test]
    fn secret_shaped_labels_endpoints_and_proxies_are_refused_before_persistence() {
        let secret = "sk-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        // Endpoint PATH bytes are served back, exported by diagnostics and
        // logged: a recognizable secret there must be refused typed.
        let err = validate_endpoint(&format!("https://api.example.com/v1/{secret}"), false, "c")
            .unwrap_err();
        assert!(err.to_string().contains("secret material"), "{err}");
        // Labels are metadata served by every connection listing.
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let service = ProviderService::new(
            db,
            SecretVault::new(
                FoundationDb::open(dir.path().join("v.db")).unwrap(),
                dir.path(),
            ),
        );
        let mut over = draft("openai", "https://api.example.com/v1", None);
        over.label = format!("prod {secret}");
        let err = service.save(&over, None).unwrap_err();
        assert!(err.to_string().contains("secret material"), "{err}");
        // The proxy boundary rejects the same bytes (no persistence).
        let mut over = draft("openai", "https://api.example.com/v1", None);
        over.proxy_url = Some(format!("http://proxy.example.com/{secret}"));
        let err = service.save(&over, None).unwrap_err();
        assert!(err.to_string().contains("proxy_url"), "{err}");
    }

    #[test]
    fn endpoint_policy_refuses_lan_and_requires_loopback_opt_in() {
        assert!(validate_endpoint("http://127.0.0.1:11434", true, "c").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:11434", false, "c").is_err());
        assert!(validate_endpoint("http://[::1]:11434", true, "c").is_ok());
        assert!(validate_endpoint("http://[::1]:11434", false, "c").is_err());
        assert!(validate_endpoint("http://192.168.1.5:8080", true, "c").is_err());
        assert!(validate_endpoint("http://10.0.0.1", true, "c").is_err());
        assert!(validate_endpoint("http://169.254.169.254", true, "c").is_err());
        assert!(validate_endpoint("https://api.example.com/v1", false, "c").is_ok());
        assert!(validate_endpoint("ftp://api.example.com", false, "c").is_err());
        assert!(validate_endpoint("https://user:pw@api.example.com", false, "c").is_err());
        // URL aliases that the shared address authority classifies as
        // special-purpose (IPv4-mapped/NAT64/6to4/Teredo/documentation/
        // unspecified) must never be persisted, even with allow_loopback.
        for alias in [
            "http://[::ffff:127.0.0.1]:11434",
            "http://[::ffff:169.254.169.254]",
            "http://[::ffff:10.0.0.1]",
            "http://[64:ff9b::a9fe:a9fe]",
            "http://[64:ff9b:1::a9fe:a9fe]",
            "http://[2002:a9fe:a9fe::1]",
            "http://[2001::a9fe:a9fe]",
            "http://[2001:db8::1]",
            "http://[::]",
            "http://0.0.0.0",
            "http://198.18.0.1",
            "http://224.0.0.1",
        ] {
            assert!(
                validate_endpoint(alias, true, "c").is_err(),
                "{alias} must be refused as a special-purpose destination"
            );
        }
        // Query/fragment endpoints would leak credentials into diagnostics
        // and error text; they are not a valid provider base URL.
        for unsafe_url in [
            "https://api.example.com/v1?api_key=sk-secret",
            "https://api.example.com/v1#token",
        ] {
            assert!(
                validate_endpoint(unsafe_url, false, "c").is_err(),
                "{unsafe_url} must be refused"
            );
        }
    }

    #[test]
    fn save_is_restart_required_and_delete_removes_health() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db.clone(), dir.path());
        let service = ProviderService::new(db, vault);
        let saved = service
            .save(&draft("openai", "https://api.example.com/v1", None), None)
            .unwrap();
        assert_eq!(saved.apply_state, "restart_required");
        assert!(service.health(&saved.id).unwrap().is_none());
        service
            .record_health(&ProviderHealth {
                connection_id: saved.id.clone(),
                state: HealthState::Failed,
                stages: vec![],
                observed_ms: 1,
                detail: Some("x".into()),
            })
            .unwrap();
        service.delete(&saved.id).unwrap();
        assert!(service.get(&saved.id).unwrap().is_none());
        assert!(service.health(&saved.id).unwrap().is_none());
    }

    #[test]
    fn foreign_vault_reference_and_bounds_fail_before_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db.clone(), dir.path());
        let service = ProviderService::new(db, vault);
        let mut d = draft("ollama", "http://127.0.0.1:11434", Some(true));
        d.credential_ref = Some("vault://provider/does-not-exist".into());
        assert!(service.save(&d, None).is_err());
        assert!(service.list().unwrap().is_empty());
        let mut d = draft("ollama", "http://127.0.0.1:11434", Some(true));
        d.models = vec!["x".repeat(257)];
        assert!(service.save(&d, None).is_err());
        assert!(service
            .save(&draft("bogus", "https://x.y", None), None)
            .is_err());
    }

    #[tokio::test]
    async fn probe_uses_the_checked_egress_path_and_merges_discovered_models() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let body = r#"{"models":[{"name":"llama3:8b"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db.clone(), dir.path());
        let service = ProviderService::new(db, vault);
        let mut d = draft("ollama", &format!("http://{addr}"), Some(true));
        d.models = vec!["declared:only".into()];
        let saved = service.save(&d, None).unwrap();
        let health = service.probe(&saved.id).await.unwrap();
        server.join().unwrap();
        assert_eq!(health.state, HealthState::Ready, "{health:?}");
        assert!(health
            .stages
            .iter()
            .any(|s| s.stage == "reachability" && s.state == "ok"));
        assert!(health
            .stages
            .iter()
            .any(|s| s.stage == "capability" && s.state == "skipped"));
        let reloaded = service.get(&saved.id).unwrap().unwrap();
        assert!(reloaded.models.contains(&"llama3:8b".to_string()));
        assert!(reloaded.models.contains(&"declared:only".to_string()));
        let recorded = service.health(&saved.id).unwrap().unwrap();
        assert_eq!(recorded.state, HealthState::Ready);
    }

    #[tokio::test]
    async fn probe_of_a_dead_loopback_endpoint_is_failed_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db.clone(), dir.path());
        let service = ProviderService::new(db, vault);
        let saved = service
            .save(&draft("ollama", "http://127.0.0.1:9", Some(true)), None)
            .unwrap();
        let health = service.probe(&saved.id).await.unwrap();
        assert_eq!(health.state, HealthState::Failed);
        assert!(!health.stages.iter().any(|s| s.state == "ok"));
    }

    #[test]
    fn model_list_verification_is_explicit_bounded_and_family_shaped() {
        use ModelListVerification::{Failed, Verified};
        let openai = br#"{"data":[{"id":"b"},{"id":"a"},{"id":"a"}]}"#;
        assert_eq!(
            verify_model_list(ProviderFamily::OpenAi, openai),
            Verified(vec!["a".to_string(), "b".to_string()])
        );
        let ollama = br#"{"models":[{"name":"llama3:8b"}]}"#;
        assert_eq!(
            verify_model_list(ProviderFamily::Ollama, ollama),
            Verified(vec!["llama3:8b".to_string()])
        );
        for (name, bytes, reason) in [
            ("empty", &b""[..], ModelListFailure::EmptyBody),
            ("whitespace", &b" \n\t"[..], ModelListFailure::EmptyBody),
            (
                "invalid json",
                &br#"{"data":[}"#[..],
                ModelListFailure::InvalidJson,
            ),
            ("array root", &br#"[]"#[..], ModelListFailure::WrongSchema),
            (
                "wrong key",
                &br#"{"models":[{"name":"llama3"}]}"#[..],
                ModelListFailure::WrongSchema,
            ),
            (
                "not an array",
                &br#"{"data":{"id":"gpt-4o"}}"#[..],
                ModelListFailure::WrongSchema,
            ),
            (
                "empty list",
                &br#"{"data":[]}"#[..],
                ModelListFailure::EmptyList,
            ),
            (
                "no recognized entries",
                &br#"{"data":[{"object":"model"}]}"#[..],
                ModelListFailure::WrongSchema,
            ),
        ] {
            assert_eq!(
                verify_model_list(ProviderFamily::OpenAi, bytes),
                Failed(reason.clone()),
                "{name}"
            );
        }
        let entries: Vec<String> = (0..MAX_MODELS_PER_CONNECTION * 2)
            .map(|i| format!(r#"{{"id":"model-{i}"}}"#))
            .collect();
        let oversized = format!(r#"{{"data":[{}]}}"#, entries.join(","));
        let Verified(models) = verify_model_list(ProviderFamily::OpenAi, oversized.as_bytes())
        else {
            panic!("bounded list must verify");
        };
        assert_eq!(models.len(), MAX_MODELS_PER_CONNECTION);
    }

    #[tokio::test]
    async fn probe_2xx_without_a_verified_model_list_is_never_ready() {
        for (name, body, declared_len) in [
            ("empty body", &b""[..], None),
            ("invalid json", &br#"{"data":[}"#[..], None),
            (
                "wrong schema",
                &br#"{"models":[{"name":"llama3"}]}"#[..],
                None,
            ),
            ("empty list", &br#"{"data":[]}"#[..], None),
            (
                "no recognized entries",
                &br#"{"data":[{"object":"model"}]}"#[..],
                None,
            ),
            (
                "truncated body",
                &br#"{"data":[{"id":"gpt-4o"}"#[..],
                Some(512),
            ),
        ] {
            let (health, persisted) = probe_openai_against(body.to_vec(), declared_len).await;
            assert_eq!(health.state, HealthState::Failed, "{name}: {health:?}");
            assert_eq!(
                persisted.state,
                HealthState::Failed,
                "{name}: {persisted:?}"
            );
            assert_eq!(
                health.detail.as_deref(),
                Some("model list verification failed"),
                "{name}: {health:?}"
            );
            let model_list = health
                .stages
                .iter()
                .find(|stage| stage.stage == "model_list")
                .unwrap_or_else(|| panic!("{name}: no model_list stage: {health:?}"));
            assert_eq!(model_list.state, "failed", "{name}: {health:?}");
        }
    }

    #[tokio::test]
    async fn probe_2xx_with_a_verified_model_list_is_ready() {
        let (health, persisted) =
            probe_openai_against(br#"{"data":[{"id":"gpt-4o"}]}"#.to_vec(), None).await;
        assert_eq!(health.state, HealthState::Ready, "{health:?}");
        assert_eq!(persisted.state, HealthState::Ready);
        assert!(health
            .stages
            .iter()
            .any(|stage| stage.stage == "model_list" && stage.state == "ok"));
    }
}
