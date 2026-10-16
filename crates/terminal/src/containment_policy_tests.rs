//! Portable containment-policy tests (split from `lib.rs` to stay under
//! the source-size ceiling; module identity unchanged).

use super::*;

/// Extract one item's `{...}` body by brace matching. Format-string
/// braces are balanced, so the scan is exact for these items.
fn body<'a>(src: &'a str, header: &str) -> &'a str {
    let start = src
        .find(header)
        .unwrap_or_else(|| panic!("source has no {header}"));
    let open = start + src[start..].find('{').expect("item body");
    let mut depth = 0usize;
    for (i, b) in src.as_bytes()[open..].iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open..open + i + 1];
                }
            }
            _ => {}
        }
    }
    panic!("unclosed item {header}");
}

/// Windows creation is suspended until the job owns the child: the pure
/// flag function is the ONLY source of the creation flags, and it is
/// exactly CREATE_SUSPENDED (0x4, winbase.h).
#[test]
fn windows_creation_is_suspended_until_assigned() {
    assert_eq!(
        win_containment_policy::creation_flags(),
        win_containment_policy::CREATE_SUSPENDED
    );
    assert_eq!(win_containment_policy::CREATE_SUSPENDED, 0x0000_0004);
}

/// Exposure is legal only with BOTH a successful strict assignment and a
/// verified membership; every other combination refuses the spawn.
#[test]
fn exposure_requires_assignment_and_verified_membership() {
    assert!(win_containment_policy::may_resume(true, true));
    assert!(!win_containment_policy::may_resume(false, true));
    assert!(!win_containment_policy::may_resume(true, false));
    assert!(!win_containment_policy::may_resume(false, false));
}

/// Source contract of the Windows spawn path: `assign_verify_resume`
/// executes the one legal order — assign_strict → membership check →
/// resume gate → resume — and every spawn path suspends on create and
/// prepares the job before spawning.
#[test]
fn windows_spawn_source_orders_assign_verify_resume() {
    let src = include_str!("lib.rs");
    let inner = body(src, "fn assign_verify_resume(");
    let i_assign = inner.find("assign_strict(").expect("strict assignment");
    let i_member = inner
        .find("contains(pid)")
        .expect("membership verification while suspended");
    let i_gate = inner.find("may_resume(").expect("exposure gate");
    let i_resume = inner.find("resume(pid)").expect("resume");
    assert!(
        i_assign < i_member,
        "membership must be verified after assignment"
    );
    assert!(
        i_member < i_gate,
        "the resume gate must follow membership verification"
    );
    assert!(i_gate < i_resume, "resume must be the last step");
    // The suspension hook is the job-preparation path's child, applied
    // before spawn on every entry point. (Scan the production source
    // only: this test's own literals must not count.)
    let production = src
        .split("mod containment_policy_tests")
        .next()
        .expect("test module split");
    let contain = body(production, "fn contain_on_create(mut cmd:");
    assert!(contain.contains("suspend_on_create"));
    // Every spawn entry point suspends on create: run/run_sync through
    // the budget-aware command builder, spawn/detached through the plain
    // env builder. The budget-aware form must still be contain_on_create.
    let contain_calls = production
        .matches("contain_on_create(self.command(&cfg))")
        .count()
        + production
            .matches("contain_on_create(self.command_with_budgets(&cfg, budgets.as_ref()))")
            .count();
    assert_eq!(
        contain_calls, 4,
        "every spawn entry point (run/run_sync/spawn/detached) must suspend on create"
    );
    // Every spawn entry point prepares the containment job (with budget
    // limits where requested) before spawning.
    let prepare_calls = production.matches("prepare_containment()?").count()
        + production
            .matches("prepare_containment_with_limits(budgets.as_ref())?")
            .count();
    assert_eq!(
        prepare_calls, 4,
        "every spawn entry point must prepare the containment job before spawning"
    );
}

/// Kill-path contract: no supervisor kill path references taskkill. The
/// primary authority is the per-child containment job (Windows) or the
/// process group (unix); taskkill survives exactly once, as the
/// documented best-effort helper for pids the supervisor never owned.
#[test]
fn supervisor_kill_paths_do_not_depend_on_taskkill() {
    let src = include_str!("lib.rs");
    let production = src
        .split("mod containment_policy_tests")
        .next()
        .expect("test module split");
    for header in [
        "async fn terminate_registered(",
        "fn terminate_registered_sync(",
        "pub fn kill(",
        "pub fn kill_all_for(",
        "impl Drop for ProcessSupervisor {",
    ] {
        let inner = body(production, header);
        assert!(
            !inner.contains("taskkill"),
            "{header} must not fall back to taskkill"
        );
        assert!(
            inner.contains("terminate_containment")
                || inner.contains("terminate_registered")
                || inner.contains("kill_group_async"),
            "{header} must terminate through the containment authority"
        );
    }
    assert_eq!(
        production.matches("Command::new(\"taskkill\")").count(),
        1,
        "taskkill must survive only as the single best-effort helper"
    );
    assert!(body(production, "fn taskkill_best_effort(").contains("taskkill"));
    // kill_child_pid routes a registered pid through the guarded kill;
    // an unowned or already-consumed pid is refused typed and never
    // raw-signalled.
    let kcp = body(production, "pub fn kill_child_pid(");
    assert!(kcp.contains("terminate_registered_sync"));
    assert!(!kcp.contains("kill_group"));
    assert!(!kcp.contains("libc::kill"));
    assert!(body(production, "fn kill_group(").contains("taskkill_best_effort"));
}

/// Pid-reuse guard contract (portable source scan): the unix signal
/// issuance locks the reap serial BEFORE observing `reaped` and only
/// calls the kernel after both, and no supervisor kill path raw-signals
/// a stored pid anymore.
#[test]
fn unix_signals_are_reap_serialized_and_kill_paths_are_guarded() {
    let src = include_str!("lib.rs");
    let production = src
        .split("mod containment_policy_tests")
        .next()
        .expect("test module split");
    let issue = body(production, "fn try_signal_group(");
    let i_serial = issue.find(".serial").expect("reap serial lock");
    let i_reaped = issue.find("self.reaped()").expect("reaped observation");
    let i_kill = issue.find("libc::kill").expect("guarded signal");
    assert!(
        i_serial < i_reaped && i_reaped < i_kill,
        "the reap serial must cover [observe reaped → signal]"
    );
    for header in [
        "async fn terminate_registered(",
        "fn terminate_registered_sync(",
        "pub fn kill_child_pid(",
        "pub fn kill_all_for(",
        "impl Drop for ProcessSupervisor {",
    ] {
        let inner = body(production, header);
        assert!(
            !inner.contains("libc::kill"),
            "{header} must signal only through the guarded issuance"
        );
        assert!(
            !inner.contains("kill_group(") && !inner.contains("kill_group_async("),
            "{header} must not raw-signal a stored pid"
        );
    }
    // The raw unix group helpers are test-only: production registered
    // kills route through ReapState, so a raw negative pgid is never
    // signalled by production API.
    assert!(production.contains("#[cfg(all(test, unix))]\nfn kill_group("));
    assert!(production.contains("#[cfg(all(test, unix))]\nasync fn kill_group_async("));
}
