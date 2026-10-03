//! Adversarial boundary corpus for the terminal budget authority.
//!
//! Requested/disabled parity, `RLIMIT_CPU` rounding boundaries, the
//! `merge_best` lattice over all state pairs, strict/unsupported violation
//! matrices, the per-axis rlimit plan, wall-deadline composition, job-object
//! mapping and real rlimit installation through a spawned child. Every row
//! asserts one exact outcome with its own message.

use std::time::Duration;

use super::budget::{
    deadline_with_wall, enforce_tree_budgets, install_child_rlimits, job_limits_enforcement,
    job_limits_for, rlimit_plan, rlimit_plan_enforcement, spawn_path_enforcement,
    BudgetEnforcement, BudgetPlatform, LimitState, RlimitResource, RlimitSpec, TreeBudgets,
    WallWatchdog,
};

fn budgets(cpu: u64, mem: u64, procs: u32, wall: u64) -> TreeBudgets {
    TreeBudgets {
        cpu_millis: cpu,
        memory_bytes: mem,
        max_processes: procs,
        wall_time_ms: wall,
    }
}

/// Requested/disabled parity and the CPU-seconds rounding boundary.
#[test]
fn tree_budget_request_parity_and_cpu_rounding() {
    let disabled = TreeBudgets::disabled();
    assert!(disabled.is_disabled(), "disabled() must report disabled");
    assert_eq!(
        disabled.requested(),
        [
            ("cpu", false),
            ("memory", false),
            ("processes", false),
            ("wall", false)
        ],
        "disabled() must request nothing"
    );
    let default = TreeBudgets::default();
    assert!(!default.is_disabled(), "default() enables all four");
    assert_eq!(
        default.requested(),
        [
            ("cpu", true),
            ("memory", true),
            ("processes", true),
            ("wall", true)
        ],
        "default() requests all four axes"
    );

    let axes: [(&str, TreeBudgets, usize); 4] = [
        ("cpu-only", budgets(1, 0, 0, 0), 0),
        ("memory-only", budgets(0, 1, 0, 0), 1),
        ("processes-only", budgets(0, 0, 1, 0), 2),
        ("wall-only", budgets(0, 0, 0, 1), 3),
    ];
    for (label, b, requested_index) in axes {
        assert!(
            !b.is_disabled(),
            "case {label}: one nonzero axis is a request"
        );
        let requested = b.requested();
        for (index, (_, present)) in requested.iter().enumerate() {
            assert_eq!(
                *present,
                index == requested_index,
                "case {label}: only axis {requested_index} may be requested: {requested:?}"
            );
        }
    }

    let cpu_rows: [(&str, u64, u64); 12] = [
        ("cpu-0", 0, 1),
        ("cpu-1ms", 1, 1),
        ("cpu-999ms", 999, 1),
        ("cpu-1000ms", 1000, 1),
        ("cpu-1001ms", 1001, 2),
        ("cpu-1999ms", 1999, 2),
        ("cpu-2000ms", 2000, 2),
        ("cpu-2001ms", 2001, 3),
        ("cpu-30min", 30 * 60 * 1000, 1800),
        ("cpu-1h", 60 * 60 * 1000, 3600),
        ("cpu-1ms-over-day", 24 * 60 * 60 * 1000 + 1, 86401),
        ("cpu-u64-max", u64::MAX, u64::MAX.div_ceil(1000).max(1)),
    ];
    for (label, millis, expected) in cpu_rows {
        assert_eq!(
            budgets(millis, 0, 0, 0).cpu_seconds_ceil(),
            expected,
            "case {label}: RLIMIT_CPU seconds must round up (never truncate)"
        );
    }
}

