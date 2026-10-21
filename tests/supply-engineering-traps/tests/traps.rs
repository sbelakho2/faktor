use std::time::{Duration, Instant};

use faktor_supply_engineering_traps::fixtures::{
    convergence_pathology, manufactured_solution_study, mesh_fixture_probe,
};
use faktor_supply_engineering_traps::{
    case_ids, run_case, run_suite, BenchError, CaseOutcome, ExpectedKind, Observation, SuiteReport,
    MAX_BOUNDARIES, MAX_CASES, MAX_EVIDENCE, MAX_MISSING_INFO, MAX_OBSERVATIONS,
};

#[test]
fn suite_is_deterministic_and_every_case_passes_twice() {
    let first = run_suite().expect("first suite run");
    let second = run_suite().expect("second suite run");
    assert_eq!(first, second, "suite report must be deterministic");
    assert_eq!(
        first.to_json(false).expect("first json"),
        second.to_json(false).expect("second json"),
        "serialized report must be byte-stable"
    );
    assert_eq!(first.cases.len(), case_ids().len());
    assert_eq!(first.cases.len(), 13);
    assert!(
        first.summary.all_passed(),
        "failed cases: {:?}",
        first.summary.failed_cases
    );
    assert_eq!(first.summary.total, first.summary.passed);
}

#[test]
fn every_case_binds_evidence_boundaries_and_a_falsifiable_experiment() {
    let suite = run_suite().expect("suite");
    for case in &suite.cases {
        assert!(!case.id.is_empty(), "empty id");
        assert!(!case.title.is_empty(), "empty title: {}", case.id);
        assert!(
            !case.shallow_claim.is_empty(),
            "no shallow claim: {}",
            case.id
        );
        assert!(!case.evidence.is_empty(), "no evidence: {}", case.id);
        assert!(
            case.evidence.len() <= MAX_EVIDENCE,
            "evidence bound: {}",
            case.id
        );
        assert!(
            !case.missing_info.is_empty(),
            "no missing info: {}",
            case.id
        );
        assert!(
            case.missing_info.len() <= MAX_MISSING_INFO,
            "missing bound: {}",
            case.id
        );
        assert!(
            !case.boundary_conditions.is_empty(),
            "no boundary: {}",
            case.id
        );
        assert!(
            case.boundary_conditions.len() <= MAX_BOUNDARIES,
            "boundary bound: {}",
            case.id
        );
        assert!(
            !case.uncertainty_statement.is_empty(),
            "no uncertainty: {}",
            case.id
        );
        assert!(
            !case.falsifiable_test.claim.is_empty()
                && !case.falsifiable_test.procedure.is_empty()
                && !case.falsifiable_test.pass_criterion.is_empty()
                && !case.falsifiable_test.refutation.is_empty()
                && !case.falsifiable_test.instrument.is_empty(),
            "incomplete falsifiable test: {}",
            case.id
        );
        assert_eq!(
            case.outcome.kind(),
            case.expected,
            "outcome mismatch: {}",
            case.id
        );
        assert!(case.metrics.all(), "metrics failed: {}", case.id);
    }
}

#[test]
fn filtering_is_exact_and_unknown_cases_are_typed() {
    assert!(matches!(run_case(""), Err(BenchError::EmptyCaseId)));
    assert!(matches!(
        run_case("not/a/case"),
        Err(BenchError::UnknownCase(_))
    ));
    for id in case_ids() {
        let report = run_case(id).expect("known case");
        assert_eq!(report.id, id);
        let single = SuiteReport::from_cases(vec![report]).expect("single report");
        assert_eq!(single.summary.total, 1);
        assert_eq!(single.summary.passed, 1);
    }
}

