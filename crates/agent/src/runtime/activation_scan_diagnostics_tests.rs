//! `runtime::activation_scan_diagnostics_tests`: fault-injection test support.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The loud diagnostic one bounded scan emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostic {
    /// The `pages x page_size` walk ended AT the bound while older pages
    /// still existed and no activation fact was seen: the set could not
    /// be proven, so the scan fell back to the safe inactive default.
    BoundExhausted { pages: usize, page_size: i64 },
}

fn events() -> &'static Mutex<HashMap<PathBuf, Vec<Diagnostic>>> {
    static EVENTS: OnceLock<Mutex<HashMap<PathBuf, Vec<Diagnostic>>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn record(root: &Path, diagnostic: Diagnostic) {
    events()
        .lock()
        .unwrap()
        .entry(root.to_path_buf())
        .or_default()
        .push(diagnostic);
}

pub fn events_for(root: &Path) -> Vec<Diagnostic> {
    events()
        .lock()
        .unwrap()
        .get(root)
        .cloned()
        .unwrap_or_default()
}

pub fn clear(root: &Path) {
    events().lock().unwrap().remove(root);
}
