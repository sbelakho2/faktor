//! `messages`: cohesive slice of the mechanically decomposed parent module.

use super::*;

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub session_id: SessionId,
    pub seq: i64,
    pub role: String,
    pub data: serde_json::Value,
    pub created_ms: i64,
}

#[derive(Debug, Clone)]
pub struct PartRow {
    pub id: i64,
    pub message_id: i64,
    pub kind: String,
    pub data: serde_json::Value,
    pub created_ms: i64,
}

pub(crate) fn message_map(r: &rusqlite::Row<'_>) -> StoreResult<MessageRow> {
    message_map_data(r, &r.get::<_, String>(4)?)
}

/// [`message_map`] over an already-read `data` column: the bounded backward
/// page reads the raw payload to measure it and used to fetch the very same
/// column a second time via [`message_map`] (two String allocations per row
/// on a hot path).
pub(crate) fn message_map_data(r: &rusqlite::Row<'_>, data_raw: &str) -> StoreResult<MessageRow> {
    let id = r.get::<_, i64>(0)?;
    Ok(MessageRow {
        id,
        session_id: id_field(&format!("message {id} session_id"), r.get::<_, i64>(1)?)?,
        seq: r.get(2)?,
        role: r.get(3)?,
        data: parse_json(&format!("message {id} data"), data_raw)?,
        created_ms: r.get(5)?,
    })
}

