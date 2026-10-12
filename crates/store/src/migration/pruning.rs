//! Restore-point retention: the bounded sweep over one database's snapshots,
//! the numeric ordering of restore-point names and the identity protection of
//! the just-written point.

use super::*;

/// Bound on retained pre-migration restore points per database (the same
/// discipline as the commercial stores' `MAX_MIGRATION_BACKUPS`): after every
/// successful new restore point the OLDEST are pruned so an arbitrarily long
/// upgrade history cannot grow the backups directory without bound. Pruning
/// is best effort — it never fails the migration, and it never removes the
/// newest point (the one most recent transitions need), not even when mtimes
/// tie: the just-written point is protected by identity and the name tail is
/// ordered numerically (see [`prune_old_pre_migration_backups`]).
pub const MAX_PRE_MIGRATION_RESTORE_POINTS: usize = 4;

/// Ordering components of one restore-point name, parsed NUMERICALLY.
///
/// Recognized forms (the numeric parse is what makes `10` sort after `9`; a
/// lexicographic tie-break used to put `...-10.db` before `...-9.db` and could
/// prune the just-written newest point on equal mtimes):
///
/// * current: `{db_name}.pre-migration-v{version}-{stamp}-{pid}-{seq}.db`
/// * legacy:  `{db_name}.pre-migration-v{version}-{stamp}.db`
///
/// A name that carries the marker but matches neither form is
/// [`RestorePointTieBreak::Unrecognized`]: it still counts as a candidate, but
/// it sorts after EVERY recognized name, so a sweep never deletes a point it
/// cannot order ahead of one it can.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RestorePointTieBreak {
    /// Comparison order: `stamp`, then form (`tail`), then `pid`, then `seq`,
    /// then `version`. `tail` is `true` for the current `-{pid}-{seq}` form;
    /// a legacy point with an equal stamp is older by construction (it cannot
    /// carry the monotonic sequence), so the just-written current-form point
    /// is never ordered behind it.
    Recognized {
        stamp: u64,
        tail: bool,
        pid: u32,
        seq: u64,
        version: i64,
    },
    Unrecognized,
}

/// Parse the numeric tail of one restore-point name (see
/// [`RestorePointTieBreak`]). Version is parsed as `i64` (the field is a
/// schema cursor); stamp/pid/seq as unsigned. Any non-numeric or unexpected
/// shape falls back to `Unrecognized` — never to string comparison of numbers.
fn restore_point_tie_break(name: &str) -> RestorePointTieBreak {
    let Some((_, tail)) = name
        .strip_suffix(".db")
        .and_then(|base| base.rsplit_once(".pre-migration-v"))
    else {
        return RestorePointTieBreak::Unrecognized;
    };
    let parts: Vec<&str> = tail.split('-').collect();
    match parts.as_slice() {
        [version, stamp] => {
            let (Ok(version), Ok(stamp)) = (version.parse::<i64>(), stamp.parse::<u64>()) else {
                return RestorePointTieBreak::Unrecognized;
            };
            RestorePointTieBreak::Recognized {
                stamp,
                tail: false,
                pid: 0,
                seq: 0,
                version,
            }
        }
        [version, stamp, pid, seq] => {
            let (Ok(version), Ok(stamp), Ok(pid), Ok(seq)) = (
                version.parse::<i64>(),
                stamp.parse::<u64>(),
                pid.parse::<u32>(),
                seq.parse::<u64>(),
            ) else {
                return RestorePointTieBreak::Unrecognized;
            };
            RestorePointTieBreak::Recognized {
                stamp,
                tail: true,
                pid,
                seq,
                version,
            }
        }
        _ => RestorePointTieBreak::Unrecognized,
    }
}

/// Prune the oldest restore points of ONE database beyond
/// [`MAX_PRE_MIGRATION_RESTORE_POINTS`]. `keep` is the just-written point:
/// it is protected by IDENTITY on top of the ordering, so no tie (equal
/// mtimes, equal stamps, an unparseable foreign name) can ever prune the
/// newest restore point. Best effort: a failed removal stops the sweep (never
/// deletes newer points around a stubborn file) and never propagates; the
/// newly written point is already verified.
pub(crate) fn prune_old_pre_migration_backups(backup_dir: &Path, db_name: &str, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(backup_dir) else {
        return;
    };
    let marker_prefix = format!("{db_name}.pre-migration-v");
    let mut points: Vec<(std::time::SystemTime, RestorePointTieBreak, String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Exact `{db_name}.pre-migration-v` prefix: a sibling database
            // whose name merely STARTS with this one (e.g. `db` vs `db2`)
            // must never have its restore points counted or pruned here.
            if !name.starts_with(&marker_prefix) || !name.ends_with(".db") {
                return None;
            }
            let path = entry.path();
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            let tie = restore_point_tie_break(&name);
            Some((modified, tie, name, path))
        })
        .collect();
    points.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut kept = points.len();
    for (_, _, _, victim) in points {
        if kept <= MAX_PRE_MIGRATION_RESTORE_POINTS {
            break;
        }
        if victim == keep {
            continue;
        }
        if std::fs::remove_file(&victim).is_err() {
            break;
        }
        kept -= 1;
    }
}

#[cfg(test)]
#[path = "pruning_tests.rs"]
mod pruning_tests;
