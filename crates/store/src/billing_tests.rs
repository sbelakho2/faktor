use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), true).unwrap();
    (dir, s)
}

#[test]
fn op_id_seq_seeds_high_and_reserves_contiguous_global_ranges() {
    // Fresh stores seed the ONE global row from the migration-time clock
    // (see `op_id_seq_seed`): the seed must sit far above every
    // pre-migration `clock + counter` id and stay aligned to the 1024-id
    // reservation quantum.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "a", "p", "m").unwrap();
    let s2 = store.create_session(ws, "b", "p", "m").unwrap();
    let hw0 = store.op_id_seq_high_water().unwrap();
    assert!(
        hw0 > (1u64 << 20),
        "seed must dominate clock+counter ids: {hw0}"
    );
    assert_eq!(hw0 % 1024, 0, "seed must sit on a quantum boundary");
    // The sequence is GLOBAL: alternating sessions get contiguous ranges.
    let (a0, n0) = store.alloc_op_ids(s1.id, 100).unwrap();
    let (b0, n1) = store.alloc_op_ids(s2.id, 250).unwrap();
    let (a1, n2) = store.alloc_op_ids(s1.id, 7).unwrap();
    assert_eq!((a0, n0), (hw0, 100), "first range starts at the seed");
    assert_eq!((b0, n1), (hw0 + 100, 250), "second range is contiguous");
    assert_eq!((a1, n2), (hw0 + 350, 7), "ranges never interleave");
    assert_eq!(
        store.op_id_seq_high_water().unwrap(),
        hw0 + 357,
        "high water is one past the last reserved id"
    );
    assert_ne!(a0, 0, "zero is contractually impossible");
}
