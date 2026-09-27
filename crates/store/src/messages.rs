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
    let id = r.get::<_, i64>(0)?;
    Ok(MessageRow {
        id,
        session_id: id_field(&format!("message {id} session_id"), r.get::<_, i64>(1)?)?,
        seq: r.get(2)?,
        role: r.get(3)?,
        data: parse_json(&format!("message {id} data"), &r.get::<_, String>(4)?)?,
        created_ms: r.get(5)?,
    })
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
        let conn = self.read()?;
        let (sql, params) = match before_seq {
            Some(b) => (
                "SELECT id, session_id, seq, role, data, created_ms FROM message
                 WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
                vec![session_id.raw() as i64, b, limit as i64],
            ),
            None => (
                "SELECT id, session_id, seq, role, data, created_ms FROM message
                 WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
                vec![session_id.raw() as i64, limit as i64],
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
    /// stepped lazily over the `idx_message_session_seq` backward index and
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
            out.push(message_map(row)?);
            total_bytes = total_bytes.saturating_add(row_bytes);
        }
        Ok(out)
    }

    pub fn put_message(
        &self,
        session_id: SessionId,
        seq: i64,
        role: &str,
        data: serde_json::Value,
    ) -> StoreResult<i64> {
        let role = role.to_owned();
        // Preparation BEFORE enqueueing: the message JSON body.
        let data_json = data.to_string();
        self.writer.execute("put_message", move |conn| {
            Self::insert_message_on(conn, session_id, seq, &role, &data_json)
        })
    }

    /// Shared single-row message insert (fixed-arity contract). Runs on the
    /// caller's connection: the actor batch executes it inside one grouped
    /// transaction, the direct path outside any explicit transaction.
    pub(crate) fn insert_message_on(
        conn: &Connection,
        session_id: SessionId,
        seq: i64,
        role: &str,
        data_json: &str,
    ) -> StoreResult<i64> {
        conn.execute(
            "INSERT INTO message(session_id, seq, role, data, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id.raw() as i64, seq, role, data_json, now_ms()],
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
        // Preparation BEFORE enqueueing: the part JSON body.
        let data_json = data.to_string();
        self.writer.execute("put_part", move |conn| {
            Self::insert_part_on(conn, message_id, &kind, &data_json)
        })
    }

    /// Shared single-row part insert; see [`Self::insert_message_on`].
    pub(crate) fn insert_part_on(
        conn: &Connection,
        message_id: i64,
        kind: &str,
        data_json: &str,
    ) -> StoreResult<i64> {
        conn.execute(
            "INSERT INTO part(message_id, kind, data, created_ms) VALUES (?1, ?2, ?3, ?4)",
            params![message_id, kind, data_json, now_ms()],
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

    // ---------------------------------------------------------------- task ledger
}

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
