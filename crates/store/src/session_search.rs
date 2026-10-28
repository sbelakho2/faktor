//! `session_search`: bounded durable session-title search.

use super::*;

impl Store {
    /// Durable bounded title search over every session (D-04/History): a
    /// case-insensitive substring scan ordered newest-first. Unknown search
    /// is never interpreted as SQL; the query is capped before the statement.
    pub fn search_session_titles(&self, query: &str, limit: u64) -> StoreResult<Vec<SessionRow>> {
        if query.is_empty() || query.len() > 256 {
            return Err(StoreError::Malformed(
                "session title search query must be 1..=256 bytes".into(),
            ));
        }
        let limit = i64::try_from(limit.clamp(1, 1000)).map_err(|_| {
            StoreError::Oversized("session title search page exceeds SQLite signed range".into())
        })?;
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms, listing_revision
             FROM session WHERE instr(lower(title), lower(?1)) > 0
             ORDER BY listing_revision DESC, id DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![query, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(session_row_map(row)?);
        }
        Ok(out)
    }
}

// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir
#[cfg(test)]
mod oracle_tests {
    use super::*;

    /// One transcript row's VISIBLE decoded text, owned by the test (never
    /// read back through a production helper).
    struct RowOracle {
        seq: i64,
        visible: Vec<String>,
    }

    /// Independent reference: case-insensitive substring over the visible
    /// decoded strings, newest-first, capped exactly at `limit`. This is the
    /// definition the production search documents; it shares no helper,
    /// folding routine, SQL, or decode path with the store.
    fn oracle_matches(rows: &[RowOracle], query: &str, limit: usize) -> Vec<i64> {
        let needle = query.to_lowercase();
        let mut out = Vec::new();
        for row in rows.iter().rev() {
            if row
                .visible
                .iter()
                .any(|text| text.to_lowercase().contains(&needle))
            {
                out.push(row.seq);
                if out.len() == limit {
                    break;
                }
            }
        }
        out
    }

    const KELVIN: &str = "\u{212A}";

