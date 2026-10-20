//! FORCE-exit browser reap: the daemon calls this before `process::exit`,
//! which runs no destructors, so the browser tree must die by its recorded
//! owner. Kept out of `lib.rs` to stay under the source-size ceiling.

use crate::{ProcessOwner, ProcessSupervisor};

impl ProcessSupervisor {
    /// Reap every live browser-owned child through the supervisor registry.
    /// Used by the daemon's FORCE-exit path (`on_force_signal`), which calls
    /// `process::exit` and therefore cannot rely on `Drop` running: the
    /// browser tree must still die by its recorded owner. Returns the child
    /// ids whose kill was attempted.
    pub fn kill_all_browsers(&self) -> Vec<u64> {
        let owners: Vec<ProcessOwner> = {
            let reg = faktor_core::lock::recover(&self.registry);
            let mut owners = Vec::new();
            for state in reg.values() {
                if state.exited.is_some()
                    || !matches!(state.owner, ProcessOwner::Browser { .. })
                    || owners.contains(&state.owner)
                {
                    continue;
                }
                owners.push(state.owner.clone());
            }
            owners
        };
        let mut killed = Vec::new();
        for owner in owners {
            killed.extend(self.kill_all_for(owner));
        }
        killed
    }
}
