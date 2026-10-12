use super::*;
use std::sync::{Arc, Barrier};

/// Adversarial first-open race: two openers migrate the SAME fresh store
/// concurrently. Rounds used to end with a complete schema and a cursor
/// written BACKWARDS (the loser replayed an idempotent step and then died
/// on a non-idempotent one), permanently bricking every later open.
#[test]
fn concurrent_migration_races_never_brick_the_cursor() {
    for _round in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        drop(rusqlite::Connection::open(&path).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut conn = rusqlite::Connection::open(&path).unwrap();
                    barrier.wait();
                    migrate(&mut conn, Some(path.as_path()))
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("no panic"))
            .collect();
        assert!(
            results.iter().any(|r| r.is_ok()),
            "at least one opener must migrate: {results:?}"
        );
        // The directory must STILL open: a later opener applies only what
        // is pending and never trusts a stale cursor.
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrate(&mut conn, Some(path.as_path()))
            .expect("the raced store must still migrate cleanly");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }
}
