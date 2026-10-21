//! Whole-workspace material generation and bounded dirty-path history.
//!
//! Consumers that cache a graph must never refresh from the paths a query
//! happens to name: an out-of-band edit to another file would leave the
//! cached relationships from an older generation, and a transitive impact
//! answer would be computed on a partial view. This authority is the shared
//! generation token: the daemon watch (and its metadata reconciliation
//! fallback) records every dirty path here, and a consumer either catches up
//! from the generation it cached or rebuilds when the bounded history no
//! longer covers that generation.
//!
//! A generation advances only when at least one non-empty path is recorded,
//! so an unchanged refresh keeps both the generation and the graph digest.
//! History is bounded in generations and paths; overflow and explicit loss
//! move `history_start` forward, and [`MaterialGeneration::dirty_since`]
//! reports `lost = true` for any cached generation it can no longer serve
//! exactly. A lost history is never silently treated as "no changes".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Bounded history: enough generations to cover ordinary edit/query
/// interleavings without retaining an unbounded transcript.
pub const DEFAULT_MAX_GENERATIONS: usize = 4096;
/// Bounded dirty paths across the retained history.
pub const DEFAULT_MAX_DIRTY_PATHS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationLimits {
    pub max_generations: usize,
    pub max_dirty_paths: usize,
}

impl Default for GenerationLimits {
    fn default() -> Self {
        Self {
            max_generations: DEFAULT_MAX_GENERATIONS,
            max_dirty_paths: DEFAULT_MAX_DIRTY_PATHS,
        }
    }
}

/// The exact dirty paths a consumer must apply to reach `generation`.
/// `lost` means the authority cannot prove the path set is complete (history
/// overflow, watcher lag or a restarted authority), so the consumer MUST
/// rebuild rather than apply a partial refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyPaths {
    pub generation: u64,
    pub paths: Vec<String>,
    pub lost: bool,
}

impl DirtyPaths {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

#[derive(Debug)]
struct State {
    current: u64,
    /// First generation whose dirty path set is fully retained. Generation
    /// `history_start - 1` is the oldest cached generation still serviceable.
    history_start: u64,
    history: BTreeMap<u64, BTreeSet<String>>,
    /// Total paths retained across `history` (the cross-generation bound).
    retained_paths: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            current: 0,
            history_start: 1,
            history: BTreeMap::new(),
            retained_paths: 0,
        }
    }
}

/// One workspace's material generation authority. Clone-free by design: the
/// daemon owns one instance per workspace and every consumer reads the SAME
/// token (no parallel counters).
#[derive(Debug)]
pub struct MaterialGeneration {
    limits: GenerationLimits,
    state: Mutex<State>,
}

impl Default for MaterialGeneration {
    fn default() -> Self {
        Self::new()
    }
}

impl MaterialGeneration {
    pub fn new() -> Self {
        Self::with_limits(GenerationLimits::default())
    }

