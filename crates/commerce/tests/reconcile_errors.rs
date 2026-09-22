//! Adversarial tests for multi-extractor reconciliation, typed source
//! errors and connector health (`docs/acquire.md` §8 and §15).

mod common;

use common::*;
use faktor_commerce::*;

fn observation(origin: ObservationOrigin, value: &str, observed_at_ms: u64) -> Observation<String> {
    Observation {
        value: value.to_string(),
        origin,
        observed_at_ms,
    }
}

fn policy(mode: ReconciliationMode) -> ReconciliationPolicy {
    ReconciliationPolicy {
        mode,
        max_observations: 16,
    }
}

#[test]
fn agreeing_observations_corroborate_with_the_highest_authority() {
    let observations = vec![
        observation(ObservationOrigin::Dom, "18.20", 10),
        observation(ObservationOrigin::OfficialApi, "18.20", 5),
        observation(ObservationOrigin::BrowserNetwork, "18.20", 20),
    ];
    let resolved = resolve_field(&observations, &policy(ReconciliationMode::RequireAgreement));
    match resolved {
        ResolvedField::Value {
            value,
            authority,
            corroborated_by,
        } => {
            assert_eq!(value, "18.20");
            assert_eq!(authority, ObservationOrigin::OfficialApi);
            assert_eq!(
                corroborated_by,
                vec![ObservationOrigin::Dom, ObservationOrigin::BrowserNetwork]
            );
        }
        other => panic!("expected a value, got {other:?}"),
    }
}

#[test]
fn conflicting_prices_are_recorded_and_never_averaged() {
    let observations = vec![
        observation(ObservationOrigin::OfficialApi, "36.00", 10),
        observation(ObservationOrigin::Dom, "2.00", 20),
    ];
    let resolved = resolve_field(&observations, &policy(ReconciliationMode::RequireAgreement));
    match &resolved {
        ResolvedField::Conflict { observations } => {
            assert_eq!(observations.len(), 2);
            assert!(resolved.value().is_none());
        }
        other => panic!("expected a recorded conflict, got {other:?}"),
    }
    // The winner by documented authority ordering is exactly one of the
    // observations, never an average (no 19.00).
    let winner = resolved.authoritative().expect("decidable conflict");
    assert_eq!(winner.value, "36.00");
    assert_eq!(winner.origin, ObservationOrigin::OfficialApi);
    assert_ne!(winner.value, "19.00");
}

#[test]
fn authority_ordering_mode_applies_the_winner_and_records_corroboration() {
    let observations = vec![
        observation(ObservationOrigin::RenderedText, "36.00", 30),
        observation(ObservationOrigin::OfficialApi, "36.00", 10),
        observation(ObservationOrigin::Dom, "2.00", 20),
    ];
    let resolved = resolve_field(
        &observations,
        &policy(ReconciliationMode::AuthorityOrdering),
    );
    match resolved {
        ResolvedField::Value {
            value,
            authority,
            corroborated_by,
        } => {
            assert_eq!(value, "36.00");
            assert_eq!(authority, ObservationOrigin::OfficialApi);
            assert_eq!(corroborated_by, vec![ObservationOrigin::RenderedText]);
        }
        other => panic!("expected an authority-ordered value, got {other:?}"),
    }
}

#[test]
fn a_tie_at_the_top_authority_stays_a_conflict() {
    let observations = vec![
        observation(ObservationOrigin::OfficialApi, "36.00", 10),
        observation(ObservationOrigin::OfficialApi, "2.00", 10),
    ];
    let resolved = resolve_field(
        &observations,
        &policy(ReconciliationMode::AuthorityOrdering),
    );
    assert!(resolved.is_conflict());
    assert!(resolved.authoritative().is_none(), "genuinely undecidable");
}

#[test]
fn the_newer_observation_wins_at_equal_authority() {
    let observations = vec![
        observation(ObservationOrigin::HttpJson, "36.00", 10),
        observation(ObservationOrigin::HttpJson, "36.50", 20),
    ];
    let resolved = resolve_field(
        &observations,
        &policy(ReconciliationMode::AuthorityOrdering),
    );
    match resolved {
        ResolvedField::Value { value, .. } => assert_eq!(value, "36.50"),
        other => panic!("expected a value, got {other:?}"),
    }
}

