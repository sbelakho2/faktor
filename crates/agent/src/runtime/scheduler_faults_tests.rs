//! `runtime::scheduler_faults_tests`: fault-injection test support.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Point {
    /// The scheduler never ran: every submitted op is still open.
    BeforeRun,
    /// The scheduler ran to completion first; its terminal statuses are
    /// visible when the injected failure is resolved.
    AfterRun,
}

fn armed() -> &'static Mutex<HashMap<(PathBuf, Point), faktor_core::Error>> {
    static ARMED: OnceLock<Mutex<HashMap<(PathBuf, Point), faktor_core::Error>>> = OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Arm one injected scheduler failure of `kind` at `point` for the store
/// at `root`. Takes precedence over any previously armed fault for the
/// same slot.
pub fn arm(root: &Path, point: Point, kind: faktor_core::ErrorKind, message: &str) {
    armed().lock().unwrap().insert(
        (root.to_path_buf(), point),
        faktor_core::Error::new(kind, message),
    );
}

pub fn take(root: &Path, point: Point) -> Option<faktor_core::Error> {
    armed().lock().unwrap().remove(&(root.to_path_buf(), point))
}