// Test-only count of serde decodes performed by transcript search on the
// current thread (proves the SQL prefilter skips non-candidate payloads).
#[cfg(test)]
thread_local! {
    pub(crate) static SEARCH_JSON_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn part_map(r: &rusqlite::Row<'_>) -> StoreResult<PartRow> {
    let id = r.get::<_, i64>(0)?;
    Ok(PartRow {
        id,
        message_id: r.get(1)?,
        kind: r.get(2)?,
        data: parse_json(&format!("part {id} data"), &r.get::<_, String>(3)?)?,
        created_ms: r.get(4)?,
    })
}

impl Store {
    /// Paging is fundamental: the webview sees the latest page immediately and
    /// earlier pages stream on demand. Returns newest-first page.
    pub fn messages_before(
        &self,
        session_id: SessionId,
        before_seq: Option<i64>,
        limit: u64,
    ) -> StoreResult<Vec<MessageRow>> {
        // `u64::MAX` is this legacy signature's documented whole-conversation
        // sentinel; every other out-of-range limit is an explicit request the
        // explicit page twin refuses typed (never a negative/unbounded LIMIT).
        let limit = if limit == u64::MAX { None } else { Some(limit) };
        self.messages_before_page(session_id, before_seq, limit)
    }

    /// Explicit-page twin of [`Store::messages_before`]: `None` binds
    /// SQLite's unbounded sentinel (`LIMIT -1`) and an explicit limit above
    /// `i64::MAX` refuses typed.
    pub fn messages_before_page(
        &self,
        session_id: SessionId,
        before_seq: Option<i64>,
        limit: Option<u64>,
    ) -> StoreResult<Vec<MessageRow>> {
        let limit = match limit {
            None => -1i64,
            Some(limit) => i64::try_from(limit).map_err(|_| {
                StoreError::Oversized("message page limit exceeds SQLite signed range".into())
            })?,
        };
        let conn = self.read()?;
        let (sql, params) = match before_seq {
            Some(b) => (
                "SELECT id, session_id, seq, role, data, created_ms FROM message
                 WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
                vec![session_id.raw() as i64, b, limit],
            ),
            None => (
                "SELECT id, session_id, seq, role, data, created_ms FROM message
                 WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
                vec![session_id.raw() as i64, limit],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(message_map(row)?);
        }
        Ok(out)
    }

    /// Newest-first bounded backward load of one conversation window
    /// (audit 29: never load thousands of historical messages just to trim
    /// them afterward). Walks rows newest-first and stops the moment EITHER
    /// bound is hit:
    ///
    /// - `max_messages`: the result never holds more rows than this.
    /// - `max_bytes`: the running total of stored message-payload bytes
    ///   (the `data` JSON column, exactly as persisted) never exceeds this.
    ///   Message granularity is absolute — a row is never partial: a single
    ///   oversized message still counts as one message and may exceed
    ///   `max_bytes` alone. The byte bound only applies between rows (the
    ///   newest row is always taken when the window is empty, so a hostile
    ///   `max_bytes = 0` yields the newest message, not an empty window).
    ///
    /// Rows older than the returned window are NEVER read: the statement is
    /// stepped lazily over the UNIQUE `(session_id, seq)` autoindex backward and
    /// the loop breaks before stepping past a bound. `before_seq` cuts
    /// strictly (`seq < before_seq`); values above `i64::MAX` clamp to "no
    /// older bound" (the newest page).
    pub fn messages_backwards_bounded(
        &self,
        session_id: SessionId,
        before_seq: Option<u64>,
        max_messages: u64,
        max_bytes: u64,
    ) -> StoreResult<Vec<MessageRow>> {
        let conn = self.read()?;
        if max_messages == 0 {
            return Ok(Vec::new());
        }
        let before = before_seq.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
        let (sql, params): (&str, Vec<rusqlite::types::Value>) = match before {
            Some(b) => (
                "SELECT id, session_id, seq, role, data, created_ms FROM message
                 WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC",
                vec![
                    rusqlite::types::Value::Integer(session_id.raw() as i64),
                    rusqlite::types::Value::Integer(b),
                ],
            ),
            None => (
                "SELECT id, session_id, seq, role, data, created_ms FROM message
                 WHERE session_id = ?1 ORDER BY seq DESC",
                vec![rusqlite::types::Value::Integer(session_id.raw() as i64)],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let mut out: Vec<MessageRow> = Vec::new();
        let mut total_bytes: u64 = 0;
        while let Some(row) = rows.next()? {
            if out.len() as u64 >= max_messages {
                break;
            }
            let raw: String = row.get(4)?;
            let row_bytes = raw.len() as u64;
            if !out.is_empty() && total_bytes.saturating_add(row_bytes) > max_bytes {
                break;
            }
            out.push(message_map_data(row, &raw)?);
            total_bytes = total_bytes.saturating_add(row_bytes);
        }
        Ok(out)
    }

    /// Newest-first bounded window that charges BOTH the message payload and
    /// its part payloads against `max_bytes`, and returns each message with
    /// its parts in one batch (no per-message N+1 query).
    ///
    /// The older [`Self::messages_backwards_bounded`] only charged the
    /// `message.data` column, which is `{"parts":[]}` for assistant/tool
    /// messages: a budget-sized window could therefore load hundreds of
    /// messages whose TEXT parts were many megabytes. This is the turn-path
    /// loader's bound: paging/windowing must account for the bytes the
    /// runtime actually materializes.
    ///
    /// `after_seq` is a strictly-greater lower bound (the durable context
    /// checkpoint's high-water mark), so a compacted session never rescans
    /// rows older than its checkpoint.
    pub fn messages_window_with_parts(
        &self,
        session_id: SessionId,
        before_seq: Option<u64>,
        after_seq: Option<u64>,
        max_messages: u64,
        max_bytes: u64,
    ) -> StoreResult<Vec<(MessageRow, Vec<PartRow>)>> {
        if max_messages == 0 {
            return Ok(Vec::new());
        }
        let conn = self.read()?;
        let before = before_seq.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
        let after = after_seq.map(|a| i64::try_from(a).unwrap_or(i64::MAX));
        let mut sql = String::from(
            "SELECT m.id, m.session_id, m.seq, m.role, m.data, m.created_ms,
                    COALESCE((SELECT SUM(length(p.data)) FROM part p
                              WHERE p.message_id = m.id), 0)
             FROM message m
             WHERE m.session_id = ?1",
        );
        let mut params: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Integer(session_id.raw() as i64)];
        if let Some(b) = before {
            sql.push_str(" AND m.seq < ?");
            params.push(rusqlite::types::Value::Integer(b));
        }
        if let Some(a) = after {
            sql.push_str(" AND m.seq > ?");
            params.push(rusqlite::types::Value::Integer(a));
        }
        sql.push_str(" ORDER BY m.seq DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut messages: Vec<MessageRow> = Vec::new();
        let mut total_bytes: u64 = 0;
        while let Some(row) = rows.next()? {
            if messages.len() as u64 >= max_messages {
                break;
            }
            let raw: String = row.get(4)?;
            let part_bytes: i64 = row.get(6)?;
            let row_bytes = (raw.len() as u64).saturating_add(part_bytes.max(0) as u64);
            // Message granularity is absolute. A row that does not fit the
            // running bound stops the walk (older rows are never preferred
            // over newer ones), EXCEPT a single row that can never fit by
            // itself: such an oversized row is taken WHOLE and then the
            // walk stops (the docs' lone-oversized-message rule). Skipping
            // it and continuing to older rows was how one large assistant/
            // tool message right before the newest prompt made the loader
            // drop the entire preceding conversation.
            let oversized = row_bytes > max_bytes;
            if !messages.is_empty()
                && !oversized
                && total_bytes.saturating_add(row_bytes) > max_bytes
            {
                break;
            }
            messages.push(message_map_data(row, &raw)?);
            total_bytes = total_bytes.saturating_add(row_bytes);
            if oversized {
                break;
            }
        }
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        // One bounded batch read for every collected message id. Ids are
        // chunked so a hostile page bound can never build an unbounded IN
        // list.
        const ID_CHUNK: usize = 500;
        let mut parts_by_message: std::collections::HashMap<i64, Vec<PartRow>> =
            std::collections::HashMap::with_capacity(messages.len());
        for chunk in messages.chunks(ID_CHUNK) {
            let mut placeholders = String::new();
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len());
            for (i, m) in chunk.iter().enumerate() {
                if i > 0 {
                    placeholders.push(',');
                }
                placeholders.push('?');
                params.push(rusqlite::types::Value::Integer(m.id));
            }
            let sql = format!(
                "SELECT id, message_id, kind, data, created_ms FROM part
                 WHERE message_id IN ({placeholders}) ORDER BY message_id, id"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
            while let Some(row) = rows.next()? {
                let part = part_map(row)?;
                parts_by_message
                    .entry(part.message_id)
                    .or_default()
                    .push(part);
            }
        }
        Ok(messages
            .into_iter()
            .map(|m| {
                let parts = parts_by_message.remove(&m.id).unwrap_or_default();
                (m, parts)
            })
            .collect())
    }

    pub fn put_message(
        &self,
        session_id: SessionId,
        seq: i64,
        role: &str,
        data: serde_json::Value,
    ) -> StoreResult<i64> {
        let role = role.to_owned();
        // Preparation BEFORE enqueueing: the message JSON body and its
        // timestamp (audit item 8) — the writer job executes SQL only.
        let data_json = data.to_string();
        let created_ms = now_ms();
        self.writer.execute_retry_busy("put_message", move |conn| {
            Self::insert_message_on(conn, session_id, seq, &role, &data_json, created_ms)
        })
    }

    /// Shared single-row message insert (fixed-arity contract). Runs on the
    /// caller's connection: the actor batch executes it inside one grouped
    /// transaction, the direct path outside any explicit transaction. The
    /// caller supplies the row timestamp (captured before enqueueing).
    pub(crate) fn insert_message_on(
        conn: &Connection,
        session_id: SessionId,
        seq: i64,
        role: &str,
        data_json: &str,
        created_ms: i64,
    ) -> StoreResult<i64> {
        conn.execute(
            "INSERT INTO message(session_id, seq, role, data, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id.raw() as i64, seq, role, data_json, created_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn put_part(
        &self,
        message_id: i64,
        kind: &str,
        data: serde_json::Value,
    ) -> StoreResult<i64> {
        let kind = kind.to_owned();
        // Preparation BEFORE enqueueing: the part JSON body and its timestamp
        // (audit item 8) — the writer job executes SQL only.
        let data_json = data.to_string();
        let created_ms = now_ms();
        self.writer.execute_retry_busy("put_part", move |conn| {
            Self::insert_part_on(conn, message_id, &kind, &data_json, created_ms)
        })
    }

    /// Shared single-row part insert; see [`Self::insert_message_on`]. The
    /// caller supplies the row timestamp (captured before enqueueing).
    pub(crate) fn insert_part_on(
        conn: &Connection,
        message_id: i64,
        kind: &str,
        data_json: &str,
        created_ms: i64,
    ) -> StoreResult<i64> {
        conn.execute(
            "INSERT INTO part(message_id, kind, data, created_ms) VALUES (?1, ?2, ?3, ?4)",
            params![message_id, kind, data_json, created_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn parts_of(&self, message_id: i64) -> StoreResult<Vec<PartRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, message_id, kind, data, created_ms FROM part WHERE message_id = ?1 ORDER BY id ASC",
        )?;
        let mut rows = stmt.query(params![message_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(part_map(row)?);
        }
        Ok(out)
    }

    /// Bounded BATCH part read for a set of message ids (one indexed `IN`
    /// query per <=500 ids, preserving per-message id order). Callers that
    /// inspect a transcript page must use this instead of one `parts_of`
    /// round trip per message: the historical per-message loop made a
    /// deleteMessage/reference scan O(messages) SQLite queries (quadratic
    /// with transcript length). Ids not present are simply absent from the
    /// map; the caller's per-message fallback is an empty Vec.
    pub fn parts_for_messages(
        &self,
        message_ids: &[i64],
    ) -> StoreResult<std::collections::HashMap<i64, Vec<PartRow>>> {
        const ID_CHUNK: usize = 500;
        let mut out: std::collections::HashMap<i64, Vec<PartRow>> =
            std::collections::HashMap::with_capacity(message_ids.len());
        if message_ids.is_empty() {
            return Ok(out);
        }
        let conn = self.read()?;
        for chunk in message_ids.chunks(ID_CHUNK) {
            let mut placeholders = String::new();
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len());
            for (i, id) in chunk.iter().enumerate() {
                if i > 0 {
                    placeholders.push(',');
                }
                placeholders.push('?');
                params.push(rusqlite::types::Value::Integer(*id));
            }
            let sql = format!(
                "SELECT id, message_id, kind, data, created_ms FROM part
                 WHERE message_id IN ({placeholders}) ORDER BY message_id, id"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
            while let Some(row) = rows.next()? {
                let part = part_map(row)?;
                out.entry(part.message_id).or_default().push(part);
            }
        }
        Ok(out)
    }

    pub fn message_count(&self, session_id: SessionId) -> StoreResult<i64> {
        let conn = self.read()?;
        let out = conn.query_row(
            "SELECT COUNT(*) FROM message WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get(0),
        )?;
        Ok(out)
    }

    /// `created_ms` of one message by (session, seq); `None` when the message
    /// does not exist. The revert wire surface uses it as the checkpoint
    /// cutoff: only checkpoints recorded at or before the message may roll
    /// back to it.
    pub fn message_created_ms(&self, session_id: SessionId, seq: i64) -> StoreResult<Option<i64>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT created_ms FROM message WHERE session_id = ?1 AND seq = ?2",
                params![session_id.raw() as i64, seq],
                |r| r.get(0),
            )
            .optional()?;
        Ok(out)
    }

    /// Durable single-message removal (deleteMessage, P1): the message row
    /// AND its part rows are deleted in ONE transaction — a crash can never
    /// leave orphan parts (part rows reference the message row by foreign
    /// key, so the order is structural, not incidental). Message sequences
    /// are STABLE: rows are removed, nothing is renumbered, and the paging
    /// projection simply skips the hole. Returns whether a message row
    /// existed (false = nothing deleted). The journal is intentionally
    /// untouched: it is the durable log of what happened; deleting a
    /// conversation row is not a state-machine event.
    pub fn delete_message(&self, session_id: SessionId, seq: i64) -> StoreResult<bool> {
        self.writer.execute("delete_message", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let id: Option<i64> = tx
                .query_row(
                    "SELECT id FROM message WHERE session_id = ?1 AND seq = ?2",
                    params![session_id.raw() as i64, seq],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(id) = id else {
                return Ok(false);
            };
            tx.execute("DELETE FROM part WHERE message_id = ?1", params![id])?;
            tx.execute(
                "DELETE FROM message WHERE session_id = ?1 AND seq = ?2",
                params![session_id.raw() as i64, seq],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// Durable transcript search over one session's stored payloads (D-03):
    /// newest-first over the message sequence, matching the VISIBLE decoded
    /// text, never the raw JSON envelope.
    ///
    /// The historical implementation ran `instr(lower(raw_json), lower(q))`:
    /// a query for `text`, `parts` or `"` matched every row because those
    /// characters occur in the JSON keys/punctuation, and SQLite's `lower()`
    /// folds ASCII only (`É` never found `é`). This implementation walks the
    /// durable transcript in bounded chunks (memory stays O(chunk)), decodes
    /// each row, extracts the fields the transcript actually renders —
    /// message `text`; part `text` for text/reasoning/summary; tool_result
    /// `excerpt`; tool_call `name` + its input string leaves — and applies
    /// Unicode `to_lowercase()` folding to both sides. The page bound is
    /// exact over MATCHES; chunks stop as soon as the page is full.
    pub fn search_messages(
        &self,
        session_id: SessionId,
        query: &str,
        limit: u64,
    ) -> StoreResult<Vec<MessageRow>> {
        if query.is_empty() || query.len() > 256 {
            return Err(StoreError::Malformed(
                "transcript search query must be 1..=256 bytes".into(),
            ));
        }
        let limit = usize::try_from(limit.clamp(1, 200)).unwrap_or(200);
        let needle = query.to_lowercase();
        // ASCII needles without JSON-escaped bytes (`"`, `\`, controls) can
        // be prefiltered by SQLite before Rust materializes a payload or
        // serde decodes a leaf. The SQL chunk still treats escape-bearing or
        // non-ASCII payloads as candidates (see `search_message_chunk`), so
        // the prefilter is never allowed to hide a decoded match. Any other
        // needle falls back to decoding the bounded chunk (Unicode
        // escapes/case folding cannot be expressed by SQLite's ASCII
        // `lower`).
        let prefilter = ascii_prefilter_needle(&needle);
        let prefilter = prefilter.as_deref();
        const CHUNK: u64 = 128;
        let mut out: Vec<MessageRow> = Vec::with_capacity(limit);
        let mut before: Option<i64> = None;
        let mut folded = String::new();
        loop {
            let rows = self.search_message_chunk(session_id, before, prefilter, CHUNK)?;
            if rows.is_empty() {
                break;
            }
            let chunk_len = rows.len() as u64;
            let last_seq = rows.last().map(|row| row.seq);
            // Candidate parts in ONE bounded batch per chunk (an IN list of
            // at most CHUNK ids; ids are i64 and the chunk is 128).
            let candidates: Vec<i64> = rows
                .iter()
                .filter(|row| row.data.is_some())
                .map(|row| row.id)
                .collect();
            let mut parts_by_message: std::collections::HashMap<i64, Vec<PartRow>> =
                std::collections::HashMap::with_capacity(candidates.len());
            if !candidates.is_empty() {
                let mut placeholders = String::new();
                let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(candidates.len());
                for (index, id) in candidates.iter().enumerate() {
                    if index > 0 {
                        placeholders.push(',');
                    }
                    placeholders.push('?');
                    params.push(rusqlite::types::Value::Integer(*id));
                }
                let conn = self.read()?;
                let sql = format!(
                    "SELECT id, message_id, kind, data, created_ms FROM part
                     WHERE message_id IN ({placeholders}) ORDER BY message_id, id"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut part_rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
                while let Some(row) = part_rows.next()? {
                    let part = part_map(row)?;
                    parts_by_message
                        .entry(part.message_id)
                        .or_default()
                        .push(part);
                }
            }
            for row in rows {
                let Some(raw) = row.data else { continue };
                #[cfg(test)]
                SEARCH_JSON_DECODES.with(|count| count.set(count.get() + 1));
                let message = MessageRow {
                    id: row.id,
                    session_id: row.session_id,
                    seq: row.seq,
                    role: row.role,
                    data: parse_json(&format!("message {} data", row.id), &raw)?,
                    created_ms: row.created_ms,
                };
                let parts = parts_by_message.remove(&row.id).unwrap_or_default();
                if message_matches(&message, &parts, &needle, &mut folded) {
                    out.push(message);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
            match last_seq {
                Some(seq) if chunk_len == CHUNK => before = Some(seq),
                _ => break,
            }
        }
        Ok(out)
    }

    /// One bounded newest-first chunk of the transcript for search. The
    /// `CASE` returns `data` ONLY for rows whose stored JSON (message or
    /// part) contains the ASCII prefilter needle; every other row is
    /// returned with `data = NULL` and is never materialized. Without a
    /// prefilter (Unicode needle) every row of the chunk is a candidate.
    fn search_message_chunk(
        &self,
        session_id: SessionId,
        before: Option<i64>,
        prefilter: Option<&str>,
        chunk: u64,
    ) -> StoreResult<Vec<SearchChunkRow>> {
        // `LIKE` is SQLite's ASCII-case-insensitive matcher without a
        // per-row `lower()` copy (measured ~30% faster than
        // `instr(lower(data), ...)` on a 50k-row transcript). Wildcards in
        // the needle are escaped by `ascii_prefilter_needle`.
        //
        // The literal LIKE is only a PERFORMANCE filter: it must never skip a
        // row that could match after decoding. Two stored-encoding cases can
        // hide a visible match from it:
        //   * a backslash introduces any JSON escape (`\u0061`, `\/`, `\n`,
        //     ...), so the decoded text need not occur literally;
        //   * a non-ASCII or control character can lowercase to ASCII
        //     (U+212A KELVIN SIGN -> `k`) or is simply not expressible by
        //     SQLite's ASCII-only `LIKE`.
        // Such rows (message OR any part payload) are always decoded and
        // judged by `message_matches`; ordinary printable-ASCII rows still
        // take the fast SQL rejection path.
        let predicate = "m.data LIKE ?3 ESCAPE '\\'
                OR instr(m.data, '\\') > 0 OR m.data GLOB '*[^ -~]*'
                OR EXISTS (
                    SELECT 1 FROM part p WHERE p.message_id = m.id
                     AND (p.data LIKE ?3 ESCAPE '\\'
                          OR instr(p.data, '\\') > 0
                          OR p.data GLOB '*[^ -~]*'))";
        let data_expr = if prefilter.is_some() {
            format!("CASE WHEN ({predicate}) THEN m.data ELSE NULL END")
        } else {
            "m.data".to_string()
        };
        let filter = if prefilter.is_some() {
            format!(" AND ({predicate})")
        } else {
            String::new()
        };
        let limit_placeholder = if prefilter.is_some() { "?4" } else { "?3" };
        let sql = format!(
            "SELECT m.id, m.session_id, m.seq, m.role, {data_expr}, m.created_ms
             FROM message m
             WHERE m.session_id = ?1 AND m.seq < ?2{filter}
             ORDER BY m.seq DESC LIMIT {limit_placeholder}"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(4);
        params.push(rusqlite::types::Value::Integer(session_id.raw() as i64));
        params.push(rusqlite::types::Value::Integer(before.unwrap_or(i64::MAX)));
        if let Some(needle) = prefilter {
            params.push(rusqlite::types::Value::Text(needle.to_string()));
        }
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(chunk).unwrap_or(i64::MAX),
        ));
        let conn = self.read()?;
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id = row.get::<_, i64>(0)?;
            let raw: Option<String> = row.get(4)?;
            out.push(SearchChunkRow {
                id,
                session_id: id_field(&format!("message {id} session_id"), row.get(1)?)?,
                seq: row.get(2)?,
                role: row.get(3)?,
                data: raw,
                created_ms: row.get(5)?,
            });
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- task ledger
}

/// One payload-bearing search candidate (see
/// [`Store::search_message_chunk`]): `data` is `None` for rows the SQL
/// prefilter rejected, so their JSON is never decoded.
struct SearchChunkRow {
    id: i64,
    session_id: SessionId,
    seq: i64,
    role: String,
    data: Option<String>,
    created_ms: i64,
}

/// The `LIKE` prefilter is only a performance shortcut for an ASCII needle
/// without `"`, `\` or control bytes. It is applied to stored JSON only
/// alongside the escape/non-ASCII candidate rule in
/// [`Store::search_message_chunk`]: a valid JSON payload may represent the
/// same visible text with `\uXXXX`/`\/` escapes or with a non-ASCII
/// character that lowercases to ASCII, so those payloads are always
/// decoded. Any needle that cannot be prefiltered returns `None` and the
/// bounded chunk is decoded in full.
fn ascii_prefilter_needle(needle: &str) -> Option<String> {
    if !needle.is_ascii()
        || needle
            .bytes()
            .any(|byte| byte < 0x20 || byte == b'"' || byte == b'\\')
    {
        return None;
    }
    let mut pattern = String::with_capacity(needle.len() + 2);
    pattern.push('%');
    for ch in needle.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    Some(pattern)
}

/// True when any VISIBLE field of one durable message/parts pair contains
/// the already-folded search needle. String leaves of a tool call's `input`
/// are visible (rendered as the call's arguments); identifiers/state are
/// not.
fn message_matches(
    message: &MessageRow,
    parts: &[PartRow],
    needle: &str,
    folded: &mut String,
) -> bool {
    if let Some(text) = message.data.get("text").and_then(|v| v.as_str()) {
        if contains_folded(text, needle, folded) {
            return true;
        }
    }
    for part in parts {
        let data = &part.data;
        match part.kind.as_str() {
            "text" | "reasoning" | "summary" => {
                if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                    if contains_folded(text, needle, folded) {
                        return true;
                    }
                }
            }
            "tool_result" => {
                if let Some(excerpt) = data.get("excerpt").and_then(|v| v.as_str()) {
                    if contains_folded(excerpt, needle, folded) {
                        return true;
                    }
                }
            }
            "tool_call" => {
                if let Some(name) = data.get("name").and_then(|v| v.as_str()) {
                    if contains_folded(name, needle, folded) {
                        return true;
                    }
                }
                if let Some(input) = data.get("input") {
                    if visible_value_matches(input, needle, folded) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn visible_value_matches(value: &serde_json::Value, needle: &str, folded: &mut String) -> bool {
    match value {
        serde_json::Value::String(text) => contains_folded(text, needle, folded),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|item| visible_value_matches(item, needle, folded)),
        serde_json::Value::Object(map) => map
            .values()
            .any(|item| visible_value_matches(item, needle, folded)),
        _ => false,
    }
}

/// Case-insensitive substring test over one visible field. The needle is
/// already Unicode-lowercased. ASCII haystacks compare bytes in place; a
/// non-ASCII haystack is folded ONCE into the caller's reused buffer (never
/// one allocation per character) and matched there.
fn contains_folded(haystack: &str, needle: &str, folded: &mut String) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() {
        return needle.is_ascii()
            && haystack
                .as_bytes()
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()));
    }
    folded.clear();
    folded.extend(haystack.chars().flat_map(char::to_lowercase));
    folded.contains(needle)
}

// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    #[test]
    fn message_paging_is_fundamental_and_stable() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap();
        for i in 0..100 {
            store
                .put_message(
                    session.id,
                    i,
                    "user",
                    serde_json::json!({"text": format!("msg {i}")}),
                )
                .unwrap();
        }
        // Newest first, page size 10.
        let page1 = store.messages_before(session.id, None, 10).unwrap();
        assert_eq!(page1.len(), 10);
        assert_eq!(page1[0].seq, 99);
        // Cursor paging reaches everything exactly once.
        let mut seen = vec![];
        let mut cursor = None;
        loop {
            let page = store.messages_before(session.id, cursor, 7).unwrap();
            if page.is_empty() {
                break;
            }
            for m in &page {
                assert!(!seen.contains(&m.seq), "duplicate message in paging");
                seen.push(m.seq);
            }
            cursor = Some(page.last().unwrap().seq);
        }
        assert_eq!(seen.len(), 100);
    }

    /// DOM-47: the `before` cursor is a LIVE strict cut (`seq < before`), not
    /// a snapshot. A concurrent writer that appends above the walk's newest
    /// row interleaves those appends *behind* the cursor and they can never
    /// enter this walk; rows deleted from a not-yet-served middle range leave
    /// holes the cursor skips; a row already served and then deleted is never
    /// duplicated or resurrected. Boundary cursors (`0`, `1`, newest,
    /// newest + 1, far past the tail, limit 0) and termination are pinned.
    #[test]
    fn message_before_walk_survives_concurrent_appends_and_middle_deletes() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        const TOTAL: i64 = 60;
        const PAGE: u64 = 7;
        for seq in 1..=TOTAL {
            store
                .put_message(
                    sid,
                    seq,
                    "user",
                    serde_json::json!({"text": format!("m{seq}")}),
                )
                .unwrap();
        }

        // Strict cursor boundaries before any walk mutates the transcript.
        assert!(
            store.messages_before(sid, Some(0), 10).unwrap().is_empty(),
            "before=0 must be an empty strict cut"
        );
        assert!(
            store.messages_before(sid, Some(1), 10).unwrap().is_empty(),
            "before=1 must be an empty strict cut (seq starts at 1)"
        );
        assert_eq!(
            store
                .messages_before(sid, Some(2), 10)
                .unwrap()
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![1],
            "before=2 cuts exactly above seq 1"
        );
        let past = store.messages_before(sid, Some(TOTAL + 5_000), 6).unwrap();
        assert_eq!(
            past.iter().map(|m| m.seq).collect::<Vec<_>>(),
            (55..=60).rev().collect::<Vec<_>>(),
            "a cursor past the newest seq is the newest page"
        );
        assert!(
            store
                .messages_before_page(sid, None, Some(0))
                .unwrap()
                .is_empty(),
            "limit 0 is an empty bounded page"
        );
        let first = store.messages_before_page(sid, None, Some(PAGE)).unwrap();
        assert_eq!(first.len() as u64, PAGE, "the first page is page-bounded");
        assert_eq!(
            first.iter().map(|m| m.seq).collect::<Vec<_>>(),
            (54..=60).rev().collect::<Vec<_>>()
        );
        assert_eq!(
            store.messages_before(sid, Some(TOTAL), PAGE).unwrap()[0].seq,
            TOTAL - 1,
            "before = the newest seq excludes that row"
        );

        // The writer runs on another thread and appends one row per page,
        // acknowledged before the walk advances, so the interleaving is
        // adversarial but deterministic.
        let (go, wait_go) = std::sync::mpsc::channel::<()>();
        let (appended_tx, appended_rx) = std::sync::mpsc::channel::<i64>();
        let mut appended: Vec<i64> = Vec::new();
        let mut seen: Vec<i64> = Vec::new();
        let mut pages = 0usize;
        let mut before: Option<i64> = None;
        let store_ref = &store;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let store = store_ref;
                let mut next = TOTAL + 1;
                while wait_go.recv().is_ok() {
                    store
                        .put_message(
                            sid,
                            next,
                            "user",
                            serde_json::json!({"text": format!("append {next}")}),
                        )
                        .unwrap();
                    appended_tx.send(next).unwrap();
                    next += 1;
                }
            });
            loop {
                let page = store.messages_before_page(sid, before, Some(PAGE)).unwrap();
                pages += 1;
                assert!(pages <= 20, "the live cursor walk must terminate");
                assert!(
                    page.len() <= PAGE as usize,
                    "a page is bounded by its limit ({} > {PAGE})",
                    page.len()
                );
                if page.is_empty() {
                    break;
                }
                assert!(
                    page.windows(2).all(|w| w[0].seq > w[1].seq),
                    "pages stay strictly newest-first"
                );
                seen.extend(page.iter().map(|m| m.seq));
                before = Some(page.last().unwrap().seq);
                if pages == 1 {
                    // A not-yet-served middle range disappears from the walk.
                    for seq in 20..=29 {
                        assert!(store.delete_message(sid, seq).unwrap());
                    }
                    // A row already served is deleted: it must not duplicate
                    // or resurrect later in the same walk.
                    assert!(store.delete_message(sid, 57).unwrap());
                }
                // Interleave one append behind the cursor before advancing.
                go.send(()).unwrap();
                appended.push(appended_rx.recv().unwrap());
            }
            drop(go);
        });

        assert!(
            !appended.is_empty(),
            "the concurrent writer must have interleaved appends"
        );
        let expected: Vec<i64> = (1..=TOTAL)
            .rev()
            .filter(|seq| !(20..=29).contains(seq))
            .collect();
        assert_eq!(
            seen, expected,
            "every surviving pre-existing row exactly once (no skip/duplicate)"
        );
        assert!(
            appended.iter().all(|seq| !seen.contains(seq)),
            "appends above the walk start never enter an older page"
        );
        let newest = store.messages_before(sid, None, 10).unwrap();
        assert_eq!(
            newest[0].seq,
            *appended.iter().max().unwrap(),
            "the next live walk starts at the appended tail"
        );
        assert_eq!(
            store
                .messages_before(sid, Some(TOTAL + 1), 5)
                .unwrap()
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![60, 59, 58, 56, 55],
            "before = original tail + 1 still excludes every append"
        );
        assert!(
            store.messages_before(sid, Some(1), 10).unwrap().is_empty(),
            "the oldest boundary remains a strict cut after appends/deletes"
        );
        assert_eq!(
            store.messages_before(sid, Some(2), 10).unwrap()[0].seq,
            1,
            "the oldest surviving row is still reachable at before=2"
        );
    }

    /// D-03: Find must reach model answers (stored ONLY as `part` rows), tool
    /// results, and prompts (stored in `message.data`), newest-first, without
    /// a false positive on a different session. This is the regression the
    /// data-only scan silently passed: assistant text lived in parts, so the
    /// newest answer was invisible to transcript search.
    #[test]
    fn transcript_search_reads_message_data_and_part_rows() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let a = store.create_session(ws, "a", "p", "m").unwrap();
        let b = store.create_session(ws, "b", "p", "m").unwrap();
        let prompt = store
            .put_message(
                a.id,
                1,
                "user",
                serde_json::json!({"text": "please inspect the alpha needle"}),
            )
            .unwrap();
        assert!(prompt > 0);
        let answer = store
            .put_message(a.id, 2, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        store
            .put_part(
                answer,
                "text",
                serde_json::json!({"text": "the beta needle is in the answer"}),
            )
            .unwrap();
        let tool = store
            .put_message(a.id, 3, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        store
            .put_part(
                tool,
                "tool_result",
                serde_json::json!({"excerpt": "gamma needle in tool output"}),
            )
            .unwrap();
        let foreign = store
            .put_message(
                b.id,
                1,
                "user",
                serde_json::json!({"text": "delta needle belongs to another session"}),
            )
            .unwrap();
        assert!(foreign > 0);

        let hits = store.search_messages(a.id, "needle", 200).unwrap();
        assert_eq!(hits.len(), 3, "all three local rows must match");
        assert_eq!(
            hits.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        let beta = store.search_messages(a.id, "BETA", 200).unwrap();
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].seq, 2);
        let gamma = store.search_messages(a.id, "gamma", 200).unwrap();
        assert_eq!(gamma.len(), 1);
        assert_eq!(gamma[0].seq, 3);
        assert!(
            store
                .search_messages(a.id, "delta", 200)
                .unwrap()
                .is_empty(),
            "a foreign session's text must never surface"
        );
        assert_eq!(
            store.search_messages(a.id, "needle", 2).unwrap().len(),
            2,
            "the page bound is exact"
        );
    }

    /// The SQL ASCII prefilter must reject non-candidate payloads BEFORE the
    /// serde decode: 2,000 ordinary rows + one match must decode exactly the
    /// candidate row (the old search decoded every row it walked).
    #[test]
    fn transcript_search_prefilter_skips_non_candidate_json_decode() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap();
        for seq in 1..=2_000i64 {
            store
                .put_message(
                    session.id,
                    seq,
                    "user",
                    serde_json::json!({"text": format!("ordinary payload {seq}")}),
                )
                .unwrap();
        }
        store
            .put_message(
                session.id,
                2_001,
                "assistant",
                serde_json::json!({"text": "the unique-needle-42 answer"}),
            )
            .unwrap();
        SEARCH_JSON_DECODES.with(|count| count.set(0));
        let hits = store
            .search_messages(session.id, "unique-needle-42", 50)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 2_001);
        let decoded = SEARCH_JSON_DECODES.with(|count| count.get());
        assert_eq!(
            decoded, 1,
            "only the candidate row may be JSON-decoded (decoded {decoded})"
        );
    }

    /// Unicode folding is per-string, never per-character allocation: the
    /// fully expanded lowercase mapping (İ -> i + combining dot) matches, and
    /// the reused buffer is the only allocation on the non-ASCII path.
    #[test]
    fn unicode_fold_matches_expanded_lowercase_without_per_char_allocations() {
        let needle = "İSTANBUL".to_lowercase();
        let mut folded = String::new();
        assert!(contains_folded(
            "visit İSTANBUL today",
            &needle,
            &mut folded
        ));
        assert!(contains_folded("CAFÉ au lait", "café", &mut folded));
        assert!(contains_folded("ABC", "abc", &mut folded));
        assert!(!contains_folded("ASCII only", "café", &mut folded));
        assert!(folded.capacity() >= 1);
    }

    /// Report row 9: Find matches the VISIBLE decoded text, never the raw
    /// JSON envelope, and folds case across Unicode. A reference oracle
    /// (visible fields only, `to_lowercase`) must agree for every query.
    #[test]
    fn transcript_search_matches_visible_text_and_unicode_not_raw_json() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "plain prompt"}))
            .unwrap();
        store
            .put_message(
                s.id,
                2,
                "user",
                serde_json::json!({"text": "the word text is visible"}),
            )
            .unwrap();
        let answer = store
            .put_message(s.id, 3, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        store
            .put_part(
                answer,
                "text",
                serde_json::json!({"text": "say \"hello\" now"}),
            )
            .unwrap();
        let answer2 = store
            .put_message(s.id, 4, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        store
            .put_part(
                answer2,
                "text",
                serde_json::json!({"text": "résumé: Élève"}),
            )
            .unwrap();
        let tool = store
            .put_message(s.id, 5, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        store
            .put_part(
                tool,
                "tool_call",
                serde_json::json!({
                    "tool_call_id": "call-1",
                    "name": "read_file",
                    "state": "requested",
                    "input": {"path": "src/main.rs"}
                }),
            )
            .unwrap();

        // Raw JSON keys/punctuation are not searchable.
        assert!(
            store
                .search_messages(s.id, "parts", 200)
                .unwrap()
                .is_empty(),
            "the `parts` KEY must not match every assistant row"
        );
        assert!(
            store
                .search_messages(s.id, "tool_call_id", 200)
                .unwrap()
                .is_empty(),
            "JSON keys are not visible text"
        );
        assert_eq!(
            store
                .search_messages(s.id, "text", 200)
                .unwrap()
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![2],
            "only the row whose VISIBLE text says `text` may match"
        );
        assert_eq!(
            store
                .search_messages(s.id, "\"", 200)
                .unwrap()
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![3],
            "a quote matches only visible quotes, never JSON punctuation"
        );
        for query in ["É", "é", "ÉLÈVE", "élève", "RÉSUMÉ"] {
            assert_eq!(
                store
                    .search_messages(s.id, query, 200)
                    .unwrap()
                    .iter()
                    .map(|m| m.seq)
                    .collect::<Vec<_>>(),
                vec![4],
                "Unicode folding must match {query:?}"
            );
        }
        assert_eq!(
            store
                .search_messages(s.id, "SRC/MAIN.RS", 200)
                .unwrap()
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![5],
            "visible tool-call input strings are searchable"
        );
    }

    /// Insert `n` messages with seq 1..=n and a controlled payload size
    /// (the exact persisted JSON bytes are returned per row for byte-bound
    /// tests). All payloads are the same size.
    pub(crate) fn seed_messages(
        store: &Store,
        sid: SessionId,
        n: i64,
        payload_len: usize,
    ) -> Vec<u64> {
        let mut sizes = Vec::new();
        for i in 1..=n {
            let data = serde_json::json!({ "text": "a".repeat(payload_len) });
            sizes.push(serde_json::to_string(&data).unwrap().len() as u64);
            store
                .put_message(
                    sid,
                    i,
                    "user",
                    serde_json::json!({ "text": "a".repeat(payload_len) }),
                )
                .unwrap();
        }
        sizes
    }

    /// (a) exactly at the max_bytes boundary: two 100-byte rows against a
    /// 200-byte budget are both returned; the third row (which would cross
    /// the boundary) stops the walk — and nothing beyond it is read.
    #[test]
    fn bounded_backwards_stops_exactly_at_the_byte_boundary() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        let sizes = seed_messages(&store, sid, 6, 100);
        assert_eq!(
            sizes[0],
            serde_json::to_string(&serde_json::json!({"text": "a".repeat(100)}))
                .unwrap()
                .len() as u64
        );
        let window = store
            .messages_backwards_bounded(sid, None, 10, sizes[0] * 2)
            .unwrap();
        assert_eq!(
            window.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![6, 5],
            "exactly two rows fit the 200-byte budget"
        );
        // The byte total of the returned window never exceeds max_bytes.
        let total: u64 = window
            .iter()
            .map(|r| serde_json::to_string(&r.data).unwrap().len() as u64)
            .sum();
        assert_eq!(total, sizes[0] * 2);
        // Hostile sub-row budget: the newest row alone is returned whole
        // (message granularity — never a partial row, never an empty window
        // when a message exists).
        let one = store
            .messages_backwards_bounded(sid, None, 10, sizes[0] - 1)
            .unwrap();
        assert_eq!(one.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![6]);
        let zero = store.messages_backwards_bounded(sid, None, 10, 0).unwrap();
        assert_eq!(
            zero.len(),
            1,
            "max_bytes = 0 still yields the newest message"
        );
        // max_messages = 0 is the empty contract (no row is ever read).
        assert!(store
            .messages_backwards_bounded(sid, None, 0, u64::MAX)
            .unwrap()
            .is_empty());
    }

    /// (b) 10k messages whose OLD tail has been corrupted into unreadable
    /// blobs: the bounded call must still succeed and return exactly the
    /// newest window — proof it never reads (never materializes) the old
    /// tail. A load-then-trim implementation would hit the corrupt rows and
    /// error. The corruption is proven live by a probe whose bound steps
    /// ONE row past the healthy window: it must fail loudly — the walk
    /// really stops where the bounds say it stops.
    #[test]
    fn bounded_backwards_never_touches_a_corrupted_old_tail() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        seed_messages(&store, sid, 10_000, 40);
        // Corrupt every row below seq 9501: data becomes a BLOB, so reading
        // it as TEXT fails loudly. The newest 500 rows (seq 9501..=10000)
        // stay healthy.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE message SET data = x'FF' WHERE session_id = ?1 AND seq < 9501",
                params![sid.raw() as i64],
            )
            .unwrap();
        }
        // Message-bound window over the healthy region: exactly the newest
        // 500 rows, never stepping into the corrupt tail.
        let window = store
            .messages_backwards_bounded(sid, None, 500, u64::MAX)
            .unwrap();
        assert_eq!(window.len(), 500, "exactly the healthy newest rows");
        assert_eq!(window[0].seq, 10_000, "newest first");
        assert_eq!(window.last().unwrap().seq, 9_501);
        // Byte-bound window stops even earlier, still never touching the
        // corrupt tail.
        let tiny = store
            .messages_backwards_bounded(sid, None, 10_000, 100)
            .unwrap();
        assert_eq!(tiny.len(), 1);
        assert_eq!(tiny[0].seq, 10_000);
        // The corruption is LIVE: a bound that steps one row past the
        // healthy window must fail loudly (never silently return garbage or
        // skip the row).
        let err = store
            .messages_backwards_bounded(sid, None, 501, u64::MAX)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Sqlite(_)),
            "corrupt row is read when the bound demands it: {err:?}"
        );
    }

    /// (b') The deletion variant: old rows removed mid-range leave holes
    /// (paging skips holes; nothing is renumbered). A bounded load over a
    /// hole-riddled tail still returns the newest window deterministically.
    #[test]
    fn bounded_backwards_skips_deleted_holes_in_the_tail() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        seed_messages(&store, sid, 10_000, 30);
        // Delete a mid-range band (seq 4000..=6000): rows below it are
        // physically gone — only a scanner that never goes there survives.
        for seq in 4000..=6000i64 {
            store.delete_message(sid, seq).unwrap();
        }
        let window = store
            .messages_backwards_bounded(sid, None, 10_000, u64::MAX)
            .unwrap();
        assert_eq!(
            window.len(),
            7_999,
            "10000 - 2001 deleted (band 4000..=6000)"
        );
        assert_eq!(window[0].seq, 10_000);
        assert_eq!(window.last().unwrap().seq, 1, "newest-first, hole-free");
        assert!(
            window.windows(2).all(|w| w[0].seq > w[1].seq),
            "strictly newest-first"
        );
    }

    /// (c) before_seq cuts exactly between messages (`seq < before`): seq 5
    /// is excluded, seq 4 is the newest of the window; u64 values above
    /// i64::MAX behave like "no older bound"; 0 and 1 cut below every row.
    #[test]
    fn bounded_backwards_before_seq_cuts_exactly_between_messages() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        seed_messages(&store, sid, 10, 10);
        let window = store
            .messages_backwards_bounded(sid, Some(5), 10_000, u64::MAX)
            .unwrap();
        assert_eq!(
            window.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![4, 3, 2, 1],
            "before = 5 excludes seq 5 itself"
        );
        let cut = store
            .messages_backwards_bounded(sid, Some(5), 2, u64::MAX)
            .unwrap();
        assert_eq!(cut.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![4, 3]);
        // Absurd cursors clamp instead of erroring.
        assert_eq!(
            store
                .messages_backwards_bounded(sid, Some(u64::MAX), 3, u64::MAX)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            vec![10, 9, 8]
        );
        assert!(store
            .messages_backwards_bounded(sid, Some(1), 10_000, u64::MAX)
            .unwrap()
            .is_empty());
        assert!(store
            .messages_backwards_bounded(sid, Some(0), 10_000, u64::MAX)
            .unwrap()
            .is_empty());
        // The cursor can itself be a hole left by deletion: rows with
        // seq < 5 after deleting seq 5..=8 still start at seq 4.
        for seq in 5..=8i64 {
            store.delete_message(sid, seq).unwrap();
        }
        assert_eq!(
            store
                .messages_backwards_bounded(sid, Some(9), 10_000, u64::MAX)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
    }

    /// (d) All history fits: the bounded call returns the full list,
    /// newest-first — identical to an unbounded `messages_before` walk.
    #[test]
    fn bounded_backwards_returns_everything_when_it_fits() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        seed_messages(&store, sid, 250, 20);
        let full = store
            .messages_backwards_bounded(sid, None, u64::MAX, u64::MAX)
            .unwrap();
        assert_eq!(full.len(), 250);
        assert!(full.windows(2).all(|w| w[0].seq > w[1].seq));
        let expected = store.messages_before(sid, None, 250).unwrap();
        assert_eq!(
            full.iter().map(|r| r.seq).collect::<Vec<_>>(),
            expected.iter().map(|r| r.seq).collect::<Vec<_>>()
        );
        // Same content byte-for-byte (data round-trips through the bound).
        for (a, b) in full.iter().zip(expected.iter()) {
            assert_eq!(a.data, b.data);
        }
    }

    /// (e) A message bigger than max_bytes alone is still returned whole —
    /// message granularity is absolute; never a partial row and never a
    /// truncation of the payload.
    #[test]
    fn bounded_backwards_oversized_message_is_returned_whole() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        let big_len = 50_000;
        let big = serde_json::json!({ "blob": "b".repeat(big_len) });
        let big_bytes = serde_json::to_string(&big).unwrap().len() as u64;
        store.put_message(sid, 1, "assistant", big.clone()).unwrap();
        store
            .put_message(sid, 2, "user", serde_json::json!({"text": "x".repeat(30)}))
            .unwrap();
        let window = store.messages_backwards_bounded(sid, None, 10, 64).unwrap();
        assert_eq!(window.len(), 1, "the oversized message alone is returned");
        assert_eq!(window[0].seq, 2, "newest first even when oversized");
        assert_eq!(window[0].data, serde_json::json!({"text": "x".repeat(30)}));
        // Same rule when the oversized message is the ONLY candidate.
        let window = store
            .messages_backwards_bounded(sid, Some(2), 10, 64)
            .unwrap();
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].seq, 1);
        assert_eq!(
            serde_json::to_string(&window[0].data).unwrap().len() as u64,
            big_bytes,
            "payload never truncated"
        );
        assert_eq!(window[0].data, big);
    }

    #[test]
    fn message_created_ms_queries_known_and_unknown() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_message(s.id, 5, "user", serde_json::json!({"text": "hi"}))
            .unwrap();
        let ms = store.message_created_ms(s.id, 5).unwrap().unwrap();
        assert!(ms > 0);
        // The same value the message row itself carries.
        assert_eq!(
            ms,
            store.messages_before(s.id, None, 10).unwrap()[0].created_ms
        );
        // Unknown seq → None, never an error.
        assert_eq!(store.message_created_ms(s.id, 99).unwrap(), None);
        assert_eq!(
            store.message_created_ms(SessionId::new(999), 5).unwrap(),
            None
        );
    }

    #[test]
    fn corrupt_message_and_part_data_return_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let mid = store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "hi"}))
            .unwrap();
        let pid = store
            .put_part(mid, "text", serde_json::json!({"t": "hi"}))
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE message SET data = 'broken{' WHERE id = ?1",
                params![mid],
            )
            .unwrap();
            conn.execute(
                "UPDATE part SET data = 'also broken' WHERE id = ?1",
                params![pid],
            )
            .unwrap();
        }
        match store.messages_before(s.id, None, 10) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt message data must error, not panic: {other:?}"),
        }
        match store.parts_of(mid) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt part data must error, not panic: {other:?}"),
        }
    }

    #[test]
    fn delete_message_removes_rows_and_parts_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let sid = s.id;
        store
            .put_message(sid, 1, "user", serde_json::json!({"text": "a"}))
            .unwrap();
        let m2 = store
            .put_message(sid, 2, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        store
            .put_part(m2, "tool_call", serde_json::json!({"tool_call_id": "c1"}))
            .unwrap();
        store
            .put_part(m2, "text", serde_json::json!({"text": "body"}))
            .unwrap();
        store
            .put_message(sid, 3, "user", serde_json::json!({"text": "b"}))
            .unwrap();
        // Delete the middle message: its part rows go with it.
        assert!(store.delete_message(sid, 2).unwrap());
        assert_eq!(store.message_count(sid).unwrap(), 2);
        assert!(store.parts_of(m2).unwrap().is_empty());
        // No orphan part rows can survive (single transaction).
        let orphans: i64 = {
            let conn = store.read().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM part WHERE message_id NOT IN (SELECT id FROM message)",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(orphans, 0, "parts are removed with their message");
        // Sequences of surviving rows are STABLE (no renumbering).
        let page = store.messages_before(sid, None, 10).unwrap();
        let seqs: Vec<i64> = page.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![3, 1]);
        // Re-removal of the same message deletes nothing and says so.
        assert!(!store.delete_message(sid, 2).unwrap());
        assert!(!store.delete_message(sid, 99).unwrap());
        // The removal is durable across a reopen.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.message_count(sid).unwrap(), 2);
        assert!(store.message_created_ms(sid, 2).unwrap().is_none());
        let page = store.messages_before(sid, None, 10).unwrap();
        let seqs: Vec<i64> = page.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![3, 1]);
    }
}