#[test]
fn authority_ordering_is_documented_and_total() {
    let ordered = [
        ObservationOrigin::OfficialApi,
        ObservationOrigin::HttpJson,
        ObservationOrigin::BrowserNetwork,
        ObservationOrigin::EmbeddedState,
        ObservationOrigin::StructuredMarkup,
        ObservationOrigin::Dom,
        ObservationOrigin::RenderedText,
    ];
    for window in ordered.windows(2) {
        assert!(
            window[0].authority_rank() > window[1].authority_rank(),
            "{:?} must outrank {:?}",
            window[0],
            window[1]
        );
    }
    let conflict = ResolvedField::Conflict {
        observations: vec![
            observation(ObservationOrigin::RenderedText, "1.00", 100),
            observation(ObservationOrigin::EmbeddedState, "2.00", 1),
        ],
    };
    assert_eq!(
        conflict.authoritative().expect("winner").origin,
        ObservationOrigin::EmbeddedState,
        "authority outranks freshness"
    );
}

#[test]
fn missing_and_over_bound_observations_are_typed() {
    let empty: Vec<Observation<String>> = Vec::new();
    let resolved = resolve_field(&empty, &policy(ReconciliationMode::RequireAgreement));
    assert!(resolved.is_missing());
    assert!(resolved.authoritative().is_none());

    let observations: Vec<Observation<String>> = (0..17)
        .map(|index| observation(ObservationOrigin::Dom, "1.00", index))
        .collect();
    let bounded = ReconciliationPolicy {
        mode: ReconciliationMode::RequireAgreement,
        max_observations: 16,
    };
    assert!(
        resolve_field(&observations, &bounded).is_conflict(),
        "an over-bound observation set is a conflict, never a silent truncation"
    );
}

#[test]
fn observation_serde_round_trips() {
    let observation = observation(ObservationOrigin::BrowserNetwork, "18.20", 42);
    let json = serde_json::to_string(&observation).expect("serialize");
    assert_eq!(
        json,
        r#"{"value":"18.20","origin":"browser_network","observed_at_ms":42}"#
    );
    assert_eq!(
        serde_json::from_str::<Observation<String>>(&json).expect("round trip"),
        observation
    );
    assert!(serde_json::from_str::<Observation<String>>(
        r#"{"value":"x","origin":"telepathy","observed_at_ms":1}"#
    )
    .is_err());
}

#[test]
fn schema_fingerprints_are_order_and_duplicate_insensitive() {
    let a = SchemaFingerprint::from_key_paths(&["data.price", "data.stock", "meta"]);
    let b = SchemaFingerprint::from_key_paths(&["meta", "data.stock", "data.price", "data.price"]);
    assert_eq!(a, b);
    assert_eq!(a.keys, 3);
    assert!(a.digest.starts_with("blake3:"));
    assert_eq!(a.digest.len(), 7 + 64);

    let drifted = SchemaFingerprint::from_key_paths(&["data.price", "data.stock"]);
    assert_ne!(a.digest, drifted.digest);
    assert_eq!(
        classify_schema_drift(Some(&a), Some(&b)),
        SchemaDrift::Match
    );
    assert_eq!(
        classify_schema_drift(Some(&a), Some(&drifted)),
        SchemaDrift::Drifted
    );
    assert_eq!(
        classify_schema_drift(Some(&a), None),
        SchemaDrift::NotComparable
    );
    assert_eq!(
        classify_schema_drift(None, None),
        SchemaDrift::NotComparable
    );

    let empty = SchemaFingerprint::from_key_paths(&[]);
    assert_eq!(empty.keys, 0);
}

#[test]
fn extraction_outcomes_are_typed_and_serializable() {
    let outcomes = vec![
        ExtractionOutcome::Success,
        ExtractionOutcome::NotApplicable,
        ExtractionOutcome::SchemaMismatch {
            expected: Some(SchemaFingerprint::from_key_paths(&["a"])),
            observed: SchemaFingerprint::from_key_paths(&["b"]),
        },
        ExtractionOutcome::Conflict {
            fields: vec![text("price")],
        },
        ExtractionOutcome::Failed {
            reason: text("truncated stream"),
        },
    ];
    let labels: Vec<&str> = outcomes.iter().map(ExtractionOutcome::label).collect();
    assert_eq!(
        labels,
        vec![
            "success",
            "not_applicable",
            "schema_mismatch",
            "conflict",
            "failed"
        ]
    );
    assert!(outcomes[0].is_success());
    assert!(!outcomes[1].is_success());
    for outcome in &outcomes {
        let json = serde_json::to_string(outcome).expect("serialize");
        let back: ExtractionOutcome = serde_json::from_str(&json).expect("round trip");
        assert_eq!(&back, outcome);
    }
}

