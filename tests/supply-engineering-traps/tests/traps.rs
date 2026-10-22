use std::time::{Duration, Instant};

use faktor_supply_engineering_traps::{
    case_ids, evaluate_answer, negative_control, oracle_expectation, run_case, run_suite,
    BenchError, ExpectedKind, Observation, SuiteReport, SutOutcome, MAX_BOUNDARIES, MAX_CASES,
    MAX_EVIDENCE, MAX_MISSING_INFO, MAX_OBSERVATIONS,
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
    assert_eq!(first.cases.len(), 15);
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
        assert!(!case.sut.evidence.is_empty(), "no evidence: {}", case.id);
        assert!(
            case.sut.evidence.len() <= MAX_EVIDENCE,
            "evidence bound: {}",
            case.id
        );
        assert!(
            !case.sut.missing_info.is_empty(),
            "no missing info: {}",
            case.id
        );
        assert!(
            case.sut.missing_info.len() <= MAX_MISSING_INFO,
            "missing bound: {}",
            case.id
        );
        assert!(
            !case.sut.boundary_conditions.is_empty(),
            "no boundary: {}",
            case.id
        );
        assert!(
            case.sut.boundary_conditions.len() <= MAX_BOUNDARIES,
            "boundary bound: {}",
            case.id
        );
        assert!(
            !case.sut.experiments.is_empty(),
            "no experiment: {}",
            case.id
        );
        assert!(
            case.sut.experiments.iter().all(|test| {
                !test.claim.is_empty()
                    && !test.procedure.is_empty()
                    && !test.pass_criterion.is_empty()
                    && !test.refutation.is_empty()
                    && !test.instrument.is_empty()
                    && !test.evidence.is_empty()
            }),
            "incomplete falsifiable test: {}",
            case.id
        );
        assert_eq!(
            case.sut.outcome.kind(),
            case.expected,
            "SUT outcome does not match the oracle expectation: {}",
            case.id
        );
        assert!(case.oracle.all(), "oracle failed: {}", case.id);
        assert_eq!(case.oracle.count(), 8);
    }
}

#[test]
fn oracle_is_independent_and_sut_has_no_metric_api() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let oracle_source = std::fs::read_to_string(format!("{manifest}/src/oracle/mod.rs"))
        .expect("oracle source")
        + &std::fs::read_to_string(format!("{manifest}/src/oracle/expectations.rs"))
            .expect("expectations source");
    for forbidden in [
        "crate::sut",
        "engineering_models",
        "engineering_solvers",
        "faktor_supply_cost",
        "faktor_supply_logistics",
        "faktor_supply_core",
    ] {
        assert!(
            !oracle_source.contains(forbidden),
            "oracle imports SUT/kernel code: {forbidden}"
        );
    }
    let contract_source =
        std::fs::read_to_string(format!("{manifest}/src/contract.rs")).expect("contract source");
    for forbidden in [
        "with_physics",
        "with_authority",
        "with_missing_info",
        "with_boundaries",
        "with_uncertainty",
        "with_trap",
        "with_experiment",
        "with_refusal",
    ] {
        assert!(
            !contract_source.contains(forbidden),
            "SUT contract exposes a metric setter: {forbidden}"
        );
    }
    let lib_source = std::fs::read_to_string(format!("{manifest}/src/lib.rs")).expect("lib source");
    assert!(!lib_source.contains("pub struct Metrics"));
    assert!(!lib_source.contains("with_physics"));
    // Every case has exactly one independent expectation: no aliasing lets a
    // case score itself against another case's oracle.
    let ids = case_ids();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate case id");
    for id in ids {
        assert!(
            oracle_expectation(id).is_some(),
            "case {id} has no independent oracle expectation"
        );
    }
    let suite = run_suite().expect("suite");
    let value = serde_json::to_value(&suite).expect("json value");
    for case in value["cases"].as_array().expect("cases") {
        assert!(
            case.get("metrics").is_none(),
            "legacy metrics field survived"
        );
        let oracle = case["oracle"].as_object().expect("oracle");
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
            assert!(oracle.contains_key(key), "oracle dimension {key} missing");
        }
        assert_eq!(oracle["correct_physics"], true);
        assert_eq!(oracle["authority"], true);
    }
}

#[test]
fn negative_controls_fail_the_oracle_for_every_case() {
    let suite = run_suite().expect("suite");
    let mut rejected = 0;
    for case in &suite.cases {
        let expectation = oracle_expectation(&case.id).expect("expectation");
        let wrong = negative_control(case);
        let verdict = evaluate_answer(expectation, &wrong);
        assert!(
            !verdict.all(),
            "a deliberately wrong SUT answer passed case {}",
            case.id
        );
        assert!(
            !verdict.correct_physics || !verdict.refused_where_appropriate,
            "wrong answer kept the physics/refusal dimensions: {}",
            case.id
        );
        rejected += 1;
    }
    assert_eq!(rejected, 15);
}

