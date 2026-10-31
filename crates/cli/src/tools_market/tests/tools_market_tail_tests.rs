//! `tools_market_tail_tests`: the job-scope/dedup test, split out of
//! `tools_market` by the source-size ceiling (its `mod tests` fixtures are
//! imported explicitly; the parent helpers stay in `mod tests`).
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::*;

#[test]
fn job_status_and_dedup_are_scoped_to_the_caller_principal() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = CommerceCfg {
        enabled: true,
        ..CommerceCfg::default()
    };
    cfg.connectors.mouser = Some(api_connector("FAKTOR_TEST_MOUSER_KEY"));
    let service =
        CommerceSourceService::open(dir.path(), service_config(&cfg), artifacts_for(dir.path()))
            .unwrap();
    service
        .register(
            Arc::new(RecordingCommerceConnector {
                source: SourceId::new("mouser").expect("source"),
                state: Arc::new(QuoteRecordingState::default()),
                offer: fixture_offer_with_variant("mouser", "TPS5430DDAR"),
            }),
            faktor_commerce::connector::ConnectorPolicy::default(),
        )
        .expect("register");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bom_args = serde_json::json!({
        "op": "bom",
        "items": [{"q": "TPS5430DDAR", "qty": 10}],
        "sources": ["mouser"],
        "freshness": "live"
    });

    let first = rt
        .block_on(run_with_ctx(
            run_ctx_for(1, 1),
            service.clone(),
            bom_args.clone(),
        ))
        .expect("first job");
    let first: serde_json::Value = serde_json::from_str(&first.text).expect("json");
    let job_id = first["job_id"].as_str().expect("job id").to_string();

    // The owner reads its own job.
    let own = rt
        .block_on(run_with_ctx(
            run_ctx_for(1, 1),
            service.clone(),
            serde_json::json!({"op": "job", "job_id": job_id}),
        ))
        .expect("owner read");
    let own: serde_json::Value = serde_json::from_str(&own.text).expect("json");
    assert_eq!(own["job_id"].as_str(), Some(job_id.as_str()));

    // A different session in the same workspace: typed NotFound, exactly
    // like a missing job.
    let error = rt
        .block_on(run_with_ctx(
            run_ctx_for(2, 1),
            service.clone(),
            serde_json::json!({"op": "job", "job_id": job_id}),
        ))
        .expect_err("cross-session read");
    assert_eq!(error.kind, ErrorKind::NotFound);
    let error = rt
        .block_on(run_with_ctx(
            run_ctx_for(1, 2),
            service.clone(),
            serde_json::json!({"op": "job", "job_id": job_id}),
        ))
        .expect_err("cross-workspace read");
    assert_eq!(error.kind, ErrorKind::NotFound);

    // Dedup at the durable store: the SAME principal submitting the same
    // request attaches to the active job; another session/account never
    // does. (The running tool already advanced the job above, so submit
    // fresh requests without advancing.)
    let account_one = AccountScope::new("acct-one").expect("account scope");
    let acquire_one =
        faktor_commerce::connector::AcquireCtx::new().with_account(account_one.clone());
    let principal_one = principal_for(&run_ctx_for(1, 1), &acquire_one);
    let acquire_two = faktor_commerce::connector::AcquireCtx::new()
        .with_account(AccountScope::new("acct-two").expect("account scope"));
    let principal_other_account = principal_for(&run_ctx_for(1, 1), &acquire_two);
    let principal_other_session = principal_for(
        &run_ctx_for(2, 1),
        &faktor_commerce::connector::AcquireCtx::new().with_account(account_one.clone()),
    );
    let line = BomItem::new("TPS5430DDAR", 10).expect("item");
    let request = |account: Option<AccountScope>| {
        faktor_commerce::bom_request(
            Bom::new(vec![line.clone()]).expect("bom"),
            faktor_commerce::SourceSet::named(vec![SourceId::new("mouser").unwrap()])
                .expect("sources"),
            FreshnessMode::Live,
            DetailLevel::Compact,
            account,
        )
    };
    let store = service.store().expect("store");
    let now = faktor_commerce::service::now_ms();
    let sources = [SourceId::new("mouser").unwrap()];
    let (job_a, attached_a) = faktor_commerce::jobs::submit_job(
        store,
        request(Some(account_one.clone())),
        &sources,
        None,
        &principal_one,
        now,
    )
    .expect("submit a");
    assert!(!attached_a);
    let (job_b, attached_b) = faktor_commerce::jobs::submit_job(
        store,
        request(Some(account_one.clone())),
        &sources,
        None,
        &principal_one,
        now,
    )
    .expect("submit b");
    assert!(attached_b, "the same owner must attach");
    assert_eq!(job_a.id, job_b.id);
    let (job_c, attached_c) = faktor_commerce::jobs::submit_job(
        store,
        request(Some(AccountScope::new("acct-two").expect("scope"))),
        &sources,
        None,
        &principal_other_account,
        now,
    )
    .expect("submit c");
    assert!(!attached_c, "another account must never attach");
    assert_ne!(job_a.id, job_c.id);
    let (job_d, attached_d) = faktor_commerce::jobs::submit_job(
        store,
        request(Some(account_one.clone())),
        &sources,
        None,
        &principal_other_session,
        now,
    )
    .expect("submit d");
    assert!(!attached_d, "another session must never attach");
    assert_ne!(job_a.id, job_d.id);

    // Scoped reads at the service boundary: wrong owner is NotFound.
    service
        .job_status(&principal_one, &job_a.id)
        .expect("owner read");
    assert!(matches!(
        service.job_status(&principal_other_account, &job_a.id),
        Err(ServiceError::Source(SourceError::ProductNotFound))
    ));
    assert!(matches!(
        service.job_status(&principal_other_session, &job_a.id),
        Err(ServiceError::Source(SourceError::ProductNotFound))
    ));

    // A different session NEVER attaches to the active job: it mints its
    // own job identity.
    let other = rt
        .block_on(run_with_ctx(
            run_ctx_for(2, 1),
            service.clone(),
            bom_args.clone(),
        ))
        .expect("other session job");
    let other: serde_json::Value = serde_json::from_str(&other.text).expect("json");
    assert_ne!(
        other["job_id"].as_str(),
        Some(job_id.as_str()),
        "two sessions must never share one active job"
    );
    // And the other session can read ITS job, not the first one.
    assert!(rt
        .block_on(run_with_ctx(
            run_ctx_for(2, 1),
            service.clone(),
            serde_json::json!({"op": "job", "job_id": other["job_id"].as_str().unwrap()}),
        ))
        .is_ok());
}