fn all_source_errors() -> Vec<SourceError> {
    vec![
        SourceError::Disabled,
        SourceError::InvalidRequest,
        SourceError::AuthenticationRequired,
        SourceError::VerificationRequired {
            kind: VerificationKind::Captcha,
        },
        SourceError::RateLimited {
            retry_after_ms: 1_500,
        },
        SourceError::QuotaExhausted { reset_ms: 60_000 },
        SourceError::CoolingDown { until_ms: 30_000 },
        SourceError::EgressUnavailable,
        SourceError::NetworkTimeout,
        SourceError::ApiUnavailable,
        SourceError::BrowserUnavailable,
        SourceError::BrowserCrashed,
        SourceError::ProductNotFound,
        SourceError::VariantAmbiguous,
        SourceError::ExtractionIncomplete,
        SourceError::ExtractionConflict,
        SourceError::ResponseTooLarge,
        SourceError::Cancelled,
        SourceError::Deadline,
        SourceError::Store,
    ]
}

#[test]
fn source_error_round_trips_every_variant_with_typed_fields() {
    for error in all_source_errors() {
        let json = serde_json::to_string(&error).expect("serialize");
        let back: SourceError = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, error, "{json}");
    }
    let json = serde_json::to_value(SourceError::RateLimited {
        retry_after_ms: 1_500,
    })
    .expect("value");
    assert_eq!(json["error"], "rate_limited");
    assert_eq!(json["retry_after_ms"], 1_500);
    let json = serde_json::to_value(SourceError::VerificationRequired {
        kind: VerificationKind::Captcha,
    })
    .expect("value");
    assert_eq!(json["error"], "verification_required");
    assert_eq!(json["kind"], "captcha");
    let json = serde_json::to_value(SourceError::Disabled).expect("value");
    assert_eq!(json, serde_json::json!({"error": "disabled"}));
}

#[test]
fn source_error_hostile_documents_are_rejected() {
    for document in [
        r#"{"error":"teleported"}"#,
        r#"{"error":"rate_limited"}"#,
        r#"{"error":"rate_limited","retry_after_ms":-1}"#,
        r#"{"error":"rate_limited","retry_after_ms":"soon"}"#,
        r#"{"error":"verification_required","kind":"telepathy"}"#,
        r#"{"kind":"disabled"}"#,
        r#"{"error":5}"#,
        r#""disabled""#,
    ] {
        assert!(
            serde_json::from_str::<SourceError>(document).is_err(),
            "{document} must be rejected"
        );
    }
}

#[test]
fn retry_classes_match_the_contract() {
    for error in all_source_errors() {
        let class = error.retry_class();
        match &error {
            SourceError::NetworkTimeout
            | SourceError::ApiUnavailable
            | SourceError::BrowserCrashed
            | SourceError::EgressUnavailable
            | SourceError::BrowserUnavailable => {
                assert_eq!(class, SourceRetryClass::Transient);
                assert!(error.is_transient());
                assert!(!error.surfaces_immediately());
            }
            SourceError::RateLimited { .. } | SourceError::CoolingDown { .. } => {
                assert_eq!(class, SourceRetryClass::RateLimited);
                assert!(error.retry_after_ms().is_some());
            }
            SourceError::QuotaExhausted { .. } => {
                assert_eq!(class, SourceRetryClass::QuotaExhausted);
                assert!(error.surfaces_immediately());
            }
            SourceError::AuthenticationRequired | SourceError::VerificationRequired { .. } => {
                assert_eq!(class, SourceRetryClass::HumanRequired);
                assert!(error.surfaces_immediately());
            }
            SourceError::Cancelled | SourceError::Deadline => {
                assert_eq!(class, SourceRetryClass::Aborted);
            }
            _ => {
                assert_eq!(class, SourceRetryClass::Permanent);
                assert!(error.retry_after_ms().is_none());
            }
        }
    }
    assert_eq!(
        SourceError::RateLimited {
            retry_after_ms: 2_000
        }
        .retry_after_ms(),
        Some(2_000)
    );
}