/// `merge_best` is a join over the total state order: the stronger state
/// wins for every pair, details concatenate without duplicates, and the
/// operation is idempotent.
#[test]
fn merge_best_lattice_is_total_and_deduplicated() {
    let states = [
        LimitState::NotRequested,
        LimitState::Unsupported,
        LimitState::Degraded,
        LimitState::Enforced,
    ];
    let rank = |s: LimitState| match s {
        LimitState::NotRequested => 0,
        LimitState::Unsupported => 1,
        LimitState::Degraded => 2,
        LimitState::Enforced => 3,
    };
    for a in states {
        for b in states {
            let left = BudgetEnforcement {
                cpu: a,
                memory: a,
                processes: a,
                wall: a,
                details: vec!["left".into()],
            };
            let right = BudgetEnforcement {
                cpu: b,
                memory: b,
                processes: b,
                wall: b,
                details: vec!["left".into(), "right".into()],
            };
            let merged = left.clone().merge_best(right.clone());
            let expect = if rank(b) > rank(a) { b } else { a };
            assert_eq!(
                merged.cpu, expect,
                "case pair({a:?},{b:?}): cpu must take the higher rank"
            );
            assert_eq!(
                merged.memory, expect,
                "case pair({a:?},{b:?}): memory must take the higher rank"
            );
            assert_eq!(
                merged.processes, expect,
                "case pair({a:?},{b:?}): processes must take the higher rank"
            );
            assert_eq!(
                merged.wall, expect,
                "case pair({a:?},{b:?}): wall must take the higher rank"
            );
            assert_eq!(
                merged.details,
                vec!["left".to_string(), "right".to_string()],
                "case pair({a:?},{b:?}): details must concatenate without duplicates"
            );
            let twice = merged.clone().merge_best(merged.clone());
            assert_eq!(
                twice, merged,
                "case pair({a:?},{b:?}): merge_best must be idempotent"
            );
        }
    }
}

/// `strict_violations` flags exactly the requested limits that ended
/// `Unsupported`; `Degraded` and `Enforced` are real mechanisms and never
/// violate, and unrequested axes never appear.
#[test]
fn strict_violations_matrix_is_exact() {
    let axes = ["cpu", "memory", "processes", "wall"];
    for (index, axis) in axes.iter().enumerate() {
        for state in [
            LimitState::NotRequested,
            LimitState::Unsupported,
            LimitState::Degraded,
            LimitState::Enforced,
        ] {
            let mut enforcement = BudgetEnforcement::not_requested();
            match index {
                0 => enforcement.cpu = state,
                1 => enforcement.memory = state,
                2 => enforcement.processes = state,
                _ => enforcement.wall = state,
            }
            let b = budgets(1000, 1024, 8, 1000);
            let violations = enforcement.strict_violations(&b);
            if state == LimitState::Unsupported {
                assert_eq!(
                    violations.len(),
                    1,
                    "case {axis}={state:?}: exactly one violation expected"
                );
                assert!(
                    violations[0].starts_with(axis),
                    "case {axis}={state:?}: the violation must name {axis}: {violations:?}"
                );
            } else {
                assert!(
                    violations.is_empty(),
                    "case {axis}={state:?}: only Unsupported violates strict mode: {violations:?}"
                );
            }
            // Unrequested axes never violate, whatever the state.
            let violations = enforcement.strict_violations(&TreeBudgets::disabled());
            assert!(
                violations.is_empty(),
                "case {axis}={state:?}: an unrequested budget never violates: {violations:?}"
            );
        }
    }
}

/// The pre-spawn platform gate: `all_unenforceable` refuses cpu/memory/
/// processes but never wall; `detect()` on this platform honestly reports its
/// documented best case.
#[test]
fn platform_unsupported_violation_matrix() {
    let forced = BudgetPlatform::all_unenforceable();
    let all = budgets(1, 1, 1, 1);
    let violations = forced.unsupported_violations(&all);
    assert_eq!(
        violations.len(),
        3,
        "the forced platform must refuse exactly cpu/memory/processes: {violations:?}"
    );
    for (index, axis) in ["cpu", "memory", "processes"].iter().enumerate() {
        assert!(
            violations[index].starts_with(axis),
            "violation {index} must name {axis}: {violations:?}"
        );
        assert!(
            violations[index].contains("forced unenforceable"),
            "the violation must carry the platform detail: {violations:?}"
        );
    }
    assert!(
        forced
            .unsupported_violations(&TreeBudgets::disabled())
            .is_empty(),
        "disabled budgets never violate the pre-spawn gate"
    );
    let wall_only = budgets(0, 0, 0, 1000);
    assert!(
        forced.unsupported_violations(&wall_only).is_empty(),
        "wall is always enforceable through the watchdog"
    );

    let detected = BudgetPlatform::detect();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(detected.cpu, LimitState::Degraded, "linux cpu best case");
        assert_eq!(
            detected.memory,
            LimitState::Degraded,
            "linux memory best case"
        );
        assert_eq!(
            detected.processes,
            LimitState::Degraded,
            "linux process best case"
        );
        assert_eq!(detected.wall, LimitState::Enforced, "linux wall best case");
        assert!(
            detected.unsupported_violations(&all).is_empty(),
            "linux has no unsupported requested axis"
        );
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(detected.cpu, LimitState::Unsupported);
        assert_eq!(detected.memory, LimitState::Unsupported);
        assert_eq!(detected.processes, LimitState::Unsupported);
        assert_eq!(detected.wall, LimitState::Enforced);
        assert_eq!(
            detected.unsupported_violations(&all).len(),
            3,
            "macos must honestly refuse cpu/memory/processes"
        );
    }
    assert!(
        !detected.detail.is_empty(),
        "the platform verdict must carry its reason"
    );
}

