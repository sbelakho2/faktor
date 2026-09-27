//! `runtime::durable_faults_tests`: fault-injection test support.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Armed `(store root, site)` pairs with the remaining number of fires:
/// store-scoped so a concurrent test's write at the same site (session
/// ids are per-store sequences and can collide across temp stores) can
/// never steal another test's fault.
fn armed() -> &'static Mutex<HashMap<(PathBuf, String), u32>> {
    static ARMED: OnceLock<Mutex<HashMap<(PathBuf, String), u32>>> = OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Arm the next guarded write of the store at `root`, at `site`, to fail
/// once.
pub fn arm(root: &Path, site: &str) {
    arm_repeat(root, site, 1);
}

/// Arm the next `times` guarded operations at `site` to fail (bounded
/// retry/backoff paths).
pub fn arm_repeat(root: &Path, site: &str, times: u32) {
    armed()
        .lock()
        .unwrap()
        .insert((root.to_path_buf(), site.to_string()), times.max(1));
}

pub fn take(root: &Path, site: &str) -> bool {
    let mut map = armed().lock().unwrap();
    let key = (root.to_path_buf(), site.to_string());
    match map.get_mut(&key) {
        None => false,
        Some(remaining) => {
            let fire = *remaining > 0;
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                map.remove(&key);
            }
            fire
        }
    }
}

/// Disarm the fault at `site` (test cleanup after a bounded-retry
/// assertion: the remaining fires must not leak into a later stage).
pub fn disarm(root: &Path, site: &str) {
    armed()
        .lock()
        .unwrap()
        .remove(&(root.to_path_buf(), site.to_string()));
}