#[cfg(test)]
mod perf_tests {
    use super::tests::tmp_store;
    use super::*;

    /// Bounded deterministic measurement on 10,000 durable message rows: the
    /// bounded backward page reads only the requested window, decodes every
    /// row correctly (including a hostile oversized newest payload and a
    /// unicode payload under the byte bound), and completes in bounded time.
    #[test]
    fn bounded_backwards_page_is_correct_and_bounded_on_10k_rows() {
        const N: i64 = 10_000;
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        {
            let conn = store.raw_conn();
            let tx = conn.unchecked_transaction().unwrap();
            for i in 1..=N {
                tx.execute(
                    "INSERT INTO message(session_id, seq, role, data, created_ms)
                     VALUES (?1, ?2, 'user', ?3, ?4)",
                    params![
                        sid.raw() as i64,
                        i,
                        format!(r#"{{"text":"payload-{i:05}"}}"#),
                        i
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let started = std::time::Instant::now();
        let page = store
            .messages_backwards_bounded(sid, None, 500, u64::MAX)
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(page.len(), 500);
        assert_eq!(page[0].seq, N);
        assert_eq!(page[499].seq, N - 499);
        for m in &page {
            let expected = format!("payload-{:05}", m.seq);
            assert_eq!(m.data["text"], expected, "row {} decodes exactly", m.seq);
        }
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "500-row page out of 10k took {elapsed:?}"
        );

        // Strict cursor cut: nothing at or above `before_seq` is returned.
        let cut = store
            .messages_backwards_bounded(sid, Some(100), 500, u64::MAX)
            .unwrap();
        assert!(cut.iter().all(|m| m.seq < 100));
        assert_eq!(cut.first().unwrap().seq, 99);

        // Adversarial payloads: an oversized newest row is still exact and
        // the byte bound holds between rows; a unicode row decodes.
        {
            let conn = store.raw_conn();
            let tx = conn.unchecked_transaction().unwrap();
            tx.execute(
                "INSERT INTO message(session_id, seq, role, data, created_ms)
                 VALUES (?1, ?2, 'user', ?3, ?4)",
                params![
                    sid.raw() as i64,
                    N + 1,
                    serde_json::json!({ "text": "x".repeat(64 * 1024) }).to_string(),
                    N + 1
                ],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO message(session_id, seq, role, data, created_ms)
                 VALUES (?1, ?2, 'user', ?3, ?4)",
                params![
                    sid.raw() as i64,
                    N + 2,
                    serde_json::json!({ "text": "π-😀 unicode" }).to_string(),
                    N + 2
                ],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let tiny = store
            .messages_backwards_bounded(sid, None, 500, 64)
            .unwrap();
        assert_eq!(tiny.len(), 1, "the byte bound stops between rows");
        assert_eq!(tiny[0].seq, N + 2);
        assert_eq!(tiny[0].data["text"], "π-😀 unicode");
        let next = store
            .messages_backwards_bounded(sid, Some((N + 2) as u64), 1, 64)
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].seq, N + 1);
        assert_eq!(next[0].data["text"].as_str().unwrap().len(), 64 * 1024);
    }

    /// NEW long-task probe: the turn-path window charges PART bytes, returns
    /// parts in the same call, never skips a newer oversized row in favour
    /// of older smaller rows, and honors the checkpoint lower bound.
    #[test]
    fn window_with_parts_charges_part_bytes_and_never_skips_the_newest_big_row() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        let ancient = store
            .put_message(sid, 1, "assistant", serde_json::json!({"parts":[]}))
            .unwrap();
        store
            .put_part(ancient, "text", serde_json::json!({"text": "ANCIENT"}))
            .unwrap();
        let older = store
            .put_message(sid, 2, "assistant", serde_json::json!({"parts":[]}))
            .unwrap();
        store
            .put_part(
                older,
                "text",
                serde_json::json!({"text": "y".repeat(50_000)}),
            )
            .unwrap();
        store
            .put_message(sid, 3, "user", serde_json::json!({"text": "the prompt"}))
            .unwrap();
        // Bound 64 bytes: the newest prompt fits, the 50 KiB part does not —
        // it is still taken WHOLE (one over-bound row) and the window stops.
        let window = store
            .messages_window_with_parts(sid, None, None, 10, 64)
            .unwrap();
        assert_eq!(window.len(), 2, "newest + one over-bound row, then STOP");
        assert_eq!(window[0].0.seq, 3);
        assert_eq!(window[1].0.seq, 2);
        assert_eq!(window[1].1.len(), 1, "parts are batched, not N+1");
        assert!(window[1].1[0].data["text"].as_str().unwrap().len() == 50_000);
        assert!(
            window.iter().all(|(m, _)| m.seq != 1),
            "older rows must never be read past the over-bound row"
        );
        // Checkpoint lower bound: seq > 1 excludes the summarized row.
        let after = store
            .messages_window_with_parts(sid, None, Some(1), 10, 64)
            .unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].0.seq, 3);
        assert_eq!(after[1].0.seq, 2);
    }
}

#[cfg(test)]
mod search_scale_tests {
    use super::*;
    use std::time::Instant;