    #[test]
    fn transcript_search_agrees_with_an_independent_visible_text_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "search", "p", "m").unwrap().id;

        let mut rows: Vec<RowOracle> = Vec::new();
        let mut put = |seq: i64,
                       message: serde_json::Value,
                       parts: &[(&str, serde_json::Value)],
                       visible: &[&str]|
         -> i64 {
            let id = store.put_message(sid, seq, "user", message).unwrap();
            for (kind, data) in parts {
                store.put_part(id, kind, data.clone()).unwrap();
            }
            rows.push(RowOracle {
                seq,
                visible: visible.iter().map(|text| (*text).to_string()).collect(),
            });
            id
        };

        put(
            1,
            serde_json::json!({"text": "Plain Prompt with WORD casing"}),
            &[],
            &["Plain Prompt with WORD casing"],
        );
        // JSON-escaped quote: querying `"` must match the visible quote only.
        put(
            2,
            serde_json::json!({"text": "say \"hello\" now", "hidden_note": "secretpocket"}),
            &[],
            &["say \"hello\" now"],
        );
        put(
            3,
            serde_json::json!({"parts": []}),
            &[(
                "text",
                serde_json::json!({"text": "résumé of Élève 😀 東京"}),
            )],
            &["résumé of Élève 😀 東京"],
        );
        // tool_result: only `excerpt` is visible; its id field is not.
        put(
            4,
            serde_json::json!({"parts": []}),
            &[(
                "tool_result",
                serde_json::json!({
                    "excerpt": "tool result mentions NEEDLE",
                    "tool_call_id": "call-hidden-id"
                }),
            )],
            &["tool result mentions NEEDLE"],
        );
        // tool_call: the name and string leaves of `input` are visible;
        // `state`, `tool_call_id`, keys inside `input`, and non-string leaves
        // are not.
        put(
            5,
            serde_json::json!({"parts": []}),
            &[(
                "tool_call",
                serde_json::json!({
                    "name": "read_file",
                    "state": "requested",
                    "tool_call_id": "call-2",
                    "input": {
                        "path": "src/main.rs",
                        "secretkey": "hidden-value",
                        "nested": {"leaf": "deep-leaf"},
                        "ratio": 7,
                        "enabled": true
                    }
                }),
            )],
            &["read_file", "src/main.rs", "hidden-value", "deep-leaf"],
        );
        put(
            6,
            serde_json::json!({"parts": []}),
            &[(
                "summary",
                serde_json::json!({"text": "SUMMARY marker Straße"}),
            )],
            &["SUMMARY marker Straße"],
        );
        // Unknown part kinds are not rendered text.
        put(
            7,
            serde_json::json!({"parts": []}),
            &[(
                "provider_meta",
                serde_json::json!({"text": "invisible-meta-token"}),
            )],
            &[],
        );

        // Valid JSON that does NOT contain the visible text literally:
        // `\u00e9`/`\u0061` escapes decode to "café abc".
        let escaped_unicode = put(
            8,
            serde_json::json!({"text": "placeholder"}),
            &[],
            &["café abc"],
        );
        store
            .raw_conn()
            .execute(
                "UPDATE message SET data = ?1 WHERE id = ?2",
                rusqlite::params![r#"{"text":"caf\u00e9 \u0061bc"}"#, escaped_unicode],
            )
            .unwrap();

        // `\/` is a legal JSON escape for `/`.
        let escaped_solidus = put(
            9,
            serde_json::json!({"text": "placeholder"}),
            &[],
            &["path a/b token"],
        );
        store
            .raw_conn()
            .execute(
                "UPDATE message SET data = ?1 WHERE id = ?2",
                rusqlite::params![r#"{"text":"path a\/b token"}"#, escaped_solidus],
            )
            .unwrap();

        // U+212A KELVIN SIGN lowercases to ASCII `k`; the literal needle
        // never occurs in the stored UTF-8 payload.
        let kelvin_text = format!("kelvin-symbol-{KELVIN}");
        put(
            10,
            serde_json::json!({"text": kelvin_text.clone()}),
            &[],
            &[&kelvin_text],
        );

        put(
            11,
            serde_json::json!({"text": "boundary"}),
            &[],
            &["boundary"],
        );
        put(
            12,
            serde_json::json!({"parts": []}),
            &[(
                "text",
                serde_json::json!({"text": "part text visible only"}),
            )],
            &["part text visible only"],
        );
        put(13, serde_json::json!({"text": ""}), &[], &[]);
        put(
            14,
            serde_json::json!({"text": "visit İSTANBUL today"}),
            &[],
            &["visit İSTANBUL today"],
        );
        put(
            15,
            serde_json::json!({"text": "windows C:\\tmp\\x"}),
            &[],
            &["windows C:\\tmp\\x"],
        );
        put(
            16,
            serde_json::json!({"text": "percent 100% done"}),
            &[],
            &["percent 100% done"],
        );
        put(
            17,
            serde_json::json!({"text": "under_score and 100%"}),
            &[],
            &["under_score and 100%"],
        );

        let search = |query: &str, limit: u64| -> Vec<i64> {
            store
                .search_messages(sid, query, limit)
                .unwrap()
                .iter()
                .map(|row| row.seq)
                .collect()
        };

        // Every query is checked against the independent oracle, including
        // raw-JSON keys/punctuation, escaped payloads, Unicode folding,
        // boundary substrings, wildcard-ish bytes and tool-result text.
        for query in [
            "WORD",
            "word",
            "\"hello\"",
            "\"",
            "say",
            "text",
            "parts",
            "tool_call_id",
            "hidden_note",
            "secretpocket",
            "call-hidden-id",
            "requested",
            "call-2",
            "secretkey",
            "hidden-value",
            "7",
            "true",
            "assistant",
            "user",
            "invisible-meta-token",
            "provider_meta",
            "café",
            "CAFÉ",
            "abc",
            "a/b",
            "path",
            "symbol-k",
            "kelvin-symbol",
            "résumé",
            "ÉLÈVE",
            "élève",
            "😀",
            "東京",
            "Straße",
            "STRAßE",
            "STRASSE",
            "strasse",
            "İSTANBUL",
            "i̇stanbul",
            "istanbul",
            "boundary",
            "bound",
            "ary",
            "boundaries",
            "src/main.rs",
            "SRC/MAIN.RS",
            "deep-leaf",
            "NEEDLE",
            "tool result",
            "100%",
            "%",
            "_",
            "C:\\tmp",
            "C:",
            "e",
        ] {
            assert_eq!(
                search(query, 200),
                oracle_matches(&rows, query, 200),
                "production search disagreed with the visible-text oracle for {query:?}"
            );
        }

        // Pin the anti-raw-JSON and anti-ASCII-only-lowering claims directly
        // so a vacuous oracle can never hide the drift.
        for raw_only in [
            "parts",
            "tool_call_id",
            "hidden_note",
            "secretpocket",
            "call-hidden-id",
            "requested",
            "call-2",
            "secretkey",
            "7",
            "true",
            "assistant",
            "user",
            "invisible-meta-token",
            "provider_meta",
            "boundaries",
            "STRASSE",
            "strasse",
            "istanbul",
        ] {
            assert!(
                search(raw_only, 200).is_empty(),
                "{raw_only:?} occurs only in stored JSON/structure, not in visible text"
            );
        }
        assert_eq!(search("\"", 200), vec![2], "only visible quotes match");
        assert_eq!(search("text", 200), vec![12], "only visible `text` matches");
        assert_eq!(
            search("abc", 200),
            vec![8],
            "\\u0061 decodes to a visible `a`"
        );
        assert_eq!(search("a/b", 200), vec![9], "\\/ decodes to a visible `/`");
        assert_eq!(
            search("symbol-k", 200),
            vec![10],
            "U+212A lowercases to ASCII `k`"
        );
        assert_eq!(search("İSTANBUL", 200), vec![14]);
        assert_eq!(search("Straße", 200), vec![6]);
        assert_eq!(search("100%", 200), vec![17, 16]);
        assert_eq!(
            search("%", 200),
            vec![17, 16],
            "`%` is literal, not a wildcard"
        );
        assert_eq!(
            search("_", 200),
            vec![17, 5],
            "`_` is literal, not a wildcard (read_file also contains one)"
        );

        // Page bound is exact over MATCHES, newest-first, and clamps to >= 1.
        assert_eq!(search("e", 0), oracle_matches(&rows, "e", 1));
        assert_eq!(search("e", 3), oracle_matches(&rows, "e", 3));
        assert_eq!(
            search("e", 200).len(),
            oracle_matches(&rows, "e", 200).len()
        );

        // Query boundaries: empty/oversized refuse typed; exactly 256 bytes
        // is accepted.
        assert!(matches!(
            store.search_messages(sid, "", 10),
            Err(StoreError::Malformed(_))
        ));
        let too_long = "x".repeat(257);
        assert!(matches!(
            store.search_messages(sid, &too_long, 10),
            Err(StoreError::Malformed(_))
        ));
        assert!(store
            .search_messages(sid, &"x".repeat(256), 10)
            .unwrap()
            .is_empty());

        // Search never crosses the session boundary.
        let other = store.create_session(ws, "other", "p", "m").unwrap().id;
        store
            .put_message(
                other,
                1,
                "user",
                serde_json::json!({"text": "azure-marker belongs here"}),
            )
            .unwrap();
        assert!(store
            .search_messages(sid, "azure-marker", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .search_messages(other, "azure-marker", 10)
                .unwrap()
                .iter()
                .map(|row| row.seq)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }
}