/// Per-axis rlimit plan: only requested axes appear, CPU is rounded up,
/// memory is present on Linux only.
#[test]
fn rlimit_plan_matrix_is_exact() {
    struct Row {
        label: &'static str,
        b: TreeBudgets,
        expect: Vec<RlimitSpec>,
    }
    let rows = vec![
        Row {
            label: "disabled-empty",
            b: TreeBudgets::disabled(),
            expect: vec![],
        },
        Row {
            label: "cpu-rounds-up",
            b: budgets(2500, 0, 0, 0),
            expect: vec![RlimitSpec {
                resource: RlimitResource::CpuSeconds,
                value: 3,
            }],
        },
        Row {
            label: "processes-exact",
            b: budgets(0, 0, 64, 0),
            expect: vec![RlimitSpec {
                resource: RlimitResource::Processes,
                value: 64,
            }],
        },
        Row {
            label: "wall-not-an-rlimit",
            b: budgets(0, 0, 0, 1000),
            expect: vec![],
        },
    ];
    for row in rows {
        let plan = rlimit_plan(&row.b);
        let mut expected = row.expect.clone();
        #[cfg(target_os = "linux")]
        if row.b.memory_bytes > 0 {
            expected.push(RlimitSpec {
                resource: RlimitResource::AddressSpace,
                value: row.b.memory_bytes,
            });
        }
        assert_eq!(plan, expected, "case {}: platform plan", row.label);
    }
    // Memory alone appears in the plan on Linux and only there.
    let memory_plan = rlimit_plan(&budgets(0, 4096, 0, 0));
    if cfg!(target_os = "linux") {
        assert_eq!(
            memory_plan,
            vec![RlimitSpec {
                resource: RlimitResource::AddressSpace,
                value: 4096,
            }],
            "linux must plan RLIMIT_AS for a memory budget"
        );
    } else {
        assert!(
            memory_plan.is_empty(),
            "a platform rejecting RLIMIT_AS must not plan it silently"
        );
    }
    // CPU + processes ordering (memory absent) is stable on every platform.
    assert_eq!(
        rlimit_plan(&budgets(1000, 0, 7, 0)),
        vec![
            RlimitSpec {
                resource: RlimitResource::CpuSeconds,
                value: 1,
            },
            RlimitSpec {
                resource: RlimitResource::Processes,
                value: 7,
            },
        ],
        "the plan order must be cpu, processes (memory appended only when requested)"
    );

    // Memory alone is Linux-only in the plan, but the enforcement report is
    // honest either way.
    let memory = budgets(0, 128 * 1024 * 1024, 0, 0);
    let report = rlimit_plan_enforcement(&memory);
    if cfg!(target_os = "linux") {
        assert_eq!(
            report.memory,
            LimitState::Degraded,
            "linux RLIMIT_AS is a real (per-process) mechanism"
        );
        assert!(
            !report.details.is_empty(),
            "the degraded state must carry its gap note"
        );
    } else {
        assert_eq!(
            report.memory,
            LimitState::Unsupported,
            "a platform rejecting RLIMIT_AS must say so typed"
        );
    }
    let enabled = budgets(1000, 1, 1, 0);
    let report = rlimit_plan_enforcement(&enabled);
    #[cfg(target_os = "linux")]
    {
        assert_eq!(report.cpu, LimitState::Degraded);
        assert_eq!(report.processes, LimitState::Degraded);
    }
    assert_eq!(
        report.wall,
        LimitState::NotRequested,
        "wall is caller-armed"
    );
    let disabled_report = rlimit_plan_enforcement(&TreeBudgets::disabled());
    assert_eq!(
        disabled_report,
        BudgetEnforcement::not_requested(),
        "a disabled plan reports nothing requested"
    );
}