    /// ([fault]) 50k-row transcript probe:
    /// `cargo test -p faktor-store fault_transcript_search_scan -- --ignored --nocapture`.
    /// The prefiltered streaming search must decode only the matching row and
    /// stay far below the historical whole-window decode walk it replaces.
    #[test]
    #[ignore = "[fault] transcript search scale probe; run explicitly with --ignored"]
    fn fault_transcript_search_scan_is_streaming_and_prefiltered() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap().id;
        const ROWS: i64 = 50_000;
        let filler = "ordinary transcript text without the marker ".repeat(20);
        let seed = Instant::now();
        for seq in 1..=ROWS {
            store
                .put_message(
                    session,
                    seq,
                    "user",
                    serde_json::json!({ "text": format!("{filler}{seq}") }),
                )
                .unwrap();
        }
        store
            .put_message(
                session,
                ROWS + 1,
                "assistant",
                serde_json::json!({ "text": "the unique-marker-777 answer" }),
            )
            .unwrap();
        eprintln!(
            "[fault] seeded {} messages in {:?}",
            ROWS + 1,
            seed.elapsed()
        );

        SEARCH_JSON_DECODES.with(|count| count.set(0));
        let started = Instant::now();
        let hits = store
            .search_messages(session, "unique-marker-777", 50)
            .unwrap();
        let search = started.elapsed();
        assert_eq!(hits.len(), 1);
        let decoded = SEARCH_JSON_DECODES.with(|count| count.get());

