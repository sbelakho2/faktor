//! Deterministic interleaving harness for the HAR-04/HAR-06/HAR-08 audit
//! rows — a loom/shuttle-style step scheduler WITHOUT an external
//! dependency.
//!
//! ## Model
//!
//! Every operation pair in this system serializes through the store's
//! single durable writer (one SQLite writer owner), so the set of
//! observable executions equals the set of ORDER-PRESERVING interleavings
//! of the actors' operation sequences. The harness enumerates that set
//! exhaustively ([`interleavings`]) and drives each schedule step by step
//! ([`drive_schedule`]); a fresh durable fixture is built per schedule, so
//! each exploration starts from an identical world and a failing schedule
//! is replayable by index.
//!
//! Bound: the enumerator refuses programs whose step count would grow past
//! a few hundred schedules (binomial cap) — the scenarios in `har04.rs`
//! are deliberately 2 actors × 1..3 steps (≤ 20 schedules each), fully
//! enumerated in milliseconds.
//!
//! ## Honest coverage note
//!
//! Steps are EXTERNAL operations (whole API calls). Interleavings INSIDE
//! one indivisible transaction are not step-addressable from outside the
//! production crates (which this test crate may not modify); those are
//! covered by the store's deterministic crash seams enumerated at every
//! named durability point instead (see the `har04` seam test).
// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir

/// One schedule entry: `(actor_index, step_index)`.
pub type ScheduleEntry = (usize, usize);

/// Hard cap on enumerated schedules (protects the suite from a combinatorial
/// accident in future callers).
pub const MAX_SCHEDULES: usize = 512;

/// Every order-preserving interleaving of the given per-actor program
/// lengths: each schedule executes every step exactly once and each
/// actor's steps stay in program order.
///
/// Deterministic output order (recursion picks the first actor first), so
/// schedule index N is stable across runs and platforms.
pub fn interleavings(program_lens: &[usize]) -> Vec<Vec<ScheduleEntry>> {
    let total: usize = program_lens.iter().sum();
    assert!(!program_lens.is_empty(), "at least one actor");
    let schedules = binomial_checked(total, program_lens);
    assert!(
        schedules <= MAX_SCHEDULES,
        "interleavings({program_lens:?}) would enumerate {schedules} schedules \
         (cap {MAX_SCHEDULES}); shrink the programs"
    );
    let mut out = Vec::with_capacity(schedules);
    let mut current = Vec::with_capacity(total);
    let mut used = vec![0usize; program_lens.len()];
    enumerate(program_lens, &mut used, &mut current, &mut out);
    out
}

fn binomial(n: usize, k: usize) -> usize {
    let k = k.min(n - k);
    let mut result: usize = 1;
    for i in 0..k {
        result = result.saturating_mul(n - i) / (i + 1);
    }
    result
}

fn binomial_checked(total: usize, program_lens: &[usize]) -> usize {
    // Multi-class multinomial = binomial over the FIRST split times the
    // rest; for the bounded program sizes here a simple per-actor fold of
    // binomials is exact: C(n, k1) * C(n-k1, k2) * ...
    let mut remaining = total;
    let mut ways: usize = 1;
    for (index, len) in program_lens.iter().enumerate() {
        if index + 1 == program_lens.len() {
            ways = ways.saturating_mul(1);
            break;
        }
        ways = ways.saturating_mul(binomial(remaining, *len));
        remaining -= len;
    }
    ways
}

fn enumerate(
    program_lens: &[usize],
    used: &mut [usize],
    current: &mut Vec<ScheduleEntry>,
    out: &mut Vec<Vec<ScheduleEntry>>,
) {
    if current.len() == program_lens.iter().sum::<usize>() {
        out.push(current.clone());
        return;
    }
    for actor in 0..program_lens.len() {
        if used[actor] >= program_lens[actor] {
            continue;
        }
        let step = used[actor];
        used[actor] += 1;
        current.push((actor, step));
        enumerate(program_lens, used, current, out);
        current.pop();
        used[actor] -= 1;
    }
}

/// Drive ONE schedule: `step(actor, step_index)` is awaited exactly in
/// schedule order on the current runtime; every step's output is collected
/// IN SCHEDULE ORDER so the caller can assert per-step outcomes without
/// mutating shared state from inside the closure.
pub async fn drive_schedule<F, Fut, T>(schedule: &[ScheduleEntry], mut step: F) -> Vec<T>
where
    F: FnMut(usize, usize) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut outputs = Vec::with_capacity(schedule.len());
    for &(actor, index) in schedule {
        outputs.push(step(actor, index).await);
    }
    outputs
}

#[cfg(test)]
mod tests {
    // TEST-SEPARATION: oracle=independent-reference isolation=pure-unit
    use super::*;

    #[test]
    fn enumeration_is_exhaustive_ordered_and_bounded() {
        // 2 actors × 2 steps: the canonical binomial(4,2) = 6 schedules.
        let schedules = interleavings(&[2, 2]);
        assert_eq!(schedules.len(), 6, "{schedules:?}");
        // Reference oracle: the six orders, independent of the generator's
        // internals (hand-written expectation, not derived output).
        let fixture: Vec<Vec<ScheduleEntry>> = vec![
            vec![(0, 0), (0, 1), (1, 0), (1, 1)],
            vec![(0, 0), (1, 0), (0, 1), (1, 1)],
            vec![(0, 0), (1, 0), (1, 1), (0, 1)],
            vec![(1, 0), (0, 0), (0, 1), (1, 1)],
            vec![(1, 0), (0, 0), (1, 1), (0, 1)],
            vec![(1, 0), (1, 1), (0, 0), (0, 1)],
        ];
        assert_eq!(schedules, fixture);
        for schedule in &schedules {
            // Every step exactly once, per-actor order preserved.
            let mut seen_a = Vec::new();
            let mut seen_b = Vec::new();
            for &(actor, step) in schedule {
                match actor {
                    0 => seen_a.push(step),
                    _ => seen_b.push(step),
                }
            }
            assert_eq!(seen_a, vec![0, 1], "actor 0 program order");
            assert_eq!(seen_b, vec![0, 1], "actor 1 program order");
        }
        // Determinism: same input, byte-identical schedule set.
        assert_eq!(schedules, interleavings(&[2, 2]));
        // Single-actor degenerates to the program itself.
        assert_eq!(interleavings(&[3]), vec![vec![(0, 0), (0, 1), (0, 2)]]);
    }

    #[test]
    #[should_panic(expected = "would enumerate")]
    fn enumeration_refuses_combinatorial_explosions() {
        let _ = interleavings(&[8, 8, 8, 8]);
    }

    #[tokio::test]
    async fn drive_schedule_runs_steps_in_exact_schedule_order() {
        let executed = drive_schedule(&[(1, 0), (0, 0), (1, 1)], |actor, step| {
            std::future::ready((actor, step))
        })
        .await;
        assert_eq!(executed, vec![(1, 0), (0, 0), (1, 1)]);
    }
}
