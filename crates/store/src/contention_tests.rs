// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Transient-contention classification for the wire's `retryable` flag.
//!
//! Adversarial by construction: every permanent failure shape must stay
//! non-retryable (a retry loop on corruption would hammer a dead store), the
//! message matcher must not fire on lookalike text from other error classes,
//! and the typed path must agree with the message path on the same errors.

use rusqlite::{ffi, Error as SqlError};

use super::StoreError;

fn sqlite_failure(code: i32, message: Option<&str>) -> StoreError {
    StoreError::Sqlite(SqlError::SqliteFailure(
        ffi::Error::new(code),
        message.map(str::to_string),
    ))
}

#[test]
fn typed_contention_shapes_are_retryable_and_every_permanent_failure_is_not() {
    // SQLITE_BUSY (5) and SQLITE_LOCKED (6), with and without a driver text.
    assert!(sqlite_failure(5, Some("database is locked")).is_transient_contention());
    assert!(sqlite_failure(5, None).is_transient_contention());
    assert!(sqlite_failure(6, Some("database table is locked")).is_transient_contention());
    assert!(
        StoreError::Busy("reader pool busy: all readers leased".into()).is_transient_contention()
    );
    assert!(StoreError::WriterQueueFull {
        operation: "settle",
        pending: 64,
        capacity: 64,
    }
    .is_transient_contention());

    // Hostile negatives: permanent classes must never read as retryable.
    assert!(!sqlite_failure(19, None).is_transient_contention());
    assert!(!StoreError::Corrupt(vec!["integrity check failed".into()]).is_transient_contention());
    assert!(!StoreError::Malformed("value rejected".into()).is_transient_contention());
    assert!(!StoreError::StorageRefused("quota exhausted".into()).is_transient_contention());
    assert!(!StoreError::SeqViolation(9).is_transient_contention());
    assert!(!StoreError::UnsupportedSchema {
        found: 42,
        maximum_supported: 7,
    }
    .is_transient_contention());
}

#[test]
fn message_contention_matcher_catches_store_display_text_and_rejects_lookalikes() {
    assert!(StoreError::is_transient_contention_message(
        "sqlite error: database is locked"
    ));
    assert!(StoreError::is_transient_contention_message(
        "sqlite error: database table is locked"
    ));
    assert!(StoreError::is_transient_contention_message(
        "reader pool busy: deadline exceeded"
    ));
    assert!(StoreError::is_transient_contention_message(
        "writer queue full: 64 pending (capacity 64); `settle` refused"
    ));
    assert!(StoreError::is_transient_contention_message(
        "session provider unavailable: sqlite error: DATABASE IS LOCKED"
    ));

    // Lookalikes and unrelated messages must not become retryable: the
    // flag turning true on a permanent error would make clients loop.
    assert!(!StoreError::is_transient_contention_message(
        "unlocked because no lease"
    ));
    assert!(!StoreError::is_transient_contention_message(
        "disk I/O error"
    ));
    assert!(!StoreError::is_transient_contention_message(
        "store is corrupted: integrity check failed"
    ));
    assert!(!StoreError::is_transient_contention_message(""));
    assert!(!StoreError::is_transient_contention_message(
        "malformed: user text 'database is locked?'"
    ));
}

#[test]
fn typed_and_message_classifications_agree_on_the_same_store_errors() {
    let cases = [
        sqlite_failure(5, Some("database is locked")),
        sqlite_failure(5, None),
        sqlite_failure(6, None),
        sqlite_failure(19, None),
        StoreError::Busy("reader pool busy: deadline exceeded".into()),
        StoreError::WriterQueueFull {
            operation: "save",
            pending: 1,
            capacity: 1,
        },
        StoreError::Corrupt(vec!["mismatch".into()]),
        StoreError::Malformed("bad key".into()),
    ];
    for error in cases {
        assert_eq!(
            error.is_transient_contention(),
            StoreError::is_transient_contention_message(&error.to_string()),
            "typed/message disagreement for {error}"
        );
    }
}