        // Historical shape: decode every message window (and its parts)
        // until the page is full.
        let started = Instant::now();
        let mut before: Option<u64> = None;
        let mut walked = 0usize;
        loop {
            let page = store
                .messages_window_with_parts(session, before, None, 256, u64::MAX)
                .unwrap();
            if page.is_empty() {
                break;
            }
            walked += page.len();
            let last = page.last().unwrap().0.seq as u64;
            if page.len() < 256 {
                break;
            }
            before = Some(last);
        }
        let decode_walk = started.elapsed();
        let conn = Connection::open(store.path()).unwrap();
        for (name, sql, param) in [
            (
                "instr(lower)",
                "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND instr(lower(data), ?2) > 0",
                "unique-marker-777",
            ),
            (
                "LIKE",
                "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND data LIKE ?2 ESCAPE '\\'",
                "%unique-marker-777%",
            ),
            (
                "instr raw",
                "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND instr(data, ?2) > 0",
                "unique-marker-777",
            ),
        ] {
            let started = Instant::now();
            let count: i64 = conn
                .query_row(sql, rusqlite::params![session.raw() as i64, param], |row| {
                    row.get(0)
                })
                .unwrap();
            eprintln!(
                "[fault] {name} scan {count} hits in {:?}",
                started.elapsed()
            );
        }
        eprintln!(
            "[fault] prefiltered search {search:?} (decoded {decoded}); whole-window decode walk {decode_walk:?} over {walked} rows"
        );
        assert_eq!(decoded, 1, "only the matching payload may be decoded");
        assert!(walked as i64 >= ROWS);
    }
}

