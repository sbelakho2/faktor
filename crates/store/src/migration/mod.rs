//! `migration`: the transactional schema ladder, the verified pre-migration
//! restore points and their bounded retention, and the one-shot v22 legacy
//! verification import. Mechanically decomposed from the former single-file
//! module; the crate-facing surface is preserved by the glob re-exports below.

use super::*;

mod legacy_import;
pub use legacy_import::*;
mod pruning;
pub use pruning::*;
mod restore_points;
pub use restore_points::*;

/// Apply migrations transactionally; `PRAGMA user_version` is the cursor.
///
/// The version read, the pre-migration restore point and every migration
/// statement run inside ONE `BEGIN IMMEDIATE` transaction, so a second
/// concurrent opener blocks on the write lock, re-reads the (already
/// advanced) version inside its own transaction and skips. Trusting a stale
/// pre-lock read replayed non-idempotent ALTERs and could commit the cursor
/// BACKWARDS, permanently bricking the data directory.
///
/// A NEGATIVE `user_version` (impossible for a database this ladder created:
/// SQLite stores the pragma as a SIGNED integer and every writer here only
/// moves it forward from 0) is durable corruption, refused typed BEFORE the
/// restore point and the ladder — treating it as v0 would silently migrate
/// corrupt state.
///
/// A version ABOVE [`SCHEMA_HEAD`] names a database migrated by a NEWER
/// binary. It is refused typed ([`StoreError::UnsupportedSchema`], naming the
/// found version and the known head) BEFORE any snapshot or write: an older
/// binary must never replay an older ladder over newer semantics, and the
/// recovery path is running the newer binary or restoring the pre-migration
/// snapshot.
pub(crate) fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> StoreResult<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let started: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if started < 0 {
        return Err(StoreError::Corrupt(vec![format!(
            "store schema user_version {started} is corrupt (negative): refusing to \
             snapshot or migrate it"
        )]));
    }
    if started > SCHEMA_HEAD {
        return Err(StoreError::UnsupportedSchema {
            found: started,
            maximum_supported: SCHEMA_HEAD,
        });
    }
    let mut version = started;
    if started < SCHEMA_HEAD {
        // A schema transition on a file-backed database runs only after a
        // VERIFIED restore point of the ACTUAL predecessor state exists; if
        // it cannot be written and self-verified, the migration is REFUSED.
        // The snapshot runs on a SEPARATE read-only connection: the backup
        // API cannot run on a connection that holds a write transaction, and
        // the BEGIN IMMEDIATE lock we hold makes every reader see exactly
        // this pre-migration state.
        if let Some(path) = db_path {
            let restore_point = verified_pre_migration_backup(path, version)?;
            tracing::info!(
                restore_point = %restore_point.display(),
                from_version = version,
                "store pre-migration restore point verified"
            );
        }
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let target = (i + 1) as i64;
            if version >= target {
                continue;
            }
            tx.execute_batch(sql)
                .map_err(|e| StoreError::Migration(format!("v{target}: {e}")))?;
            // The v9 op-id sequence table needs its one global row seeded from
            // the migration-time clock, which no static SQL can express. The
            // INSERT is idempotent so a replay (or a second opener racing the
            // first migration) can never double-seed or overwrite.
            if i == OP_ID_SEQ_MIGRATION_INDEX {
                tx.execute(
                    "INSERT OR IGNORE INTO op_id_seq (session_scope, next_value) VALUES (0, ?1)",
                    params![op_id_seq_seed()],
                )
                .map_err(|e| StoreError::Migration(format!("v{target} seed: {e}")))?;
            }
            tx.execute_batch(&format!("PRAGMA user_version = {target}"))
                .map_err(|e| StoreError::Migration(format!("v{target} version write: {e}")))?;
            version = target;
        }
    }
    tx.commit()
        .map_err(|e| StoreError::Migration(format!("commit: {e}")))?;
    // One-shot repair on EVERY open (marker-guarded, idempotent): project
    // pre-v22 verification `memory_fact` rows into the v22 tables. Runs after
    // the schema cursor reached the newest version, so the target tables
    // always exist; a store with no legacy rows pays one indexed SELECT.
    import_legacy_verification_facts_conn(conn)?;
    Ok(())
}

#[cfg(test)]
#[path = "concurrent_migration_tests.rs"]
mod concurrent_migration_tests;

#[cfg(test)]
#[path = "verification_scope_tests.rs"]
mod verification_scope_tests;

#[cfg(test)]
#[path = "schema_head_guard_tests.rs"]
mod schema_head_guard_tests;

#[cfg(test)]
#[path = "ladder_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "typed_ledger_tests.rs"]
mod typed_ledger_tests;
