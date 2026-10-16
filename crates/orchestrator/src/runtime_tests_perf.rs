//! Orchestrator perf-wave evidence tests (split from `runtime_tests.rs` to
//! stay under the source-size ceiling; fixtures reach here via
//! `use super::*`).

use super::*;

#[test]
fn assignment_shape_check_is_one_pass_and_old_scans_are_equivalent() {
    // Evidence for the one-pass indexes in `assignment_shape_violations`:
    // the historical loop ran a plan `find` + a plan `position` + a
    // spawn-item scan per durable row (O(rows · items)); the indexed loop
    // is O(rows + items). The naive per-row work is measured here on the
    // same corpus for the before/after bound, and the verdicts are pinned.
    let n = 1500usize;
    let items: Vec<WorkItem> = (0..n)
        .map(|i| {
            let deps: Vec<String> = (0..i.saturating_sub(1))
                .rev()
                .take(2)
                .map(|d| format!("i{d:04}"))
                .collect();
            let deps: Vec<&str> = deps.iter().map(|s| s.as_str()).collect();
            wi(&format!("i{i:04}"), WorkKind::Analysis, &deps)
        })
        .collect();
    let plan = raw_plan(items);
    let specs: HashMap<String, ChildSpec> = plan
        .work_items
        .iter()
        .map(|w| (w.id.clone(), spec(&w.id)))
        .collect();
    let rows: Vec<WorkItemAssignment> = (0..n)
        .map(|i| WorkItemAssignment {
            run_id: "run-index".into(),
            item_id: format!("i{i:04}"),
            plan_step_index: i,
            child_id: format!("child-{i}"),
            ownership: OwnershipSpec::NoWrites,
            run_base_snapshot: None,
        })
        .collect();

    // Correctness: a sound corpus has no violations; adversarial rows are
    // still refused with the same messages (unknown item, wrong run,
    // wrong step, duplicate child).
    let clean =
        OrchestratorRuntime::assignment_shape_violations("run-index", &plan, &specs, &rows, &[]);
    assert!(clean.is_empty(), "sound corpus: {clean:?}");
    let mut hostile = rows.clone();
    hostile.push(WorkItemAssignment {
        run_id: "run-index".into(),
        item_id: "ghost".into(),
        plan_step_index: 0,
        child_id: "child-x".into(),
        ownership: OwnershipSpec::NoWrites,
        run_base_snapshot: None,
    });
    hostile.push(WorkItemAssignment {
        run_id: "run-index".into(),
        item_id: "i0007".into(),
        plan_step_index: 9999,
        child_id: "child-1".into(),
        ownership: OwnershipSpec::NoWrites,
        run_base_snapshot: None,
    });
    hostile.push(WorkItemAssignment {
        run_id: "other-run".into(),
        item_id: "i0001".into(),
        plan_step_index: 1,
        child_id: "child-y".into(),
        ownership: OwnershipSpec::NoWrites,
        run_base_snapshot: None,
    });
    let violations =
        OrchestratorRuntime::assignment_shape_violations("run-index", &plan, &specs, &hostile, &[]);
    let joined = violations.join("\n");
    assert!(joined.contains("unknown work item \"ghost\""), "{joined}");
    assert!(joined.contains("records plan step 9999"), "{joined}");
    assert!(joined.contains("names run other-run"), "{joined}");
    assert!(joined.contains("duplicate assignment"), "{joined}");

    // Bounded measurement: the old per-row work (find + position + spawn
    // scan) against the indexed check on the same corpus.
    let spawn_items: Vec<&str> = plan.work_items.iter().map(|w| w.id.as_str()).collect();
    let t0 = std::time::Instant::now();
    let mut sink = 0usize;
    for row in &rows {
        let item = plan
            .work_items
            .iter()
            .find(|w| w.id == row.item_id)
            .expect("known item");
        sink += plan
            .work_items
            .iter()
            .position(|w| w.id == item.id)
            .expect("position");
        sink += usize::from(spawn_items.contains(&item.id.as_str()));
    }
    let old_elapsed = t0.elapsed();
    let t1 = std::time::Instant::now();
    let indexed =
        OrchestratorRuntime::assignment_shape_violations("run-index", &plan, &specs, &rows, &[]);
    let new_elapsed = t1.elapsed();
    assert!(indexed.is_empty() && sink > 0);
    eprintln!(
        "assignment shape check ({n} items x {n} rows): old scans {old_elapsed:?} -> indexed {new_elapsed:?}"
    );
    assert!(
        new_elapsed < old_elapsed,
        "indexed check must beat per-row scans: old {old_elapsed:?} new {new_elapsed:?}"
    );
}

#[test]
fn dependency_index_matches_the_plan_scan_on_adversarial_plans() {
    // The per-settle `DependencyIndex` replaces per-transition plan scans:
    // dependents and deps must be byte-equal to the naive scan for
    // duplicate deps, diamonds, self-referencing rows (never validated
    // here — the index must merely not lose entries), and empty plans.
    let plans: Vec<Vec<WorkItem>> = vec![
        vec![],
        vec![wi("a", WorkKind::Analysis, &[])],
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
            wi("c", WorkKind::Analysis, &["a"]),
            wi("d", WorkKind::Analysis, &["b", "c"]),
            // duplicate dependency spelling
            wi("e", WorkKind::Analysis, &["a", "a", "d"]),
        ],
        // deep chain
        (0..600)
            .map(|i| {
                if i == 0 {
                    wi("c0000", WorkKind::Analysis, &[])
                } else {
                    let dep = format!("c{:04}", i - 1);
                    wi(&format!("c{i:04}"), WorkKind::Analysis, &[dep.as_str()])
                }
            })
            .collect(),
    ];
    for items in plans {
        let plan = raw_plan(items);
        let index = DependencyIndex::build(&plan);
        let mut probes = 0usize;
        for w in &plan.work_items {
            let naive: Vec<&str> = plan
                .work_items
                .iter()
                .filter(|d| d.depends_on.contains(&w.id))
                .map(|d| d.id.as_str())
                .collect();
            let indexed = index.dependents_of(&w.id);
            assert_eq!(
                naive,
                indexed.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                "dependents drift for {}",
                w.id
            );
            let mut deduped: Vec<&str> = Vec::new();
            for d in &w.depends_on {
                if !deduped.contains(&d.as_str()) {
                    deduped.push(d.as_str());
                }
            }
            assert_eq!(
                deduped,
                index
                    .deps_of(&w.id)
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>(),
                "deps drift for {}",
                w.id
            );
            probes += 1;
        }
        assert!(probes == plan.work_items.len());
    }
}