#[cfg(test)]
mod search_corruption_tests {
    use super::*;

    /// A corrupt CANDIDATE row is still loud: the prefilter only skips rows
    /// whose bytes cannot match, never hides a decode failure for a row that
    /// the search would return.
    #[test]
    fn transcript_search_refuses_a_corrupt_matching_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap().id;
        let id = store
            .put_message(
                session,
                1,
                "assistant",
                serde_json::json!({ "text": "needle in a valid row first" }),
            )
            .unwrap();
        store
            .raw_conn()
            .execute(
                "UPDATE message SET data = '{\"text\":\"needle truncated' WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
        let error = store
            .search_messages(session, "needle", 10)
            .expect_err("a corrupt matching row must refuse typed");
        assert!(
            matches!(error, StoreError::Corrupt(_) | StoreError::Malformed(_)),
            "expected typed corruption, got {error:?}"
        );
    }
}

#[cfg(test)]
mod messages_perf_probe {
    use super::*;

    /// ([perf]) 400-message page with 4 parts each: the historical handler
    /// ran `messages_before` plus ONE `parts_of` query per message (401
    /// SQLite round trips); the batched window is 2. Measured here on the
    /// same rows so the improvement is attributable, not inferred.
    ///
    /// `cargo test -p faktor-store --release messages_page_parts_batch -- --ignored --nocapture`
    #[test]
    #[ignore = "[perf] measured probe; run explicitly with --ignored --nocapture"]
    fn messages_page_parts_batch_is_much_cheaper_than_per_row_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap().id;
        const ROWS: i64 = 400;
        for seq in 1..=ROWS {
            let id = store
                .put_message(session, seq, "user", serde_json::json!({"text": "turn"}))
                .unwrap();
            for part in 0..4 {
                store
                    .put_part(id, "text", serde_json::json!({"i": part}))
                    .unwrap();
            }
        }
        let legacy = || {
            let started = std::time::Instant::now();
            let rows = store.messages_before(session, None, ROWS as u64).unwrap();
            let mut parts = 0usize;
            for row in &rows {
                parts += store.parts_of(row.id).unwrap().len();
            }
            (started.elapsed(), rows.len(), parts)
        };
        let batched = || {
            let started = std::time::Instant::now();
            let rows = store
                .messages_window_with_parts(session, None, None, ROWS as u64, u64::MAX)
                .unwrap();
            (
                started.elapsed(),
                rows.len(),
                rows.iter().map(|(_, p)| p.len()).sum(),
            )
        };
        // Warm the page cache, then take the best of three each.
        let _ = legacy();
        let _ = batched();
        let mut legacy_best = std::time::Duration::MAX;
        let mut batch_best = std::time::Duration::MAX;
        for _ in 0..3 {
            let (t, rows, parts) = legacy();
            assert_eq!((rows, parts), (ROWS as usize, ROWS as usize * 4));
            legacy_best = legacy_best.min(t);
            let (t, rows, parts) = batched();
            assert_eq!((rows, parts), (ROWS as usize, ROWS as usize * 4));
            batch_best = batch_best.min(t);
        }
        println!(
            "[perf] 400 rows x 4 parts: per-row parts_of {legacy_best:?} vs batched {batch_best:?} \
             (queries 401 -> 2)"
        );
        assert!(
            batch_best <= legacy_best,
            "batching regressed: legacy {legacy_best:?} batched {batch_best:?}"
        );
    }
}