/// Deadline composition and the Windows job mapping (asserted on every
/// platform: the mapping is pure data).
#[test]
fn deadline_and_job_limit_matrix() {
    let rows: [(&str, Duration, &TreeBudgets, Duration); 6] = [
        (
            "wall-disabled",
            Duration::from_secs(30),
            &TreeBudgets::disabled(),
            Duration::from_secs(30),
        ),
        (
            "wall-shorter",
            Duration::from_secs(30),
            &budgets(0, 0, 0, 1_000),
            Duration::from_secs(1),
        ),
        (
            "wall-longer",
            Duration::from_secs(30),
            &budgets(0, 0, 0, 60_000),
            Duration::from_secs(30),
        ),
        (
            "wall-equal",
            Duration::from_secs(5),
            &budgets(0, 0, 0, 5_000),
            Duration::from_secs(5),
        ),
        (
            "wall-zero-explicit",
            Duration::from_secs(7),
            &budgets(0, 0, 0, 0),
            Duration::from_secs(7),
        ),
        (
            "wall-overflow-max",
            Duration::from_secs(7),
            &budgets(0, 0, 0, u64::MAX),
            Duration::from_secs(7),
        ),
    ];
    for (label, deadline, b, expected) in rows {
        assert_eq!(
            deadline_with_wall(deadline, b),
            expected,
            "case {label}: the shorter bound must win"
        );
    }

    let b = budgets(1000, 1024, 8, 0);
    let job = job_limits_for(&b);
    assert_eq!(
        job.process_memory_bytes,
        Some(1024),
        "memory budget maps to the job memory limit"
    );
    assert_eq!(
        job.active_processes,
        Some(8),
        "process budget maps to the active-process limit"
    );
    assert!(job.kill_on_close, "the job must always kill on close");
    let disabled_job = job_limits_for(&TreeBudgets::disabled());
    assert_eq!(disabled_job.process_memory_bytes, None);
    assert_eq!(disabled_job.active_processes, None);

    let job_report = job_limits_enforcement(&b);
    assert_eq!(
        job_report.cpu,
        LimitState::Unsupported,
        "job objects have no total-CPU control"
    );
    assert_eq!(job_report.memory, LimitState::Enforced);
    assert_eq!(job_report.processes, LimitState::Enforced);

    let spawn_report = spawn_path_enforcement(&b);
    assert_ne!(
        spawn_report.cpu,
        LimitState::NotRequested,
        "a requested cpu budget must never be reported NotRequested"
    );
    assert_eq!(
        spawn_path_enforcement(&TreeBudgets::disabled()),
        BudgetEnforcement::not_requested(),
        "disabled spawn budgets report nothing requested"
    );
}