    pub fn with_limits(limits: GenerationLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(State::default()),
        }
    }

    pub fn limits(&self) -> GenerationLimits {
        self.limits
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current whole-workspace generation. Cheap: a single lock + read,
    /// never a filesystem scan.
    pub fn current(&self) -> u64 {
        self.lock().current
    }

    /// Record one observation of dirty paths and advance the generation.
    /// Empty observations are no-ops (an unchanged refresh must not move the
    /// token). The returned value is the new current generation.
    pub fn observe<I, S>(&self, paths: I) -> u64
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let paths: BTreeSet<String> = paths
            .into_iter()
            .map(Into::into)
            .filter(|path| !path.is_empty())
            .collect();
        if paths.is_empty() {
            return self.current();
        }
        let mut state = self.lock();
        state.current = state.current.saturating_add(1);
        let generation = state.current;
        if self.limits.max_generations == 0 || paths.len() > self.limits.max_dirty_paths {
            // The dirty set itself cannot be represented: keep the generation
            // monotonic and move the serviceable floor past it so every
            // older cached graph is forced to rebuild.
            state.history.clear();
            state.retained_paths = 0;
            state.history_start = generation.saturating_add(1);
            return generation;
        }
        while !state.history.is_empty()
            && (state.history.len() >= self.limits.max_generations
                || state.retained_paths + paths.len() > self.limits.max_dirty_paths)
        {
            let Some(oldest) = state.history.keys().next().copied() else {
                break;
            };
            if let Some(removed) = state.history.remove(&oldest) {
                state.retained_paths = state.retained_paths.saturating_sub(removed.len());
            }
        }
        state.retained_paths += paths.len();
        state.history.insert(generation, paths);
        state.history_start = state
            .history
            .keys()
            .next()
            .copied()
            .unwrap_or_else(|| generation.saturating_add(1));
        generation
    }

    /// All dirty paths strictly after `cached` up to the current generation.
    /// `lost` is set when the requested interval is not fully retained (or
    /// when the authority restarted behind the cached generation).
    pub fn dirty_since(&self, cached: u64) -> DirtyPaths {
        let state = self.lock();
        let generation = state.current;
        if cached == generation {
            return DirtyPaths {
                generation,
                paths: Vec::new(),
                lost: false,
            };
        }
        if cached > generation {
            return DirtyPaths {
                generation,
                paths: Vec::new(),
                lost: true,
            };
        }
        if cached.saturating_add(1) < state.history_start {
            return DirtyPaths {
                generation,
                paths: Vec::new(),
                lost: true,
            };
        }
        let mut paths = BTreeSet::new();
        for entry in state.history.range((
            std::ops::Bound::Excluded(cached),
            std::ops::Bound::Included(generation),
        )) {
            paths.extend(entry.1.iter().cloned());
        }
        DirtyPaths {
            generation,
            paths: paths.into_iter().collect(),
            lost: false,
        }
    }

    /// Explicit loss (watcher lag, discarded events or a corrupt dirty
    /// set). The token advances once so even a consumer that was exactly
    /// current observes `lost`: dropped events may have happened after its
    /// generation, and "no new paths recorded" must never be mistaken for
    /// "nothing changed". Returns the new current generation.
    pub fn mark_history_lost(&self) -> u64 {
        let mut state = self.lock();
        state.current = state.current.saturating_add(1);
        let generation = state.current;
        state.history.clear();
        state.retained_paths = 0;
        state.history_start = generation.saturating_add(1);
        generation
    }

    /// Rebind the authority after a restart. A consumer that cached a newer
    /// generation than `generation` sees `lost` and rebuilds; a consumer at
    /// exactly `generation` is current.
    pub fn reset(&self, generation: u64) {
        let mut state = self.lock();
        state.current = generation;
        state.history.clear();
        state.retained_paths = 0;
        state.history_start = generation.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_observations_never_advance() {
        let generation = MaterialGeneration::new();
        assert_eq!(generation.current(), 0);
        assert_eq!(generation.observe(Vec::<String>::new()), 0);
        assert_eq!(generation.observe(["", ""]), 0);
        assert_eq!(generation.current(), 0);
        let dirty = generation.dirty_since(0);
        assert!(dirty.paths.is_empty());
        assert!(!dirty.lost);
    }

    #[test]
    fn dirty_paths_union_every_generation_after_the_cache() {
        let generation = MaterialGeneration::new();
        generation.observe(["a.rs"]);
        generation.observe(["b.rs", "c.rs"]);
        assert_eq!(generation.current(), 2);
        let dirty = generation.dirty_since(0);
        assert_eq!(dirty.paths, vec!["a.rs", "b.rs", "c.rs"]);
        assert!(!dirty.lost);
        let tail = generation.dirty_since(1);
        assert_eq!(tail.paths, vec!["b.rs", "c.rs"]);
        let current = generation.dirty_since(2);
        assert!(current.paths.is_empty());
        assert!(!current.lost);
    }

    #[test]
    fn generation_overflow_forces_a_rebuild() {
        let generation = MaterialGeneration::with_limits(GenerationLimits {
            max_generations: 2,
            max_dirty_paths: 100,
        });
        generation.observe(["a.rs"]);
        generation.observe(["b.rs"]);
        generation.observe(["c.rs"]);
        assert_eq!(generation.current(), 3);
        let lost = generation.dirty_since(0);
        assert!(lost.lost, "dropped history must be lost: {lost:?}");
        let servable = generation.dirty_since(1);
        assert_eq!(servable.paths, vec!["b.rs", "c.rs"]);
        assert!(!servable.lost);
    }

    #[test]
    fn dirty_path_overflow_forces_a_rebuild() {
        let generation = MaterialGeneration::with_limits(GenerationLimits {
            max_generations: 8,
            max_dirty_paths: 2,
        });
        generation.observe(["a.rs", "b.rs", "c.rs"]);
        let dirty = generation.dirty_since(0);
        assert!(dirty.lost);
        assert!(dirty.paths.is_empty());
        // The token stays monotonic; the next small observation is exact.
        generation.observe(["d.rs"]);
        let next = generation.dirty_since(1);
        assert_eq!(next.paths, vec!["d.rs"]);
        assert!(!next.lost);
    }

    #[test]
    fn retained_paths_are_bounded_across_generations() {
        let generation = MaterialGeneration::with_limits(GenerationLimits {
            max_generations: 8,
            max_dirty_paths: 3,
        });
        generation.observe(["a.rs", "b.rs"]);
        generation.observe(["c.rs", "d.rs"]);
        let lost = generation.dirty_since(0);
        assert!(lost.lost, "the oldest generation must be dropped");
        let servable = generation.dirty_since(1);
        assert_eq!(servable.paths, vec!["c.rs", "d.rs"]);
        assert!(!servable.lost);
    }

    #[test]
    fn explicit_loss_and_restart_are_typed() {
        let generation = MaterialGeneration::new();
        generation.observe(["a.rs"]);
        generation.observe(["b.rs"]);
        let loss = generation.mark_history_lost();
        assert_eq!(loss, 3);
        assert!(generation.dirty_since(0).lost);
        assert!(generation.dirty_since(1).lost);
        assert!(
            generation.dirty_since(2).lost,
            "a consumer that was current when the tap lagged must rebuild"
        );
        assert!(!generation.dirty_since(3).lost);

        generation.reset(1);
        let restarted = generation.dirty_since(2);
        assert!(
            restarted.lost,
            "a restarted authority invalidates the cache"
        );
        assert_eq!(restarted.generation, 1);
    }
}