#[test]
fn targeted_shallow_answers_are_rejected() {
    let suite = run_suite().expect("suite");
    let units = suite
        .cases
        .iter()
        .find(|case| case.id == "units/torque-lbf-ft")
        .expect("units case");
    let expectation = oracle_expectation(&units.id).expect("units expectation");
    let mut wrong = units.sut.clone();
    if let SutOutcome::Solved { observations } = &mut wrong.outcome {
        for observation in observations {
            if let Observation::Quantity { name, value, .. } = observation {
                if name == "100 lbf*ft in N*m" {
                    *value = 100.0;
                }
            }
        }
    }
    assert!(!evaluate_answer(expectation, &wrong).all());

    let thermal = suite
        .cases
        .iter()
        .find(|case| case.id == "thermal/missing-contact-resistance")
        .expect("thermal case");
    let expectation = oracle_expectation(&thermal.id).expect("thermal expectation");
    let mut wrong = thermal.sut.clone();
    wrong.outcome = SutOutcome::Solved {
        observations: vec![Observation::quantity("junction_temperature", 100.0, "degC")],
    };
    wrong.uncertainty = faktor_supply_engineering_traps::SutUncertainty::NotEstimated {
        reason: "fabricated".to_string(),
    };
    assert!(!evaluate_answer(expectation, &wrong).all());

    let supplier = suite
        .cases
        .iter()
        .find(|case| case.id == "supply/cheaper-supplier-tco")
        .expect("supplier case");
    let expectation = oracle_expectation(&supplier.id).expect("supplier expectation");
    let mut wrong = supplier.sut.clone();
    if let SutOutcome::Solved { observations } = &mut wrong.outcome {
        for observation in observations {
            if let Observation::Money { name, micros, .. } = observation {
                if name == "cheap per unit" {
                    *micros = 8_000_000;
                }
            }
        }
    }
    assert!(!evaluate_answer(expectation, &wrong).all());
}

#[test]
fn typed_refusal_and_ambiguity_cases_are_represented() {
    let suite = run_suite().expect("suite");
    let refused = suite
        .cases
        .iter()
        .filter(|case| case.expected == ExpectedKind::Refused)
        .count();
    let rejected = suite
        .cases
        .iter()
        .filter(|case| case.expected == ExpectedKind::Rejected)
        .count();
    let research = suite
        .cases
        .iter()
        .filter(|case| case.expected == ExpectedKind::ResearchRequired)
        .count();
    let unavailable = suite
        .cases
        .iter()
        .filter(|case| case.expected == ExpectedKind::Unavailable)
        .count();
    let ambiguous = suite
        .cases
        .iter()
        .filter(|case| case.expected == ExpectedKind::Ambiguous)
        .count();
    assert_eq!(refused, 4);
    assert_eq!(rejected, 3);
    assert_eq!(research, 4);
    assert_eq!(unavailable, 1);
    assert_eq!(ambiguous, 1);
    assert_eq!(refused + rejected + research + unavailable + ambiguous, 13);
    assert_eq!(suite.summary.refused, 4);
    assert_eq!(suite.summary.rejected, 3);
    assert_eq!(suite.summary.research_required, 4);
    assert_eq!(suite.summary.unavailable, 1);
    assert_eq!(suite.summary.ambiguous, 1);
    assert_eq!(suite.summary.solved, 2);
    for case in &suite.cases {
        if case.expected.is_typed_refusal() {
            assert!(
                !matches!(case.sut.outcome, SutOutcome::Solved { .. }),
                "typed refusal case solved: {}",
                case.id
            );
            assert!(case.oracle.refused_where_appropriate);
        }
    }
}

#[test]
fn research_required_is_justified_by_absent_pack_coverage() {
    let suite = run_suite().expect("suite");
    for case in &suite.cases {
        if case.expected != ExpectedKind::ResearchRequired {
            continue;
        }
        let expectation = oracle_expectation(&case.id).expect("expectation");
        assert!(
            matches!(
                expectation.authority,
                faktor_supply_engineering_traps::AuthorityExpectation::NoPackCoverage(_)
                    | faktor_supply_engineering_traps::AuthorityExpectation::PackText { .. }
            ),
            "research case {} lacks an independent authority check",
            case.id
        );
        assert!(
            case.oracle.authority,
            "oracle could not justify research for {}",
            case.id
        );
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
    assert_eq!(value["schema"], "faktor.supply-engineering-traps/v2");
    assert_eq!(value["cases"].as_array().expect("cases").len(), 15);
    for case in value["cases"].as_array().expect("cases") {
        assert!(case["sut"]["outcome"]["kind"].is_string());
        assert!(case["oracle"]["failed_dimensions"].is_array());
    }
    assert!(!text.contains("NaN"), "NaN in report");
    assert!(!text.contains("Infinity"), "infinity in report");
    for case in &suite.cases {
        assert!(case.sut.outcome.observations().len() <= MAX_OBSERVATIONS);
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
    match &supplier.sut.outcome {
        SutOutcome::Solved { observations } => {
            let money: Vec<&Observation> = observations
                .iter()
                .filter(|observation| matches!(observation, Observation::Money { .. }))
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

// Mesh and manufactured-solution adversarial coverage lives in the fixture
// unit tests, which exercise the same real parser without a duplicate suite run.
