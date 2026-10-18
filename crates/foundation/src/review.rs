//! Durable review comments (Rev8 V-02): line-anchored comments tied to a
//! session, task, change identity and diff side. Comments are ordinary
//! durable records — reading a diff never creates or edits them, and a
//! stale content hash is surfaced instead of silently rebasing the comment.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::now_ms;
use crate::{hash_hex, FoundationDb, FoundationError};

pub const MAX_REVIEW_COMMENTS: usize = 2_000;
pub const MAX_COMMENT_BODY_BYTES: usize = 16 * 1024;
pub const MAX_COMMENT_PATH_BYTES: usize = 4_096;
pub const MAX_COMMENT_ID_BYTES: usize = 128;
pub const MAX_COMMENT_AUTHOR_BYTES: usize = 128;
pub const MAX_LINE_NUMBER: u64 = 10_000_000;
pub const MAX_COMMENT_SPAN: u64 = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewCommentDraft {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub change_id: Option<String>,
    pub path: String,
    pub line_start: u64,
    pub line_end: u64,
    /// `old` | `new`
    pub side: String,
    pub body: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub resolved: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    pub id: String,
    pub session_id: u64,
    pub task_id: Option<String>,
    pub change_id: Option<String>,
    pub path: String,
    pub line_start: u64,
    pub line_end: u64,
    pub side: String,
    pub body: String,
    pub author: String,
    pub content_hash: Option<String>,
    pub resolved: bool,
    pub revision: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewCommentPatch {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub resolved: Option<bool>,
}

pub struct ReviewService {
    db: FoundationDb,
}

impl ReviewService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn list(
        &self,
        session_id: u64,
        task_id: Option<&str>,
        path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReviewComment>, FoundationError> {
        let limit = limit.clamp(1, 500) as i64;
        self.db.read("review comment list", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, task_id, change_id, path, line_start, line_end,
                            side, body, author, content_hash, resolved, revision,
                            created_ms, updated_ms
                     FROM review_comment
                     WHERE session_id = ?1
                       AND (?2 IS NULL OR task_id = ?2)
                       AND (?3 IS NULL OR path = ?3)
                     ORDER BY path ASC, line_start ASC, id ASC
                     LIMIT ?4",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![session_id.to_string(), task_id, path, limit],
                    map_comment,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn get(&self, session_id: u64, id: &str) -> Result<Option<ReviewComment>, FoundationError> {
        self.db.read("review comment get", |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, change_id, path, line_start, line_end,
                        side, body, author, content_hash, resolved, revision,
                        created_ms, updated_ms
                 FROM review_comment WHERE session_id = ?1 AND id = ?2",
                params![session_id.to_string(), id],
                map_comment,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    pub fn create(
        &self,
        session_id: u64,
        draft: &ReviewCommentDraft,
    ) -> Result<ReviewComment, FoundationError> {
        validate_draft(draft)?;
        let id = draft.id.clone().unwrap_or_else(|| {
            format!(
                "rc-{}",
                &hash_hex(authoritative_seed(session_id, draft).as_bytes())[..16]
            )
        });
        if id.is_empty()
            || id.len() > MAX_COMMENT_ID_BYTES
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(FoundationError::Malformed(
                "review comment id must be 1..=128 bytes of ASCII [A-Za-z0-9._-]".into(),
            ));
        }
        let author = draft.author.clone().unwrap_or_else(|| "local-user".into());
        validate_author(&author)?;
        let now = now_ms();
        self.db.write("review comment create", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin review comment: {e}")))?;
            if let Some(existing) = tx
                .query_row(
                    "SELECT id FROM review_comment WHERE session_id = ?1 AND id = ?2",
                    params![session_id.to_string(), id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?
            {
                return Err(FoundationError::Conflict(format!(
                    "review comment {existing} already exists"
                )));
            }
            let count: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM review_comment WHERE session_id = ?1",
                    params![session_id as i64],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if count as usize >= MAX_REVIEW_COMMENTS {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_REVIEW_COMMENTS} review comments per session"
                )));
            }
            tx.execute(
                "INSERT INTO review_comment(
                   id, session_id, task_id, change_id, path, line_start, line_end,
                   side, body, author, content_hash, resolved, revision,
                   created_ms, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, ?13)",
                params![
                    id,
                    session_id.to_string(),
                    draft.task_id,
                    draft.change_id,
                    draft.path,
                    draft.line_start as i64,
                    draft.line_end as i64,
                    draft.side,
                    draft.body,
                    author,
                    draft.content_hash,
                    if draft.resolved.unwrap_or(false) {
                        1
                    } else {
                        0
                    },
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit review comment: {e}")))?;
            self.read_comment(&id)
        })
    }

    pub fn update(
        &self,
        session_id: u64,
        id: &str,
        expected_revision: Option<u64>,
        patch: &ReviewCommentPatch,
    ) -> Result<ReviewComment, FoundationError> {
        if let Some(body) = &patch.body {
            if body.is_empty() || body.len() > MAX_COMMENT_BODY_BYTES {
                return Err(FoundationError::Malformed(format!(
                    "comment body must be 1..={MAX_COMMENT_BODY_BYTES} bytes"
                )));
            }
        }
        let now = now_ms();
        self.db.write("review comment update", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin review update: {e}")))?;
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT revision, body FROM review_comment WHERE session_id = ?1 AND id = ?2",
                    params![session_id.to_string(), id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let Some((revision, current_body)) = existing else {
                return Err(FoundationError::NotFound(format!(
                    "review comment {id} of session {session_id}"
                )));
            };
            if let Some(expected) = expected_revision {
                if revision.max(0) as u64 != expected {
                    return Err(FoundationError::Conflict(format!(
                        "review comment {id} revision changed: expected {expected} but found {revision}"
                    )));
                }
            }
            let body = patch.body.clone().unwrap_or(current_body);
            tx.execute(
                "UPDATE review_comment SET body = ?3, resolved = COALESCE(?4, resolved),
                   revision = revision + 1, updated_ms = ?5
                 WHERE session_id = ?1 AND id = ?2",
                params![
                    session_id.to_string(),
                    id,
                    body,
                    patch.resolved.map(|v| if v { 1_i64 } else { 0 }),
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit review update: {e}")))?;
            self.read_comment(id)
        })
    }

    pub fn delete(
        &self,
        session_id: u64,
        id: &str,
        expected_revision: Option<u64>,
    ) -> Result<(), FoundationError> {
        self.db.write("review comment delete", |conn| {
            let revision: Option<i64> = conn
                .query_row(
                    "SELECT revision FROM review_comment WHERE session_id = ?1 AND id = ?2",
                    params![session_id.to_string(), id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let Some(revision) = revision else {
                return Err(FoundationError::NotFound(format!(
                    "review comment {id} of session {session_id}"
                )));
            };
            if let Some(expected) = expected_revision {
                if revision.max(0) as u64 != expected {
                    return Err(FoundationError::Conflict(format!(
                        "review comment {id} revision changed: expected {expected} but found {revision}"
                    )));
                }
            }
            conn.execute(
                "DELETE FROM review_comment WHERE session_id = ?1 AND id = ?2",
                params![session_id.to_string(), id],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(())
        })
    }

    fn read_comment(&self, id: &str) -> Result<ReviewComment, FoundationError> {
        self.db.read("review comment read", |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, change_id, path, line_start, line_end,
                        side, body, author, content_hash, resolved, revision,
                        created_ms, updated_ms
                 FROM review_comment WHERE id = ?1",
                params![id],
                map_comment,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }
}

fn authoritative_seed(session_id: u64, draft: &ReviewCommentDraft) -> String {
    format!(
        "{session_id}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        draft.path,
        draft.line_start,
        draft.line_end,
        draft.side,
        draft.change_id.as_deref().unwrap_or(""),
        draft.body
    )
}

fn validate_author(author: &str) -> Result<(), FoundationError> {
    if author.is_empty()
        || author.len() > MAX_COMMENT_AUTHOR_BYTES
        || author.chars().any(char::is_control)
    {
        return Err(FoundationError::Malformed(
            "comment author must be 1..=128 non-control bytes".into(),
        ));
    }
    Ok(())
}

fn validate_draft(draft: &ReviewCommentDraft) -> Result<(), FoundationError> {
    if draft.path.is_empty()
        || draft.path.len() > MAX_COMMENT_PATH_BYTES
        || draft.path.contains('\0')
        || draft.path.starts_with('/')
        || draft.path.starts_with('\\')
        || draft.path.contains("..")
        || draft.path.as_bytes().get(1) == Some(&b':')
    {
        return Err(FoundationError::Malformed(
            "comment path must be a bounded workspace-relative path".into(),
        ));
    }
    if draft.line_start == 0
        || draft.line_end < draft.line_start
        || draft.line_end > MAX_LINE_NUMBER
        || draft.line_end - draft.line_start > MAX_COMMENT_SPAN
    {
        return Err(FoundationError::Malformed(format!(
            "comment lines must be 1..={MAX_LINE_NUMBER} with end >= start and a span <= {MAX_COMMENT_SPAN}"
        )));
    }
    if draft.side != "old" && draft.side != "new" {
        return Err(FoundationError::Malformed(
            "comment side must be old or new".into(),
        ));
    }
    if draft.body.is_empty()
        || draft.body.len() > MAX_COMMENT_BODY_BYTES
        || draft.body.contains('\0')
    {
        return Err(FoundationError::Malformed(format!(
            "comment body must be 1..={MAX_COMMENT_BODY_BYTES} bytes without NUL"
        )));
    }
    if let Some(task_id) = &draft.task_id {
        if task_id.is_empty() || task_id.len() > 128 {
            return Err(FoundationError::Malformed(
                "taskId must be 1..=128 bytes".into(),
            ));
        }
    }
    if let Some(change_id) = &draft.change_id {
        if change_id.is_empty() || change_id.len() > 256 {
            return Err(FoundationError::Malformed(
                "changeId must be 1..=256 bytes".into(),
            ));
        }
    }
    if let Some(hash) = &draft.content_hash {
        if faktor_core::hash::FileHash::from_hex(hash).is_none() {
            return Err(FoundationError::Malformed(
                "contentHash must be a 64-character hex digest".into(),
            ));
        }
    }
    Ok(())
}

fn map_comment(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewComment> {
    Ok(ReviewComment {
        id: row.get(0)?,
        session_id: row.get::<_, String>(1)?.parse::<u64>().map_err(|_| {
            rusqlite::Error::InvalidColumnType(1, "session_id".into(), rusqlite::types::Type::Text)
        })?,
        task_id: row.get(2)?,
        change_id: row.get(3)?,
        path: row.get(4)?,
        line_start: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
        line_end: u64::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
        side: row.get(7)?,
        body: row.get(8)?,
        author: row.get(9)?,
        content_hash: row.get(10)?,
        resolved: row.get::<_, i64>(11)? != 0,
        revision: u64::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
        created_ms: row.get(13)?,
        updated_ms: row.get(14)?,
    })
}
