//! Durable session UI metadata (pin/archive) used by the History/parallel
//! session surfaces. Titles stay in the session store (the rename authority);
//! this table carries only presentation state that must survive restarts.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{now_ms, FoundationDb};
use crate::FoundationError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: u64,
    pub pinned: bool,
    pub archived: bool,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetaPatch {
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub archived: Option<bool>,
}

pub struct SessionMetaService {
    db: FoundationDb,
}

impl SessionMetaService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn get(&self, session_id: u64) -> Result<SessionMeta, FoundationError> {
        self.db.read("session meta get", |conn| {
            conn.query_row(
                "SELECT session_id, pinned, archived, updated_ms
                 FROM session_ui_meta WHERE session_id = ?1",
                params![session_id as i64],
                |r| {
                    Ok(SessionMeta {
                        session_id: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        pinned: r.get::<_, i64>(1)? != 0,
                        archived: r.get::<_, i64>(2)? != 0,
                        updated_ms: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
            .map(|row| {
                row.unwrap_or(SessionMeta {
                    session_id,
                    pinned: false,
                    archived: false,
                    updated_ms: 0,
                })
            })
        })
    }

    /// All pinned/archived rows in ONE statement (bounded by list size),
    /// so a 1k-session listing never runs 1k metadata queries.
    pub fn all(&self) -> Result<std::collections::BTreeMap<u64, SessionMeta>, FoundationError> {
        self.db.read("session meta all", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, pinned, archived, updated_ms
                     FROM session_ui_meta ORDER BY session_id ASC LIMIT 100000",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(SessionMeta {
                        session_id: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        pinned: r.get::<_, i64>(1)? != 0,
                        archived: r.get::<_, i64>(2)? != 0,
                        updated_ms: r.get(3)?,
                    })
                })
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows.into_iter().map(|m| (m.session_id, m)).collect())
        })
    }

    pub fn update(
        &self,
        session_id: u64,
        patch: &SessionMetaPatch,
    ) -> Result<SessionMeta, FoundationError> {
        if patch.pinned.is_none() && patch.archived.is_none() {
            return Err(FoundationError::Malformed(
                "session meta patch must set pinned and/or archived".into(),
            ));
        }
        self.db.write("session meta update", |conn| {
            let existing: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT pinned, archived FROM session_ui_meta WHERE session_id = ?1",
                    params![session_id as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let pinned = patch
                .pinned
                .unwrap_or_else(|| existing.map(|(pinned, _)| pinned != 0).unwrap_or(false));
            let archived = patch
                .archived
                .unwrap_or_else(|| existing.map(|(_, archived)| archived != 0).unwrap_or(false));
            let now = now_ms();
            conn.execute(
                "INSERT INTO session_ui_meta(session_id, pinned, archived, updated_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                   pinned = excluded.pinned,
                   archived = excluded.archived,
                   updated_ms = excluded.updated_ms",
                params![
                    session_id as i64,
                    if pinned { 1 } else { 0 },
                    if archived { 1 } else { 0 },
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(SessionMeta {
                session_id,
                pinned,
                archived,
                updated_ms: now,
            })
        })
    }
}
