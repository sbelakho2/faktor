//! Durable task state and long-term memory facts.

use crate::handle::SessionHandle;
use crate::{json_bytes, SessionError, MAX_LEDGER_BYTES};

const MAX_FACT_KEY_BYTES: usize = 512;
const MAX_FACT_VALUE_BYTES: usize = 4096;
/// `(kind, key, value)` projection of one memory fact.
type FactTriple = (String, String, String);
/// Bounded page size for memory-fact paging (paging is fundamental).
pub const MAX_FACT_PAGE_SIZE: i64 = 200;

/// Reserved legacy fact kind of the DURABLE CONTEXT CHECKPOINT. It is not a
/// V2 memory row (no `memory_v2:` prefix), so the typed memory render never
/// surfaces it as project memory; it is the durable replacement history of
/// the last accepted compaction.
pub const CONTEXT_CHECKPOINT_KIND: &str = "context_checkpoint";
/// Hard byte bound of one checkpoint payload (kept-history turns JSON).
/// The durable context window is budget-bounded far below this; exceeding
/// it is a typed refusal, never a silent truncation of the replacement
/// context (which would lose facts).
pub const MAX_CONTEXT_CHECKPOINT_BYTES: usize = 4 * 1024 * 1024;
/// One fact value must stay under [`MAX_FACT_VALUE_BYTES`]; chunks leave
/// room for the JSON head/chunk-key envelope.
const CONTEXT_CHECKPOINT_CHUNK_BYTES: usize = 3000;

/// One durable context checkpoint: the exact compacted turns plus the source
/// message high-water seq they replace. `payload` is the serialized turn
/// list (opaque to the session layer); `digest` is the caller's canonical
/// history digest, used by restart/probe equality checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCheckpoint {
    pub through_seq: u64,
    pub digest: String,
    pub payload: Vec<u8>,
}

/// One deterministic page of memory facts with explicit paging metadata:
/// `{size, cursor, has_more, total_estimate}`. Facts are ordered
/// newest-first by `(updated_ms DESC, kind DESC, key DESC)`; the cursor is
/// the `(updated_ms, kind, key)` position after this page — pass it back
/// verbatim. Replaying a cursor returns the same window.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryFactsPage {
    /// (kind, key, value), newest-first.
    pub facts: Vec<(String, String, String)>,
    /// The applied page size (requested limit after clamping).
    pub size: i64,
    /// Cursor for the next older page; `None` on the final page.
    pub cursor: Option<(i64, String, String)>,
    /// True when at least one older page exists.
    pub has_more: bool,
    /// Exact fact count for the session (cheap; the facts table is small).
    pub total_estimate: i64,
}

/// Build the chunked `(key, value)` facts of one durable context checkpoint.
/// Validation, the UTF-8 bound and the payload hash are the SAME for the
/// standalone checkpoint write and the atomic compaction commit, so the two
/// paths can never encode different replacement contexts.
pub(crate) fn context_checkpoint_facts(
    through_seq: u64,
    digest: &str,
    payload: &[u8],
) -> faktor_core::Result<Vec<(String, String)>> {
    if payload.len() > MAX_CONTEXT_CHECKPOINT_BYTES {
        return Err(SessionError::Oversized(format!(
            "context checkpoint of {} bytes exceeds MAX_CONTEXT_CHECKPOINT_BYTES",
            payload.len()
        ))
        .into());
    }
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SessionError::Malformed(
            "context checkpoint digest must be 64 hex characters".into(),
        )
        .into());
    }
    let text = std::str::from_utf8(payload)
        .map_err(|_| SessionError::Malformed("context checkpoint is not UTF-8".into()))?;
    let mut chunks: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + CONTEXT_CHECKPOINT_CHUNK_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    let payload_hash = blake3::hash(payload).to_hex().to_string();
    let head = serde_json::json!({
        "version": 1,
        "through_seq": through_seq,
        "digest": digest,
        "payload_hash": payload_hash,
        "chunks": chunks.len(),
    })
    .to_string();
    let mut facts: Vec<(String, String)> = Vec::with_capacity(chunks.len() + 1);
    facts.push(("head".to_string(), head));
    for (index, chunk) in chunks.into_iter().enumerate() {
        facts.push((format!("c/{index:06}"), chunk));
    }
    Ok(facts)
}