#[test]
fn report_is_machine_readable_and_bounded() {
    let suite = run_suite().expect("suite");
    assert!(suite.cases.len() <= MAX_CASES);
    let text = suite.to_json(false).expect("json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(value["schema"], "faktor.supply-engineering-traps/v1");
    assert_eq!(value["cases"].as_array().expect("cases").len(), 13);
    for case in value["cases"].as_array().expect("cases") {
        let metrics = case["metrics"].as_object().expect("metrics");
        for key in [
            "correct_physics",
            "authority",
            "missing_info_detected",
            "boundary_conditions",
            "uncertainty",
            "trap_detected",
            "falsifiable_experiment",
            "refused_where_appropriate",
        ] {
            assert_eq!(metrics[key], true, "metric {key} in {}", case["id"]);
        }
    }
    assert!(!text.contains("NaN"), "NaN in report");
    assert!(!text.contains("Infinity"), "infinity in report");
    for case in &suite.cases {
        if let CaseOutcome::Solved { observations, .. } = &case.outcome {
            assert!(
                observations.len() <= MAX_OBSERVATIONS,
                "observation bound: {}",
                case.id
            );
        }
    }
}

#[test]
fn money_observations_stay_exact_integers() {
    let suite = run_suite().expect("suite");
    let supplier = suite
        .cases
        .iter()
        .find(|case| case.id == "supply/cheaper-supplier-tco")
        .expect("supplier case");
    match &supplier.outcome {
        CaseOutcome::Solved { observations, .. } => {
            let money: Vec<&Observation> = observations
                .iter()
                .filter(|observation| observation.is_money())
                .collect();
            assert_eq!(money.len(), 6);
            for observation in money {
                if let Observation::Money {
                    micros, currency, ..
                } = observation
                {
                    assert_eq!(currency, "USD");
                    assert!(*micros >= 0, "negative fixture money");
                }
            }
        }
        other => panic!("supplier case outcome: {other:?}"),
    }
}

#[test]
fn unknown_cost_and_refusal_outcomes_are_not_solved() {
    let suite = run_suite().expect("suite");
    let unknown = suite
        .cases
        .iter()
        .find(|case| case.id == "supply/unknown-cost-not-zero")
        .expect("unknown-cost case");
    assert_eq!(unknown.expected, ExpectedKind::Unavailable);
    assert!(matches!(&unknown.outcome, CaseOutcome::Unavailable { .. }));
    let refusals = suite
        .cases
        .iter()
        .filter(|case| {
            matches!(
                case.expected,
                ExpectedKind::Refused | ExpectedKind::Rejected | ExpectedKind::ResearchRequired
            )
        })
        .count();
    assert_eq!(refusals, 9);
    assert_eq!(suite.summary.refused, 4);
    assert_eq!(suite.summary.rejected, 1);
    assert_eq!(suite.summary.research_required, 4);
    assert_eq!(suite.summary.unavailable, 1);
    assert_eq!(suite.summary.escalated, 1);
    assert_eq!(suite.summary.solved, 2);
}

#[test]
fn suite_completes_inside_a_fixed_time_budget() {
    let started = Instant::now();
    let suite = run_suite().expect("suite");
    let elapsed = started.elapsed();
    assert!(suite.summary.all_passed());
    assert!(
        elapsed < Duration::from_secs(5),
        "suite exceeded the 5s bound: {elapsed:?}"
    );
}

#[test]
fn adversarial_fixtures_are_refused_or_detected() {
    let probe = mesh_fixture_probe();
    assert!(probe.ok(), "mesh fixtures: {probe:?}");
    assert!(convergence_pathology(&[0.40, 0.35, 0.36, 0.22]).is_some());
    assert!(convergence_pathology(&[0.40, 0.20, 0.10, 0.095]).is_some());
    assert!(convergence_pathology(&[0.4, f64::NAN, 0.1]).is_some());
    let study = manufactured_solution_study().expect("manufactured solution");
    assert_eq!(study.len(), 5);
    assert!(study.windows(2).all(|pair| pair[1].1 < pair[0].1));
}