#[test]
fn connector_health_round_trips_and_maps_errors() {
    let health = vec![
        ConnectorHealth::Healthy,
        ConnectorHealth::Degraded {
            reason: text("schema drift"),
        },
        ConnectorHealth::CoolingDown { until_ms: 1_000 },
        ConnectorHealth::RateLimited { until_ms: 2_000 },
        ConnectorHealth::AuthenticationRequired,
        ConnectorHealth::VerificationRequired {
            challenge: text("captcha"),
        },
        ConnectorHealth::QuotaExhausted { reset_ms: 3_000 },
        ConnectorHealth::Unavailable,
    ];
    for state in &health {
        let json = serde_json::to_string(state).expect("serialize");
        let back: ConnectorHealth = serde_json::from_str(&json).expect("round trip");
        assert_eq!(&back, state);
    }
    assert!(health[0].is_available());
    assert!(health[1].is_available());
    assert!(!health[2].is_available());
    assert_eq!(health[2].until_ms(), Some(1_000));
    assert_eq!(health[0].to_error(), None);
    assert_eq!(health[1].to_error(), None);
    assert_eq!(
        health[2].to_error(),
        Some(SourceError::CoolingDown { until_ms: 1_000 })
    );

    for error in all_source_errors() {
        let health = ConnectorHealth::from_error(&error);
        // Degraded connectors stay plannable; only Disabled/EgressUnavailable
        // and the human/quota states take a connector out of rotation.
        let expected_available = matches!(
            error,
            SourceError::InvalidRequest
                | SourceError::NetworkTimeout
                | SourceError::ApiUnavailable
                | SourceError::BrowserUnavailable
                | SourceError::BrowserCrashed
                | SourceError::ExtractionIncomplete
                | SourceError::ExtractionConflict
                | SourceError::ResponseTooLarge
                | SourceError::Store
                | SourceError::ProductNotFound
                | SourceError::VariantAmbiguous
                | SourceError::Cancelled
                | SourceError::Deadline
        );
        assert_eq!(health.is_available(), expected_available, "{error:?}");
        if !expected_available {
            assert!(
                health.to_error().is_some(),
                "{health:?} must map back to a failure"
            );
        }
    }
    assert_eq!(
        ConnectorHealth::from_error(&SourceError::QuotaExhausted { reset_ms: 9 }),
        ConnectorHealth::QuotaExhausted { reset_ms: 9 }
    );
    assert_eq!(
        ConnectorHealth::from_error(&SourceError::RateLimited { retry_after_ms: 7 }),
        ConnectorHealth::RateLimited { until_ms: 7 }
    );
    assert_eq!(
        SourceError::BrowserCrashed.into_health(),
        ConnectorHealth::Degraded {
            reason: text("browser_crashed")
        }
    );
    assert!(serde_json::from_str::<ConnectorHealth>(r#"{"state":"vibing"}"#).is_err());
    assert!(serde_json::from_str::<ConnectorHealth>(r#"{"state":"degraded"}"#).is_err());
}

#[test]
fn every_static_label_is_valid_bounded_text() {
    for label in SOURCE_ERROR_LABELS {
        assert!(
            Text::<256>::new(label).is_ok(),
            "{label:?} is not valid bounded text"
        );
    }
    for label in VERIFICATION_KIND_LABELS {
        assert!(Text::<256>::new(label).is_ok(), "{label:?}");
    }
    for error in all_source_errors() {
        assert!(
            SOURCE_ERROR_LABELS.contains(&error.as_str()),
            "{:?} is missing from the label table",
            error.as_str()
        );
        let health = ConnectorHealth::from_error(&error);
        if let ConnectorHealth::Degraded { reason } = health {
            assert!(Text::<256>::new(reason.as_str()).is_ok());
        }
    }
    assert_eq!(SOURCE_ERROR_LABELS.len(), all_source_errors().len());
}