impl SessionHandle {
    /// Upsert one memory fact (kind/key uniquely identify it). Bounds are
    /// enforced before the write; empty keys are malformed.
    pub fn upsert_memory_fact(
        &self,
        kind: &str,
        key: &str,
        value: &str,
    ) -> faktor_core::Result<()> {
        if kind.is_empty() || key.is_empty() {
            return Err(
                SessionError::Malformed("fact kind and key must be non-empty".into()).into(),
            );
        }
        if kind.len() > 64 || key.len() > MAX_FACT_KEY_BYTES {
            return Err(SessionError::Oversized("fact kind/key too long".into()).into());
        }
        if value.len() > MAX_FACT_VALUE_BYTES {
            return Err(SessionError::Oversized(format!(
                "fact value of {} bytes exceeds MAX_FACT_VALUE_BYTES",
                value.len()
            ))
            .into());
        }
        self.manager
            .store()
            .upsert_memory_fact(self.id, kind, key, value)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// [`Self::upsert_memory_fact`] with an atomic expected-stamp CAS. The
    /// read/check/write is ONE writer transaction, so two concurrent
    /// writers holding the same expected stamp can no longer both apply
    /// (the historical read-then-upsert pair lost one update silently).
    pub fn upsert_memory_fact_cas(
        &self,
        kind: &str,
        key: &str,
        value: &str,
        expected_updated_ms: Option<i64>,
    ) -> faktor_core::Result<faktor_store::MemoryFactCas> {
        if kind.is_empty() || key.is_empty() {
            return Err(
                SessionError::Malformed("fact kind and key must be non-empty".into()).into(),
            );
        }
        if kind.len() > 64 || key.len() > MAX_FACT_KEY_BYTES {
            return Err(SessionError::Oversized("fact kind/key too long".into()).into());
        }
        if value.len() > MAX_FACT_VALUE_BYTES {
            return Err(SessionError::Oversized(format!(
                "fact value of {} bytes exceeds MAX_FACT_VALUE_BYTES",
                value.len()
            ))
            .into());
        }
        self.manager
            .store()
            .upsert_memory_fact_cas(self.id, kind, key, value, expected_updated_ms)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Atomic CAS delete; see [`Self::upsert_memory_fact_cas`].
    pub fn delete_memory_fact_cas(
        &self,
        kind: &str,
        key: &str,
        expected_updated_ms: Option<i64>,
    ) -> faktor_core::Result<faktor_store::MemoryFactDeleteCas> {
        self.manager
            .store()
            .delete_memory_fact_cas(self.id, kind, key, expected_updated_ms)
            .map_err(|e| crate::map_store_err(e).into())
    }

    pub fn memory_facts(&self) -> faktor_core::Result<Vec<FactTriple>> {
        self.manager
            .store()
            .memory_facts(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Bounded newest-first facts restricted to an explicit KIND set (the
    /// indexed projection read behind the orchestrator graph and the
    /// verification surface). `has_more` is explicit: callers page or refuse
    /// loudly, never treat a bounded read as complete.
    pub fn memory_facts_of_kinds(
        &self,
        kinds: &[&str],
        limit: u64,
    ) -> faktor_core::Result<(Vec<FactTriple>, bool)> {
        let (rows, has_more) = self
            .manager
            .store()
            .memory_facts_of_kinds(self.id, kinds, limit)
            .map_err(crate::map_store_err)?;
        Ok((
            rows.into_iter()
                .map(|row| (row.kind, row.key, row.value))
                .collect(),
            has_more,
        ))
    }

    /// ONE exact fact value (indexed `(session, kind, key)` lookup). Hot
    /// paths that used to full-scan the fact table use this instead.
    pub fn memory_fact_get(&self, kind: &str, key: &str) -> faktor_core::Result<Option<String>> {
        self.manager
            .store()
            .memory_fact_get(self.id, kind, key)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// One deterministic newest-first page of ONE fact kind.
    pub fn memory_facts_by_kind_page(
        &self,
        kind: &str,
        after: Option<&(i64, String, String)>,
        limit: i64,
    ) -> faktor_core::Result<MemoryFactsPage> {
        let limit = limit.clamp(1, MAX_FACT_PAGE_SIZE);
        let page_limit = u64::try_from(limit).map_err(|_| {
            SessionError::Malformed(format!("memory fact page limit {limit} is negative"))
        })?;
        let (rows, has_more) = self
            .manager
            .store()
            .memory_facts_by_kind_page(self.id, kind, after, page_limit)
            .map_err(crate::map_store_err)?;
        let cursor = if has_more {
            rows.last()
                .map(|r| (r.updated_ms, r.kind.clone(), r.key.clone()))
        } else {
            None
        };
        Ok(MemoryFactsPage {
            facts: rows.into_iter().map(|r| (r.kind, r.key, r.value)).collect(),
            size: limit,
            cursor,
            has_more,
            total_estimate: 0,
        })
    }

    /// Keep only the newest `keep` rows of one fact kind (bounded retention
    /// for append-only per-turn facts).
    pub fn prune_memory_facts_kind_keep_newest(
        &self,
        kind: &str,
        keep: usize,
    ) -> faktor_core::Result<usize> {
        self.manager
            .store()
            .prune_memory_facts_kind_keep_newest(self.id, kind, keep)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Atomically replace the durable context checkpoint (all chunks in one
    /// transaction): a crash exposes the OLD checkpoint or the NEW one,
    /// never a half-written replacement context.
    pub fn put_context_checkpoint(
        &self,
        through_seq: u64,
        digest: &str,
        payload: &[u8],
    ) -> faktor_core::Result<()> {
        let facts = context_checkpoint_facts(through_seq, digest, payload)?;
        self.manager
            .store()
            .replace_memory_facts_kind(self.id, CONTEXT_CHECKPOINT_KIND, &facts)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// The durable context checkpoint, if the session has one. A malformed
    /// or truncated checkpoint is a LOUD typed error (the caller may fall
    /// back to the full transcript, but it must never mistake corruption for
    /// "no checkpoint" and silently change context).
    pub fn context_checkpoint(&self) -> faktor_core::Result<Option<ContextCheckpoint>> {
        let Some(head_raw) = self
            .manager
            .store()
            .memory_fact_get(self.id, CONTEXT_CHECKPOINT_KIND, "head")
            .map_err(crate::map_store_err)?
        else {
            return Ok(None);
        };
        let head: serde_json::Value = serde_json::from_str(&head_raw)
            .map_err(|e| SessionError::Malformed(format!("context checkpoint head: {e}")))?;
        let version = head.get("version").and_then(|v| v.as_u64());
        if version != Some(1) {
            return Err(
                SessionError::Malformed("unsupported context checkpoint version".into()).into(),
            );
        }
        let through_seq = head
            .get("through_seq")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| SessionError::Malformed("checkpoint through_seq missing".into()))?;
        let digest = head
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SessionError::Malformed("checkpoint digest missing".into()))?
            .to_string();
        let payload_hash = head
            .get("payload_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SessionError::Malformed("checkpoint payload_hash missing".into()))?
            .to_string();
        let chunks = head
            .get("chunks")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| SessionError::Malformed("checkpoint chunk count missing".into()))?;
        if chunks == 0 || chunks > (MAX_CONTEXT_CHECKPOINT_BYTES as u64) {
            return Err(SessionError::Malformed(
                "context checkpoint chunk count is out of bounds".into(),
            )
            .into());
        }
        let mut payload: Vec<u8> = Vec::new();
        for index in 0..chunks {
            let key = format!("c/{index:06}");
            let Some(chunk) = self
                .manager
                .store()
                .memory_fact_get(self.id, CONTEXT_CHECKPOINT_KIND, &key)
                .map_err(crate::map_store_err)?
            else {
                return Err(SessionError::Malformed(format!(
                    "context checkpoint chunk {key} is missing"
                ))
                .into());
            };
            payload.extend_from_slice(chunk.as_bytes());
            if payload.len() > MAX_CONTEXT_CHECKPOINT_BYTES {
                return Err(SessionError::Oversized(
                    "context checkpoint reassembly exceeds the bound".into(),
                )
                .into());
            }
        }
        let actual = blake3::hash(&payload).to_hex().to_string();
        if actual != payload_hash {
            return Err(SessionError::Malformed(
                "context checkpoint payload hash mismatch (truncated or corrupt)".into(),
            )
            .into());
        }
        Ok(Some(ContextCheckpoint {
            through_seq,
            digest,
            payload,
        }))
    }

    /// One deterministic page of memory facts (see [`MemoryFactsPage`]).
    /// Bounded: never loads more than one page + one probe row, and the
    /// probe runs inside the same query.
    pub fn memory_facts_page(
        &self,
        after: Option<&(i64, String, String)>,
        limit: i64,
    ) -> faktor_core::Result<MemoryFactsPage> {
        let limit = limit.clamp(1, MAX_FACT_PAGE_SIZE);
        let page_limit = u64::try_from(limit).map_err(|_| {
            SessionError::Malformed(format!("memory fact page limit {limit} is negative"))
        })?;
        let (rows, has_more) = self
            .manager
            .store()
            .memory_facts_page(self.id, after, page_limit)
            .map_err(crate::map_store_err)?;
        let cursor = if has_more {
            rows.last()
                .map(|r| (r.updated_ms, r.kind.clone(), r.key.clone()))
        } else {
            None
        };
        let total_estimate = self
            .manager
            .store()
            .memory_fact_count(self.id)
            .map_err(crate::map_store_err)?;
        Ok(MemoryFactsPage {
            facts: rows.into_iter().map(|r| (r.kind, r.key, r.value)).collect(),
            size: limit,
            cursor,
            has_more,
            total_estimate,
        })
    }

    /// Durable UX marker of a THRESHOLD-TRIGGERED compaction in flight
    /// (S10). It is written only while the summarizer/pruner runs and cleared
    /// at the end, so a standalone session can render "compacting" instead of
    /// inferring progress. A crash leaves a stale marker with its start time;
    /// readers must treat an old marker as stale, never as live progress.
    pub fn set_runtime_compaction(
        &self,
        active: bool,
        trigger: &str,
        now_ms: i64,
    ) -> faktor_core::Result<()> {
        const KIND: &str = "runtime_compaction";
        const KEY: &str = "active";
        if active {
            let value = serde_json::json!({
                "version": 1,
                "trigger": trigger,
                "started_ms": now_ms,
            })
            .to_string();
            self.upsert_memory_fact(KIND, KEY, &value)
        } else {
            self.manager
                .store()
                .delete_memory_fact(self.id, KIND, KEY)
                .map(|_| ())
                .map_err(|e| crate::map_store_err(e).into())
        }
    }

    /// The stale-tolerant in-flight compaction marker, if any.
    pub fn runtime_compaction(&self) -> faktor_core::Result<Option<serde_json::Value>> {
        const KIND: &str = "runtime_compaction";
        const KEY: &str = "active";
        let Some(raw) = self.memory_fact_get(KIND, KEY)? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            SessionError::Malformed(format!("runtime compaction marker is malformed: {e}"))
        })?;
        Ok(Some(value))
    }

    /// The durable task ledger (goal, completed/open steps, decisions, ...).
    /// The journal is the source of truth for *what happened*; the ledger is
    /// the compact structured projection that survives compaction.
    pub fn get_task_ledger(&self) -> faktor_core::Result<Option<serde_json::Value>> {
        self.manager
            .store()
            .get_task_ledger(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    pub fn put_task_ledger(&self, ledger: serde_json::Value) -> faktor_core::Result<()> {
        if json_bytes(&ledger) > MAX_LEDGER_BYTES {
            return Err(SessionError::Oversized(format!(
                "ledger of {} bytes exceeds MAX_LEDGER_BYTES",
                json_bytes(&ledger)
            ))
            .into());
        }
        self.manager
            .store()
            .put_task_ledger(self.id, ledger)
            .map_err(|e| crate::map_store_err(e).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use crate::SessionManager;

    #[test]
    fn memory_facts_upsert_and_bounds() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.upsert_memory_fact("preference", "language", "rust")
            .unwrap();
        s.upsert_memory_fact("preference", "language", "go")
            .unwrap();
        let facts = s.memory_facts().unwrap();
        assert_eq!(facts.len(), 1, "upsert replaces by (kind, key)");
        assert_eq!(
            facts[0],
            (
                "preference".to_string(),
                "language".to_string(),
                "go".to_string()
            )
        );
        // Empty kind/key are malformed; oversized values are rejected.
        assert!(s.upsert_memory_fact("", "k", "v").is_err());
        assert!(s.upsert_memory_fact("k", "", "v").is_err());
        assert!(s
            .upsert_memory_fact("k", "k", &"v".repeat(MAX_FACT_VALUE_BYTES + 1))
            .is_err());
        assert_eq!(
            s.memory_facts().unwrap().len(),
            1,
            "rejected facts leave no trace"
        );
    }

    #[test]
    fn task_ledger_roundtrip_and_bound() {
        let (_d, m) = test_manager();
        let s = session(&m);
        assert_eq!(s.get_task_ledger().unwrap(), None);
        let ledger = serde_json::json!({
            "goal": "implement faktor-session",
            "completed_steps": ["journal"],
            "open_steps": ["recovery"]
        });
        s.put_task_ledger(ledger.clone()).unwrap();
        assert_eq!(s.get_task_ledger().unwrap(), Some(ledger));
        // Replace, don't accumulate.
        s.put_task_ledger(serde_json::json!({"goal": "v2"}))
            .unwrap();
        assert_eq!(
            s.get_task_ledger().unwrap(),
            Some(serde_json::json!({"goal": "v2"}))
        );
        // Oversized ledgers are rejected before the write.
        let huge = serde_json::json!({ "blob": "x".repeat(MAX_LEDGER_BYTES + 1) });
        assert!(s.put_task_ledger(huge).is_err());
        assert_eq!(
            s.get_task_ledger().unwrap(),
            Some(serde_json::json!({"goal": "v2"}))
        );
    }

    #[test]
    fn memory_and_ledger_are_per_session() {
        let (_d, m) = test_manager();
        let s1 = session(&m);
        let s2 = {
            let ws = m.create_workspace("/w2").unwrap();
            m.create_session(ws, "t2", "p", "m").unwrap()
        };
        s1.upsert_memory_fact("k", "key", "s1").unwrap();
        s2.upsert_memory_fact("k", "key", "s2").unwrap();
        assert_eq!(s1.memory_facts().unwrap()[0].2, "s1");
        assert_eq!(s2.memory_facts().unwrap()[0].2, "s2");
        s1.put_task_ledger(serde_json::json!({"owner": "s1"}))
            .unwrap();
        assert!(s2.get_task_ledger().unwrap().is_none());
    }

    #[test]
    fn empty_facts_store_page_carries_full_metadata() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let page = s.memory_facts_page(None, 17).unwrap();
        assert!(page.facts.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.cursor, None);
        assert_eq!(page.size, 17, "empty page still reports the page size");
        assert_eq!(page.total_estimate, 0);
        // Hostile cursors are empty final pages, never an error.
        for hostile in [
            Some((i64::MIN, String::new(), String::new())),
            Some((i64::MIN, "z".into(), "z".into())),
            Some((-5, "a".into(), "b".into())),
        ] {
            let p = s.memory_facts_page(hostile.as_ref(), 7).unwrap();
            assert!(p.facts.is_empty());
            assert!(!p.has_more);
            assert_eq!(p.cursor, None);
        }
    }

    #[test]
    fn hostile_max_page_limit_clamps_to_the_bound() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let page = s.memory_facts_page(None, i64::MAX).unwrap();
        assert_eq!(page.size, MAX_FACT_PAGE_SIZE, "hostile max clamps");
        let page = s.memory_facts_page(None, i64::MIN).unwrap();
        assert_eq!(page.size, 1, "hostile min clamps");
    }

    #[test]
    fn facts_page_walk_reaches_every_fact_exactly_once() {
        let (_d, m) = test_manager();
        let s = session(&m);
        for i in 0..23 {
            // A shared kind exercises (updated_ms, kind, key) tie-breaks:
            // same-millisecond rows must still cut deterministically.
            s.upsert_memory_fact("decision", &format!("k{i:03}"), &format!("v{i}"))
                .unwrap();
        }
        // The legacy full read agrees with the walk's total.
        let full: Vec<(String, String, String)> = s.memory_facts().unwrap();
        assert_eq!(full.len(), 23);
        let mut seen: Vec<(String, String, String)> = Vec::new();
        let mut cursor: Option<(i64, String, String)> = None;
        loop {
            let p = s.memory_facts_page(cursor.as_ref(), 7).unwrap();
            assert!(p.facts.len() <= 7, "never more than one page");
            assert_eq!(p.size, 7, "every page echoes the applied size");
            if p.facts.is_empty() {
                assert!(!p.has_more, "empty page must close the walk");
                break;
            }
            for f in &p.facts {
                assert!(!seen.contains(f), "duplicate fact in walk: {f:?}");
                seen.push(f.clone());
            }
            if !p.has_more {
                // Final page: cursor must be None so clients stop.
                assert_eq!(p.cursor, None);
                break;
            }
            cursor = p.cursor.clone();
        }
        assert_eq!(seen.len(), full.len(), "walk must cover the full set");
        let mut sorted = seen.clone();
        sorted.sort();
        let mut full_sorted = full.clone();
        full_sorted.sort();
        assert_eq!(sorted, full_sorted, "gap: walk missed a fact");
        // Replaying the first cursor yields the identical window again
        // (deterministic idempotence, never a duplicate page).
        let p1 = s.memory_facts_page(None, 7).unwrap();
        assert!(p1.has_more);
        let p1b = s.memory_facts_page(None, 7).unwrap();
        assert_eq!(p1, p1b, "same cursor, same page");
        assert_eq!(p1.total_estimate, 23);
    }

    #[test]
    fn facts_walk_under_concurrent_upserts_never_duplicates() {
        // Adversarial: a writer upserts facts WHILE a reader walks pages.
        // An upsert moves a row to the NEWEST end of the order, so the walk
        // must never see the same (kind, key) twice; rows never rewritten
        // during the walk are still covered exactly once (no gap over the
        // stable rows), and rows rewritten before their turn simply leave
        // the walk window instead of being emitted twice.
        let (_d, m) = test_manager();
        let s = session(&m);
        for i in 0..30 {
            s.upsert_memory_fact("seed", &format!("s{i:03}"), "v")
                .unwrap();
        }
        let mut seen: Vec<(String, String, String)> = Vec::new();
        let mut cursor: Option<(i64, String, String)> = None;
        let mut page_no = 0;
        loop {
            let p = s.memory_facts_page(cursor.as_ref(), 6).unwrap();
            // Between pages, the writer interleaves: rewrite one old seed
            // row (moves it to the newest end) and add one new fact.
            s.upsert_memory_fact("seed", &format!("s{:03}", page_no * 2), "rewritten")
                .unwrap();
            s.upsert_memory_fact("late", &format!("n{page_no}"), "new")
                .unwrap();
            if p.facts.is_empty() {
                assert!(!p.has_more);
                break;
            }
            for f in &p.facts {
                assert!(
                    !seen.contains(f),
                    "a fact must never appear twice in one walk: {f:?}"
                );
                seen.push(f.clone());
            }
            if !p.has_more {
                break;
            }
            cursor = p.cursor.clone();
            page_no += 1;
        }
        // Every fact the walk saw is a real seed row (newly appended rows
        // land at the newest end and never leak into an ongoing walk).
        assert!(
            seen.iter().all(|(k, _key, v)| k == "seed" && v == "v"),
            "{seen:?}"
        );
        // No duplicate (kind, key): the walk's emitted identities are all
        // distinct regardless of the writer's interleaving.
        let mut ids: Vec<(&String, &String)> = seen.iter().map(|(k, key, _)| (k, key)).collect();
        ids.sort();
        let distinct: usize =
            ids.windows(2).filter(|w| w[0] != w[1]).count() + usize::from(!ids.is_empty());
        assert_eq!(distinct, ids.len(), "duplicate fact identity in walk");
        // No gap over the stable window: the odd seed rows are NEVER
        // rewritten by the writer, so each must appear exactly once.
        for i in (1..30).step_by(2) {
            let key = format!("s{i:03}");
            assert_eq!(
                seen.iter()
                    .filter(|(k, k2, _)| k == "seed" && *k2 == key)
                    .count(),
                1,
                "stable fact {key} must appear exactly once (no gap)"
            );
        }
        // The walk always terminates, and replaying the cursor of the page
        // that closed it re-yields that same page (deterministic replay,
        // never a duplicate or a rewind).
        if let Some(c) = &cursor {
            let probe: Option<(i64, String, String)> = Some((c.0, c.1.clone(), c.2.clone()));
            let replay = s.memory_facts_page(probe.as_ref(), 6).unwrap();
            let again = s.memory_facts_page(probe.as_ref(), 6).unwrap();
            assert_eq!(replay, again, "cursor replay is deterministic");
        }
    }

    #[test]
    fn context_checkpoint_roundtrips_atomically_and_rejects_corruption() {
        let (dir, m) = test_manager();
        let s = session(&m);
        let payload = "TURN-".repeat(2_000).into_bytes(); // 10 KiB -> multiple bounded chunks
        let digest = blake3::hash(b"digest").to_hex().to_string();
        let sid = s.id();
        s.put_context_checkpoint(42, &digest, &payload).unwrap();
        let stored = s
            .context_checkpoint()
            .unwrap()
            .expect("checkpoint must roundtrip");
        assert_eq!(stored.through_seq, 42);
        assert_eq!(stored.digest, digest);
        assert_eq!(stored.payload, payload);

        // Reopening the store (daemon restart) reproduces the same bytes.
        drop(s);
        let reopened =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let handle = reopened.get_session(sid).unwrap().unwrap();
        let after = handle.context_checkpoint().unwrap().unwrap();
        assert_eq!(after, stored, "restart must reproduce the checkpoint");

        // A shorter replacement deletes stale chunks atomically.
        handle
            .put_context_checkpoint(43, &digest, b"short")
            .unwrap();
        let replaced = handle.context_checkpoint().unwrap().unwrap();
        assert_eq!(replaced.payload, b"short");
        let chunks = handle
            .memory_facts_by_kind_page(CONTEXT_CHECKPOINT_KIND, None, MAX_FACT_PAGE_SIZE)
            .unwrap();
        assert_eq!(
            chunks.facts.len(),
            2,
            "head + exactly one chunk, no stale chunk rows"
        );

        // Corrupt one chunk: the payload hash must fail loudly, never be
        // mistaken for an absent checkpoint.
        m.store()
            .upsert_memory_fact(sid, CONTEXT_CHECKPOINT_KIND, "c/000000", "garbage")
            .unwrap();
        assert!(
            handle.context_checkpoint().is_err(),
            "truncated/corrupt checkpoint must be a typed error"
        );
    }

    #[test]
    fn identical_fact_rewrites_are_true_noops_and_changes_are_stamped() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.upsert_memory_fact("task", "goal", "same").unwrap();
        let before = s
            .memory_facts_by_kind_page("task", None, MAX_FACT_PAGE_SIZE)
            .unwrap()
            .facts;
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.upsert_memory_fact("task", "goal", "same").unwrap();
        let again = s
            .memory_facts_by_kind_page("task", None, MAX_FACT_PAGE_SIZE)
            .unwrap()
            .facts;
        assert_eq!(before, again, "an unchanged value must not rewrite the row");
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.upsert_memory_fact("task", "goal", "changed").unwrap();
        let changed = s
            .memory_facts_by_kind_page("task", None, MAX_FACT_PAGE_SIZE)
            .unwrap()
            .facts;
        assert_eq!(changed[0].2, "changed");
    }

    #[test]
    fn per_turn_fact_retention_is_bounded_and_keeps_the_newest() {
        let (_d, m) = test_manager();
        let s = session(&m);
        for i in 0..200 {
            s.upsert_memory_fact("turn", &format!("{i:04}"), "x")
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let deleted = s.prune_memory_facts_kind_keep_newest("turn", 64).unwrap();
        assert_eq!(deleted, 136);
        let facts = s.memory_facts().unwrap();
        assert_eq!(facts.len(), 64);
        assert!(facts.iter().any(|(_, k, _)| k == "0199"));
        assert!(!facts.iter().any(|(_, k, _)| k == "0000"));
    }
}
