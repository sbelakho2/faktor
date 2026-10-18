//! BK04 durable context manifest and accounting.
//!
//! Every captured manifest binds exact source identity (URI/range/version/
//! content hash/inclusion reason) to one task/call/model/capability revision.
//! Categories share ONE basis; reserves are never counted as consumed input;
//! all arithmetic is checked. Unknown token accounting stays `Unavailable`,
//! never a fabricated 0 or a measured-looking meter.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{now_ms, FoundationDb};
use crate::{page_limit_i64, FoundationError};

pub const MAX_SOURCES: usize = 512;
pub const MAX_MANIFEST_PAGE: usize = 200;
pub const MAX_SOURCE_TOKEN_BOUND: u64 = 100_000_000;
pub const MAX_CONTEXT_PINS: usize = 2_000;
pub const MAX_SOURCE_ID_BYTES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Basis {
    Estimated { method: String, version: String },
    Observed { receipt: String },
    Unavailable { reason: String },
}

impl Basis {
    pub fn tag(&self) -> &'static str {
        match self {
            Basis::Estimated { .. } => "estimated",
            Basis::Observed { .. } => "observed",
            Basis::Unavailable { .. } => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub uri: String,
    #[serde(default)]
    pub range: Option<String>,
    pub version: String,
    pub content_hash: String,
    pub category: String,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub bytes: Option<u64>,
    pub inclusion_reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub session_id: u64,
    pub revision: u64,
    pub operation_id: String,
    #[serde(default)]
    pub call_id: Option<String>,
    pub model: String,
    pub capacity: u64,
    #[serde(default)]
    pub input: Option<u64>,
    pub output_reserve: u64,
    pub safety_reserve: u64,
    pub basis: Basis,
    pub sources: Vec<Source>,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryTotal {
    pub category: String,
    pub tokens: u64,
}

/// The served `contextUsage` projection. `headroom` and `input` are null
/// when the arithmetic cannot be trusted (unavailable/overflow).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub revision: u64,
    pub call_id: Option<String>,
    pub model: String,
    pub capacity: u64,
    pub input: Option<u64>,
    pub output_reserve: u64,
    pub safety_reserve: u64,
    pub headroom: Option<u64>,
    pub basis: Basis,
    pub estimated: bool,
    pub measured: bool,
    pub categories: Vec<CategoryTotal>,
    pub sources: Vec<Source>,
    pub overflow: bool,
    pub overflow_reason: Option<String>,
    pub created_ms: i64,
}

/// Checked category sum.
pub fn sum_categories(values: &[u64]) -> Result<u64, FoundationError> {
    values.iter().try_fold(0_u64, |sum, value| {
        sum.checked_add(*value)
            .ok_or_else(|| FoundationError::Oversized("context category overflow".into()))
    })
}

/// Checked headroom: `capacity - (input + output_reserve + safety_reserve)`.
/// `Ok(None)` means input is unknown; an overflow is a typed error.
pub fn headroom(
    capacity: u64,
    input: Option<u64>,
    output_reserve: u64,
    safety_reserve: u64,
) -> Result<Option<u64>, FoundationError> {
    let Some(input) = input else {
        return Ok(None);
    };
    let total = input
        .checked_add(output_reserve)
        .and_then(|v| v.checked_add(safety_reserve))
        .ok_or_else(|| FoundationError::Oversized("context accounting overflow".into()))?;
    capacity
        .checked_sub(total)
        .map(Some)
        .ok_or_else(|| {
            FoundationError::Validation(vec![crate::config::FieldError {
                key: "context".into(),
                scope: None,
                code: "overflow".into(),
                message: format!(
                    "input {input} + output reserve {output_reserve} + safety reserve {safety_reserve} exceeds capacity {capacity}; remove or compact context"
                ),
            }])
        })
}

impl Manifest {
    /// Validate shape and arithmetic before persistence. All sources must
    /// share the declared basis; if any source's token count is unknown the
    /// manifest input stays `None` (Unavailable), never a partial sum.
    pub fn validate(&self) -> Result<(), FoundationError> {
        if self.sources.len() > MAX_SOURCES {
            return Err(FoundationError::Oversized(format!(
                "context manifest carries {} sources, over {MAX_SOURCES}",
                self.sources.len()
            )));
        }
        if self.operation_id.is_empty() || self.operation_id.len() > 256 {
            return Err(FoundationError::Malformed(
                "context operation_id must be 1..=256 bytes".into(),
            ));
        }
        if self.model.is_empty() || self.model.len() > 256 {
            return Err(FoundationError::Malformed(
                "context model must be 1..=256 bytes".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        let mut categories: BTreeMap<&str, u64> = BTreeMap::new();
        let mut complete = true;
        for source in &self.sources {
            if source.id.is_empty() || source.id.len() > 256 {
                return Err(FoundationError::Malformed(
                    "context source id must be 1..=256 bytes".into(),
                ));
            }
            if !ids.insert(source.id.as_str()) {
                return Err(FoundationError::Malformed(format!(
                    "duplicate context source id {:?}",
                    source.id
                )));
            }
            if source.uri.is_empty() || source.uri.len() > 8192 {
                return Err(FoundationError::Malformed(
                    "context source uri must be 1..=8192 bytes".into(),
                ));
            }
            if source.version.is_empty() || source.version.len() > 256 {
                return Err(FoundationError::Malformed(
                    "context source version must be 1..=256 bytes".into(),
                ));
            }
            if source.content_hash.is_empty() || source.content_hash.len() > 256 {
                return Err(FoundationError::Malformed(
                    "context source content hash must be 1..=256 bytes".into(),
                ));
            }
            if source.category.is_empty() || source.category.len() > 128 {
                return Err(FoundationError::Malformed(
                    "context source category must be 1..=128 bytes".into(),
                ));
            }
            if source.inclusion_reason.is_empty() || source.inclusion_reason.len() > 1024 {
                return Err(FoundationError::Malformed(
                    "context source inclusion reason must be 1..=1024 bytes".into(),
                ));
            }
            match source.tokens {
                Some(tokens) if tokens <= MAX_SOURCE_TOKEN_BOUND => {
                    let entry = categories.entry(source.category.as_str()).or_insert(0);
                    *entry = entry.checked_add(tokens).ok_or_else(|| {
                        FoundationError::Oversized("context category overflow".into())
                    })?;
                }
                Some(_) => {
                    return Err(FoundationError::Oversized(format!(
                        "context source {:?} tokens exceed {MAX_SOURCE_TOKEN_BOUND}",
                        source.id
                    )))
                }
                None => complete = false,
            }
        }
        let summed = sum_categories(&categories.values().copied().collect::<Vec<_>>())?;
        if let Some(input) = self.input {
            if complete && input != summed {
                return Err(FoundationError::Malformed(format!(
                    "manifest input {input} does not equal the category sum {summed}"
                )));
            }
            headroom(
                self.capacity,
                Some(input),
                self.output_reserve,
                self.safety_reserve,
            )?;
        } else if complete && !self.sources.is_empty() {
            return Err(FoundationError::Malformed(
                "a complete source set must declare its input token total".into(),
            ));
        }
        Ok(())
    }

    pub fn usage(&self) -> ContextUsage {
        let mut cats: BTreeMap<&str, u64> = BTreeMap::new();
        for source in &self.sources {
            if let Some(tokens) = source.tokens {
                *cats.entry(source.category.as_str()).or_insert(0) =
                    cats.get(source.category.as_str()).copied().unwrap_or(0) + tokens;
            }
        }
        let categories = cats
            .into_iter()
            .map(|(category, tokens)| CategoryTotal {
                category: category.to_string(),
                tokens,
            })
            .collect();
        let headroom = headroom(
            self.capacity,
            self.input,
            self.output_reserve,
            self.safety_reserve,
        );
        let (headroom, overflow, overflow_reason) = match headroom {
            Ok(value) => (value, false, None),
            Err(FoundationError::Validation(errors)) => (
                None,
                true,
                Some(
                    errors
                        .first()
                        .map(|e| e.message.clone())
                        .unwrap_or_else(|| "context exceeds capacity".into()),
                ),
            ),
            Err(e) => (None, false, Some(e.to_string())),
        };
        ContextUsage {
            revision: self.revision,
            call_id: self.call_id.clone(),
            model: self.model.clone(),
            capacity: self.capacity,
            input: self.input,
            output_reserve: self.output_reserve,
            safety_reserve: self.safety_reserve,
            headroom,
            basis: self.basis.clone(),
            estimated: matches!(self.basis, Basis::Estimated { .. }),
            measured: matches!(self.basis, Basis::Observed { .. }),
            categories,
            sources: self.sources.clone(),
            overflow,
            overflow_reason,
            created_ms: self.created_ms,
        }
    }
}

pub struct ContextStore {
    db: FoundationDb,
}

impl ContextStore {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    /// Capture one immutable manifest as the session's next revision.
    pub fn capture(&self, manifest: Manifest) -> Result<Manifest, FoundationError> {
        manifest.validate()?;
        let sources_json = serde_json::to_string(&manifest.sources)
            .map_err(|e| FoundationError::Store(format!("encode sources: {e}")))?;
        let basis_tag = manifest.basis.tag().to_string();
        let (basis_method, basis_version, basis_detail) = match &manifest.basis {
            Basis::Estimated { method, version } => {
                (Some(method.clone()), Some(version.clone()), None)
            }
            Basis::Observed { receipt } => (None, None, Some(receipt.clone())),
            Basis::Unavailable { reason } => (None, None, Some(reason.clone())),
        };
        self.db.write("context capture", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin capture: {e}")))?;
            let current: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM context_revision WHERE session_id = ?1",
                    params![manifest.session_id as i64],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let revision = current.unwrap_or(0).saturating_add(1);
            let mut saved = manifest.clone();
            saved.revision = revision as u64;
            saved.created_ms = now_ms();
            tx.execute(
                "INSERT INTO context_manifest(
                   session_id, revision, operation_id, call_id, model, capacity,
                   input_tokens, output_reserve, safety_reserve, basis_kind,
                   basis_method, basis_version, basis_detail, sources_json, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    saved.session_id as i64,
                    revision,
                    saved.operation_id,
                    saved.call_id,
                    saved.model,
                    saved.capacity as i64,
                    saved.input.map(|v| v as i64),
                    saved.output_reserve as i64,
                    saved.safety_reserve as i64,
                    basis_tag,
                    basis_method,
                    basis_version,
                    basis_detail,
                    sources_json,
                    saved.created_ms,
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.execute(
                "INSERT INTO context_revision(session_id, revision, updated_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                   revision = excluded.revision, updated_ms = excluded.updated_ms",
                params![saved.session_id as i64, revision, saved.created_ms],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit capture: {e}")))?;
            Ok(saved)
        })
    }

    pub fn latest(&self, session_id: u64) -> Result<Option<Manifest>, FoundationError> {
        self.db.read("context latest", |conn| {
            conn.query_row(
                "SELECT session_id, revision, operation_id, call_id, model, capacity,
                        input_tokens, output_reserve, safety_reserve, basis_kind,
                        basis_method, basis_version, basis_detail, sources_json, created_ms
                 FROM context_manifest WHERE session_id = ?1
                 ORDER BY revision DESC LIMIT 1",
                params![session_id as i64],
                map_manifest,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn revision(
        &self,
        session_id: u64,
        revision: u64,
    ) -> Result<Option<Manifest>, FoundationError> {
        self.db.read("context revision", |conn| {
            conn.query_row(
                "SELECT session_id, revision, operation_id, call_id, model, capacity,
                        input_tokens, output_reserve, safety_reserve, basis_kind,
                        basis_method, basis_version, basis_detail, sources_json, created_ms
                 FROM context_manifest WHERE session_id = ?1 AND revision = ?2",
                params![session_id as i64, revision as i64],
                map_manifest,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn page(
        &self,
        session_id: u64,
        before_revision: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Manifest>, FoundationError> {
        let limit = page_limit_i64(limit.clamp(1, MAX_MANIFEST_PAGE), "context manifest")?;
        self.db.read("context page", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, revision, operation_id, call_id, model, capacity,
                            input_tokens, output_reserve, safety_reserve, basis_kind,
                            basis_method, basis_version, basis_detail, sources_json, created_ms
                     FROM context_manifest
                     WHERE session_id = ?1 AND revision < ?2
                     ORDER BY revision DESC LIMIT ?3",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![
                        session_id as i64,
                        before_revision
                            .map(|v| i64::try_from(v).unwrap_or(i64::MAX))
                            .unwrap_or(i64::MAX),
                        limit
                    ],
                    map_manifest,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }
}

fn map_manifest(row: &rusqlite::Row<'_>) -> rusqlite::Result<Manifest> {
    let basis_kind: String = row.get(9)?;
    let method: Option<String> = row.get(10)?;
    let version: Option<String> = row.get(11)?;
    let detail: Option<String> = row.get(12)?;
    let basis = match basis_kind.as_str() {
        "estimated" => Basis::Estimated {
            method: method.unwrap_or_else(|| "unknown".into()),
            version: version.unwrap_or_else(|| "unknown".into()),
        },
        "observed" => Basis::Observed {
            receipt: detail.unwrap_or_default(),
        },
        "unavailable" => Basis::Unavailable {
            reason: detail.unwrap_or_else(|| "unavailable".into()),
        },
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown context basis {other:?}"),
                )),
            ))
        }
    };
    let sources_raw: String = row.get(13)?;
    let sources: Vec<Source> = serde_json::from_str(&sources_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            13,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    Ok(Manifest {
        session_id: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
        revision: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        operation_id: row.get(2)?,
        call_id: row.get(3)?,
        model: row.get(4)?,
        capacity: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
        input: row
            .get::<_, Option<i64>>(6)?
            .and_then(|v| u64::try_from(v).ok()),
        output_reserve: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        safety_reserve: u64::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
        basis,
        sources,
        created_ms: row.get(14)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinAction {
    Pin,
    Exclude,
}

impl PinAction {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pin" => Some(Self::Pin),
            "exclude" => Some(Self::Exclude),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Exclude => "exclude",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPin {
    pub session_id: u64,
    pub source_id: String,
    pub action: PinAction,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSource {
    pub source: Source,
    pub pinned: bool,
    pub excluded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextOmission {
    pub source_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextExport {
    pub session_id: u64,
    pub manifest: Option<Manifest>,
    pub context_usage: Option<ContextUsage>,
    pub sources: Vec<ExportSource>,
    pub pins: Vec<ContextPin>,
    pub omissions: Vec<ContextOmission>,
    pub generated_ms: i64,
}

impl ContextStore {
    /// Pin or exclude one exact source id. Exclusion always wins over a pin;
    /// the table stores ONE decision per source, so there is no stale
    /// duplicate decision to reconcile.
    pub fn set_pin(
        &self,
        session_id: u64,
        source_id: &str,
        action: PinAction,
    ) -> Result<ContextPin, FoundationError> {
        if source_id.is_empty()
            || source_id.len() > MAX_SOURCE_ID_BYTES
            || source_id.contains('\u{0}')
        {
            return Err(FoundationError::Malformed(format!(
                "sourceId must be 1..={MAX_SOURCE_ID_BYTES} bytes without NUL"
            )));
        }
        let now = now_ms();
        self.db.write("context pin", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin pin: {e}")))?;
            let count: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM context_pin WHERE session_id = ?1",
                    params![session_id as i64],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM context_pin WHERE session_id = ?1 AND source_id = ?2",
                    params![session_id as i64, source_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if exists.is_none() && count as usize >= MAX_CONTEXT_PINS {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_CONTEXT_PINS} context pin decisions per session"
                )));
            }
            tx.execute(
                "INSERT INTO context_pin(session_id, source_id, action, updated_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id, source_id) DO UPDATE SET
                   action = excluded.action, updated_ms = excluded.updated_ms",
                params![session_id as i64, source_id, action.as_str(), now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit pin: {e}")))?;
            Ok(ContextPin {
                session_id,
                source_id: source_id.to_string(),
                action,
                updated_ms: now,
            })
        })
    }

    pub fn remove_pin(&self, session_id: u64, source_id: &str) -> Result<bool, FoundationError> {
        self.db.write("context unpin", |conn| {
            let changed = conn
                .execute(
                    "DELETE FROM context_pin WHERE session_id = ?1 AND source_id = ?2",
                    params![session_id as i64, source_id],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(changed > 0)
        })
    }

    pub fn pins(&self, session_id: u64) -> Result<Vec<ContextPin>, FoundationError> {
        self.db.read("context pins", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, source_id, action, updated_ms FROM context_pin
                     WHERE session_id = ?1 ORDER BY source_id ASC LIMIT ?2",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![session_id as i64, MAX_CONTEXT_PINS as i64], |r| {
                    let action: String = r.get(2)?;
                    Ok((
                        u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        r.get::<_, String>(1)?,
                        action,
                        r.get(3)?,
                    ))
                })
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            rows.into_iter()
                .map(|(session_id, source_id, action, updated_ms)| {
                    let action = PinAction::parse(&action).ok_or_else(|| {
                        FoundationError::Corrupt(format!(
                            "context pin {source_id:?} carries unknown action {action:?}"
                        ))
                    })?;
                    Ok(ContextPin {
                        session_id,
                        source_id,
                        action,
                        updated_ms,
                    })
                })
                .collect()
        })
    }

    /// Explicit bounded export of the last captured manifest + pin decisions.
    /// No credentials, no hidden system prompt, and every omission is named.
    pub fn export(&self, session_id: u64) -> Result<ContextExport, FoundationError> {
        let manifest = self.latest(session_id)?;
        let pins = self.pins(session_id)?;
        let pin_ids: BTreeMap<&str, PinAction> = pins
            .iter()
            .map(|p| (p.source_id.as_str(), p.action))
            .collect();
        let mut sources = Vec::new();
        let mut present = BTreeSet::new();
        if let Some(manifest) = &manifest {
            for source in &manifest.sources {
                present.insert(source.id.clone());
                sources.push(ExportSource {
                    pinned: pin_ids.get(source.id.as_str()) == Some(&PinAction::Pin),
                    excluded: pin_ids.get(source.id.as_str()) == Some(&PinAction::Exclude),
                    source: source.clone(),
                });
            }
        }
        let mut omissions = Vec::new();
        for pin in &pins {
            if pin.action == PinAction::Pin && !present.contains(&pin.source_id) {
                omissions.push(ContextOmission {
                    source_id: pin.source_id.clone(),
                    reason: "pinned source is absent from the last captured manifest".into(),
                });
            }
            if pin.action == PinAction::Exclude {
                omissions.push(ContextOmission {
                    source_id: pin.source_id.clone(),
                    reason: "excluded by the operator; not part of the captured manifest".into(),
                });
            }
        }
        Ok(ContextExport {
            session_id,
            context_usage: manifest.as_ref().map(Manifest::usage),
            manifest,
            sources,
            pins,
            omissions,
            generated_ms: now_ms(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, category: &str, tokens: Option<u64>) -> Source {
        Source {
            id: id.into(),
            uri: format!("file:///w/{id}.rs"),
            range: Some("1-10".into()),
            version: "v1".into(),
            content_hash: format!("hash-{id}"),
            category: category.into(),
            tokens,
            bytes: Some(100),
            inclusion_reason: "explicit mention".into(),
        }
    }

    fn manifest(session: u64, tokens: &[u64]) -> Manifest {
        Manifest {
            session_id: session,
            revision: 0,
            operation_id: "op".into(),
            call_id: Some("call".into()),
            model: "m".into(),
            capacity: 32_768,
            input: Some(tokens.iter().sum()),
            output_reserve: 4_096,
            safety_reserve: 2_048,
            basis: Basis::Estimated {
                method: "generic-estimator".into(),
                version: "1".into(),
            },
            sources: tokens
                .iter()
                .enumerate()
                .map(|(i, t)| source(&format!("s{i}"), "files", Some(*t)))
                .collect(),
            created_ms: 0,
        }
    }

    #[test]
    fn arithmetic_matches_the_spec_sample_and_rejects_overflow() {
        let m = manifest(1, &[18_400, 8_224]);
        m.validate().unwrap();
        let usage = m.usage();
        assert_eq!(usage.input, Some(26_624));
        assert_eq!(usage.categories[0].tokens, 26_624);
        assert_eq!(usage.headroom, Some(0));
        assert!(!usage.overflow);
        let mut over = manifest(1, &[30_000]);
        over.input = Some(30_000);
        let err = over.validate().unwrap_err();
        assert!(err.to_string().contains("exceeds capacity"));
        assert_eq!(
            sum_categories(&[u64::MAX, 1]).unwrap_err().to_string(),
            "oversized: context category overflow"
        );
    }

    #[test]
    fn unknown_tokens_stay_unavailable_and_are_never_zero() {
        let mut m = manifest(2, &[100]);
        m.sources[0].tokens = None;
        m.input = None;
        m.validate().unwrap();
        let usage = m.usage();
        assert_eq!(usage.input, None);
        assert_eq!(usage.headroom, None);
        assert!(!usage.measured && usage.estimated);
    }

    #[test]
    fn capture_versions_are_monotonic_immutable_and_paged() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let store = ContextStore::new(db);
        let one = store.capture(manifest(9, &[10])).unwrap();
        let two = store.capture(manifest(9, &[20])).unwrap();
        assert_eq!(one.revision, 1);
        assert_eq!(two.revision, 2);
        assert_eq!(store.latest(9).unwrap().unwrap().input, Some(20));
        assert_eq!(store.revision(9, 1).unwrap().unwrap().input, Some(10));
        assert_eq!(store.page(9, None, 10).unwrap().len(), 2);
        assert!(store.latest(10).unwrap().is_none());
    }

    #[test]
    fn pins_exclusions_and_export_are_bounded_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let store = ContextStore::new(db);
        store.capture(manifest(4, &[10, 20])).unwrap();
        let pinned = store.set_pin(4, "s0", PinAction::Pin).unwrap();
        assert_eq!(pinned.action, PinAction::Pin);
        store.set_pin(4, "absent", PinAction::Pin).unwrap();
        store.set_pin(4, "s1", PinAction::Exclude).unwrap();
        let export = store.export(4).unwrap();
        assert_eq!(export.sources.len(), 2);
        assert!(export
            .sources
            .iter()
            .any(|s| s.source.id == "s0" && s.pinned && !s.excluded));
        assert!(export
            .sources
            .iter()
            .any(|s| s.source.id == "s1" && !s.pinned && s.excluded));
        assert!(export
            .omissions
            .iter()
            .any(|o| o.source_id == "absent" && o.reason.contains("absent")));
        assert!(export
            .omissions
            .iter()
            .any(|o| o.source_id == "s1" && o.reason.contains("excluded")));
        assert!(store.remove_pin(4, "s0").unwrap());
        assert!(!store.remove_pin(4, "s0").unwrap());
        assert!(store.set_pin(4, "", PinAction::Pin).is_err());
        for pin in store.pins(4).unwrap() {
            assert!(store.remove_pin(4, &pin.source_id).unwrap());
        }
        for i in 0..MAX_CONTEXT_PINS {
            store.set_pin(4, &format!("s{i}"), PinAction::Pin).unwrap();
        }
        assert!(matches!(
            store.set_pin(4, "overflow", PinAction::Pin),
            Err(FoundationError::Oversized(_))
        ));
    }

    #[test]
    fn duplicate_sources_and_broken_input_sums_are_refused() {
        let mut m = manifest(3, &[10, 20]);
        m.sources[1].id = m.sources[0].id.clone();
        assert!(m.validate().is_err());
        let mut m = manifest(3, &[10, 20]);
        m.input = Some(31);
        assert!(m
            .validate()
            .unwrap_err()
            .to_string()
            .contains("category sum"));
    }
}