/// The wall watchdog fires exactly once at the deadline, can be cancelled
/// before it fires, and never fires after cancellation.
#[test]
fn wall_watchdog_fire_and_cancel_boundaries() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Fires once at/after the deadline.
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let watchdog = WallWatchdog::arm(
        30,
        Box::new(move || {
            hits2.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while hits.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the 30ms watchdog must fire within 3s"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(watchdog);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the watchdog must fire exactly once"
    );

    // Cancelled before the deadline: never fires.
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let watchdog = WallWatchdog::arm(
        80,
        Box::new(move || {
            hits2.fetch_add(1, Ordering::SeqCst);
        }),
    );
    watchdog.cancel();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "a cancelled watchdog must never fire"
    );
    drop(watchdog);

    // Zero-millisecond watchdog fires immediately (bounded poll).
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let watchdog = WallWatchdog::arm(
        0,
        Box::new(move || {
            hits2.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while hits.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a zero watchdog must fire promptly"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(watchdog);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "zero watchdog fires once");
}

/// `enforce_tree_budgets` against a real child: requested axes are never
/// reported `NotRequested`, disabled budgets report nothing, and the wall
/// stays caller-armed until `arm_wall`.
#[test]
fn enforce_tree_budgets_against_a_live_child_is_honest() {
    let spawn = || {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("fixture child")
    };
    let mut child = spawn();
    let guard = enforce_tree_budgets(child.id(), &TreeBudgets::disabled()).unwrap();
    assert_eq!(
        guard.enforcement(),
        &BudgetEnforcement::not_requested(),
        "case disabled: nothing may be claimed"
    );
    drop(guard);
    let _ = child.kill();
    let _ = child.wait();

    let rows: [(&str, TreeBudgets); 3] = [
        ("cpu-only", budgets(2500, 0, 0, 0)),
        ("memory-only", budgets(0, 256 * 1024 * 1024, 0, 0)),
        ("processes-only", budgets(0, 0, 128, 0)),
    ];
    for (label, b) in rows {
        let mut child = spawn();
        let guard = enforce_tree_budgets(child.id(), &b).unwrap_or_else(|e| {
            panic!("case {label}: enforcement against a live child must not refuse: {e}")
        });
        let report = guard.enforcement();
        let requested = [
            ("cpu", b.cpu_millis > 0, report.cpu),
            ("memory", b.memory_bytes > 0, report.memory),
            ("processes", b.max_processes > 0, report.processes),
        ];
        for (axis, requested, state) in requested {
            if requested {
                assert_ne!(
                    state,
                    LimitState::NotRequested,
                    "case {label}: requested axis {axis} must never read NotRequested"
                );
            } else {
                assert_eq!(
                    state,
                    LimitState::NotRequested,
                    "case {label}: unrequested axis {axis} must stay NotRequested"
                );
            }
            if state == LimitState::Unsupported {
                assert!(
                    !report.details.is_empty(),
                    "case {label}: an Unsupported axis must carry its reason"
                );
            }
        }
        assert_eq!(
            report.wall,
            LimitState::NotRequested,
            "case {label}: wall is armed separately"
        );
        drop(guard);
        let _ = child.kill();
        let _ = child.wait();
    }

    // arm_wall parity and honesty.
    let mut child = spawn();
    let mut guard = enforce_tree_budgets(child.id(), &budgets(0, 0, 0, 0)).unwrap();
    guard
        .arm_wall(0, || {})
        .expect("zero wall is a no-op, not an error");
    assert_eq!(
        guard.enforcement().wall,
        LimitState::NotRequested,
        "arm_wall(0) must stay NotRequested"
    );
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired2 = fired.clone();
    guard
        .arm_wall(30, move || {
            fired2.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("a real wall arms");
    assert_eq!(
        guard.enforcement().wall,
        LimitState::Enforced,
        "an armed wall is Enforced"
    );
    let enforcement = guard.into_enforcement();
    assert_eq!(enforcement.wall, LimitState::Enforced);
    let _ = child.kill();
    let _ = child.wait();

    // InvalidTree refusal is typed.
    let err = enforce_tree_budgets(0, &budgets(1000, 0, 0, 0)).unwrap_err();
    assert!(
        err.to_string().contains("leader pid 0"),
        "a pid-0 tree must be refused typed: {err}"
    );
}

/// Real rlimit installation through the public pre-exec path: the child sees
/// the rounded CPU seconds, byte-exact address space and process ceiling.
#[test]
#[cfg(unix)]
fn installed_child_rlimits_are_visible_in_the_child() {
    // The CPU and address-space assertions run through every POSIX sh.
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("ulimit -t; ulimit -v");
    let b = budgets(2500, 128 * 1024 * 1024, 64, 0);
    install_child_rlimits(&mut cmd, &b);
    let output = cmd.output().expect("child runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let cpu = lines.next().unwrap_or_default().trim();
    let address = lines.next().unwrap_or_default().trim();
    assert_eq!(
        cpu, "3",
        "case cpu: ulimit -t must show the rounded-up seconds: {text:?}"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        address, "131072",
        "case memory: ulimit -v must show 128 MiB in KiB: {text:?}"
    );
    #[cfg(not(target_os = "linux"))]
    let _ = address;

    // RLIMIT_NPROC visibility needs a shell with `ulimit -u` (dash lacks
    // it). The environment predicate is asserted, never a silent skip.
    let bash_available = std::process::Command::new("bash")
        .arg("-c")
        .arg(":")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        bash_available,
        "case processes: this host must provide bash for the RLIMIT_NPROC visibility probe"
    );
    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg("ulimit -u");
    install_child_rlimits(&mut cmd, &b);
    let output = cmd.output().expect("bash child runs");
    let procs = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        procs, "64",
        "case processes: ulimit -u must show the ceiling, got {procs:?}"
    );
}
