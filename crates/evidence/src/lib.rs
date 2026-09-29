//! Faktor-native evidence/CCR layer (audit 34/36/37/38/39).
//!
//! Raw tool/process/search output is normalized into typed compact
//! representations with full backing in CAS, provenance that can never
//! gain instruction authority, compression policies hardcoded by kind,
//! and retrieval by typed selector — never whole-blob auto-dumps.

pub mod compress;
pub mod normalize;
pub mod provenance;
pub mod render;
pub mod retrieve;
pub mod store;
pub mod types;

#[cfg(test)]
mod durable_domain_tests {
    use crate::types::EvidenceId;
    use faktor_core::{SessionId, WorkspaceId};
    use faktor_store::{EvidenceRow, StoreError};

    fn tmp_store() -> (tempfile::TempDir, faktor_store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = faktor_store::Store::open(dir.path(), true).unwrap();
        (dir, store)
    }

    fn row(session_id: SessionId, workspace_id: WorkspaceId, compact: &str) -> EvidenceRow {
        EvidenceRow {
            id: 0,
            session_id,
            workspace_id,
            task_id: Some(7),
            kind: "process_log".into(),
            revision: 1,
            provenance_json: r#"{"entries":["tool"]}"#.into(),
            compressibility: "aggressive".into(),
            compression_json: r#"{"algorithm":"identity"}"#.into(),
            retrieval_json: r#"{"allow_ranges":true,"allow_search":true,"max_bytes":64}"#.into(),
            compact_json: compact.into(),
            backing_cas_hash: Some("ab".repeat(32)),
            completeness: "complete".into(),
            created_ms: 1234,
        }
    }

    /// The durable evidence-id domain is `1..=i64::MAX`: both edges
    /// round-trip through the store, and an id above the edge is refused
    /// typed at every boundary BEFORE SQL, so it can never be persisted in
    /// its two's-complement negative form and read back as corruption.
    #[test]
    fn evidence_ids_honor_the_durable_signed_domain() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        let max_durable = i64::MAX as u64;
        assert_eq!(
            EvidenceId(max_durable).0,
            max_durable,
            "the in-memory id names the durable edge exactly"
        );

        // The smallest durable id round-trips.
        let mut first = row(sid, ws, "id-1");
        first.id = 1;
        assert_eq!(store.evidence_insert(&first).unwrap(), 1);
        assert_eq!(store.evidence_get(1).unwrap().unwrap().compact_json, "id-1");
        assert_eq!(store.evidence_high_water().unwrap(), 1);

        // The largest durable id round-trips.
        let mut largest = row(sid, ws, "id-max");
        largest.id = max_durable;
        assert_eq!(store.evidence_insert(&largest).unwrap(), max_durable);
        assert_eq!(
            store
                .evidence_get(max_durable)
                .unwrap()
                .unwrap()
                .compact_json,
            "id-max"
        );
        assert_eq!(store.evidence_high_water().unwrap(), max_durable);

        // The exclusive newest-first cursor may sit ON the domain edge.
        let edge = store
            .evidence_list_by_scope_newest(sid, ws, Some(max_durable), 10)
            .unwrap();
        assert_eq!(edge.len(), 1);
        assert_eq!(edge[0].id, 1);

        // Anything above the durable domain refuses BEFORE SQL: no row is
        // stored (the scoped listing still holds exactly the two rows), the
        // by-id read refuses, and the cursor refuses.
        for hostile in [max_durable + 1, u64::MAX] {
            let mut bad = row(sid, ws, "out-of-domain");
            bad.id = hostile;
            assert!(
                matches!(store.evidence_insert(&bad), Err(StoreError::Malformed(_))),
                "evidence id {hostile} must refuse typed"
            );
            assert!(
                matches!(store.evidence_get(hostile), Err(StoreError::Malformed(_))),
                "evidence_get({hostile}) must refuse typed"
            );
            assert!(
                matches!(
                    store.evidence_list_by_scope_newest(sid, ws, Some(hostile), 10),
                    Err(StoreError::Malformed(_))
                ),
                "evidence cursor {hostile} must refuse typed"
            );
        }
        assert_eq!(
            store.evidence_list_by_scope(sid, ws, 100).unwrap().len(),
            2,
            "no out-of-domain row may be durably stored"
        );
        assert_eq!(store.evidence_high_water().unwrap(), max_durable);
    }
}
