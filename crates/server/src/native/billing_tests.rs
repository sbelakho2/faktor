//! Adversarial route tests of the Wave 3 commercial metering surface:
//! billing-disabled parity, org isolation over HTTP, strict DTOs, the
//! admin-only idempotency-keyed credit grant, the derived entitlement
//! snapshot and the byte-for-byte fold-over-the-reservation-ledger check.

use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};
use faktor_cloud::{
    BillingConfig, EntitlementService, ManualClock, MemoryBillingStore, OrganizationId, PlanConfig,
    Subscription, SubscriptionStatus, CAUSE_SUBSCRIPTION_ACTIVE, FEATURE_MANAGED_PROVIDERS,
    LIMIT_MAX_ACTIVE_TASKS, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
};
use faktor_core::id::OpId;
use faktor_session::SessionManager;

const NOW_MS: i64 = 1_700_000_000_000;

fn billing_config() -> BillingConfig {
    let mut config = BillingConfig::default();
    config.plans.insert(
        "pro".into(),
        PlanConfig {
            period_ms: None,
            plan_id: "pro".into(),
            features: [FEATURE_MANAGED_PROVIDERS.to_string()]
                .into_iter()
                .collect(),
            limits: [
                (LIMIT_MAX_ACTIVE_TASKS.to_string(), 3),
                (
                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(),
                    5_000_000,
                ),
            ]
            .into_iter()
            .collect(),
        },
    );
    config.default_plan = Some("pro".into());
    config.managed_providers.insert("managed-provider".into());
    config
}

fn billing_service() -> Arc<EntitlementService> {
    EntitlementService::new(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        billing_config(),
    )
    .unwrap()
}

/// Read one money field off the wire: it MUST be a quoted decimal string.
fn money(value: &serde_json::Value) -> u64 {
    value
        .as_str()
        .unwrap_or_else(|| panic!("money field is not a decimal string: {value}"))
        .parse()
        .expect("money field parses as u64")
}

/// The response-shape invariant: every money-named key (`*_micro` /
/// `*Micro`), at any depth, is a decimal string or JSON null (an absent
/// optional cap); it is NEVER a JSON number. No other numeric field is
/// converted (ids/seqs/counts/tokens stay JSON numbers).
fn assert_money_encoding(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, entry) in map {
                if faktor_cloud::money::is_money_field(key) && !entry.is_null() {
                    let raw = entry.as_str().unwrap_or_else(|| {
                        panic!("money field {key:?} is emitted as a JSON number: {entry}")
                    });
                    raw.parse::<u64>()
                        .unwrap_or_else(|_| panic!("money field {key:?}={raw:?} is not decimal"));
                }
                assert_money_encoding(entry);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_money_encoding(item);
            }
        }
        _ => {}
    }
}

/// One served daemon with the cloud control plane AND the billing service
/// wired (the `[cloud]` + `[billing]` configuration).
async fn billing_deps(
    root: &std::path::Path,
) -> (
    ServerHandle,
    Arc<faktor_cloud::ControlPlane>,
    Arc<EntitlementService>,
    String,
    Arc<SessionManager>,
) {
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let billing = billing_service();
    let deps = test_deps(root)
        .with_control_plane(control_plane.clone())
        .with_billing(billing.clone());
    let token = deps.auth_token.clone();
    let session = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    (
        handle,
        control_plane,
        billing,
        token.as_str().to_string(),
        session,
    )
}

/// Provision the local billing account the route's grant/ingest paths use
/// (the route surface intentionally has no account-creation endpoint).
fn provision_account(
    billing: &EntitlementService,
    organization: &OrganizationId,
) -> faktor_cloud::BillingAccountId {
    let account = faktor_cloud::BillingAccountId::try_new("acct_local").unwrap();
    billing
        .ensure_account(organization, &account, "local", true)
        .unwrap();
    account
}

fn bootstrap(
    control_plane: &faktor_cloud::ControlPlane,
    name: &str,
    email: &str,
) -> (String, String) {
    let result = control_plane
        .bootstrap_organization(name, email, "Owner", &format!("boot-{email}"))
        .unwrap();
    let org = result.organization.id.as_str().to_string();
    let token = result
        .token
        .map(|t| t.expose().to_string())
        .expect("first issuance presents the token");
    (org, token)
}

#[tokio::test]
async fn billing_disabled_answers_typed_409_and_legacy_usage_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path()).with_control_plane(control_plane.clone());
    assert!(deps.billing.is_none(), "billing is disabled by default");
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "Acme", "owner@acme.test");

    // ORDERING: a missing/invalid control credential is the same typed 401 a
    // wired billing service would answer — never `billing_disabled`.
    for control in [None, Some("bogus-billing-token")] {
        let mut request = client
            .get(format!("{base}/native/entitlements"))
            .bearer_auth(token.as_str());
        if let Some(control) = control {
            request = request.header("x-faktor-control-token", control);
        }
        let resp = request.send().await.unwrap();
        assert_eq!(
            resp.status(),
            401,
            "control={control:?} must lose before billing_disabled"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    // Every billing surface answers the typed disabled refusal.
    let resp = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "billing_disabled");
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "billing_disabled"
    );
    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &control_token)
        .header("idempotency-key", "k1")
        .json(&serde_json::json!({"amount_micro": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "billing_disabled"
    );
    // The legacy usage route keeps its frozen shape without `org`.
    let resp = client
        .get(format!("{base}/native/usage"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["durable"].is_object(), "the pre-billing shape: {body}");
    assert!(body["totals"]["spent"].is_number());
}

#[tokio::test]
async fn usage_org_is_isolated_and_foreign_orgs_are_indistinguishable_from_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, _billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org_a, token_a) = bootstrap(&control_plane, "A", "a@a.test");
    let (org_b, _token_b) = bootstrap(&control_plane, "B", "b@b.test");
    // Own org: admitted (200).
    let resp = client
        .get(format!("{base}/native/usage?org={org_a}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["organization"].as_str(), Some(org_a.as_str()));
    // Foreign org: the byte-identical 404 a missing org answers.
    let foreign = client
        .get(format!("{base}/native/usage?org={org_b}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    let missing = client
        .get(format!("{base}/native/usage?org=org_missing"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 404);
    assert_eq!(missing.status(), 404);
    let foreign_body = foreign.text().await.unwrap();
    let missing_body = missing.text().await.unwrap();
    assert_eq!(
        foreign_body, missing_body,
        "a foreign organization must not be distinguishable"
    );
    assert!(foreign_body.contains("not_found"), "{foreign_body}");
    // No control-plane credential at all: unauthorized, not a guess.
    let resp = client
        .get(format!("{base}/native/usage?org={org_a}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // A strict query: an unknown key is a 400, never a silent legacy answer.
    let resp = client
        .get(format!("{base}/native/usage?org={org_a}&since=0&bogus=1"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn usage_fold_matches_the_reservation_ledger_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);

    // ---- the durable reservation/settlement ledger (the SOURCE)
    let ws = session.create_workspace("/w").unwrap();
    let sess = session
        .create_session(ws, "t", "managed-provider", "m")
        .unwrap();
    let task = sess
        .create_task(faktor_session::Task {
            task_id: sess.task_id().unwrap(),
            session_id: sess.id(),
            goal: "metered".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            contract: Default::default(),
            budget: Default::default(),
            state: faktor_core::state::TaskState::Running,
            created_ms: NOW_MS,
            updated_ms: NOW_MS,
        })
        .unwrap();
    let store = session.store();
    let mut expected_cost = 0u64;
    let mut expected_in = 0u64;
    let mut expected_out = 0u64;
    for (ordinal, (in_tokens, out_tokens, cost)) in
        [(10u64, 20u64, 111u64), (5, 7, 222), (0, 3, 333)]
            .into_iter()
            .enumerate()
    {
        let attempt = faktor_core::op::ModelCallAttempt::new(
            OpId::new(ordinal as u64 + 1),
            OpId::new(ordinal as u64 + 11),
            ordinal as u32 + 1,
        )
        .unwrap();
        let granted = store
            .cost_reserve_attempt(
                sess.id(),
                task.task_id,
                &attempt,
                cost + 1_000,
                NOW_MS,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .unwrap();
        let reservation = match granted {
            faktor_store::CostReserveOutcome::Granted(id) => id,
            other => panic!("reservation refused: {other:?}"),
        };
        assert!(matches!(
            store.cost_mark_dispatched(reservation, NOW_MS).unwrap(),
            faktor_store::CostReservationState::Applied
        ));
        // A dispatched attempt's durable provider-call row (the token/cost
        // provenance the projection joins by reservation id).
        store
            .record_provider_call_attempt(
                sess.id(),
                &attempt,
                Some(reservation),
                "managed-provider",
                "m",
                "completed",
                Some(in_tokens),
                Some(out_tokens),
                None,
            )
            .unwrap();
        assert!(matches!(
            store
                .cost_settle_usage(
                    reservation,
                    in_tokens,
                    0,
                    0,
                    out_tokens,
                    Some(cost),
                    None,
                    NOW_MS,
                )
                .unwrap(),
            faktor_store::CostSettleOutcome::Applied { .. }
        ));
        expected_cost += cost;
        expected_in += in_tokens;
        expected_out += out_tokens;
    }
    // A reservation that was refunded pre-dispatch must NOT be projected.
    let refund_attempt =
        faktor_core::op::ModelCallAttempt::new(OpId::new(50), OpId::new(51), 9).unwrap();
    let refundable = match store
        .cost_reserve_attempt(
            sess.id(),
            task.task_id,
            &refund_attempt,
            777,
            NOW_MS,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .unwrap()
    {
        faktor_store::CostReserveOutcome::Granted(id) => id,
        other => panic!("reservation refused: {other:?}"),
    };
    assert!(matches!(
        store.cost_refund(refundable, NOW_MS).unwrap(),
        faktor_store::RefundOutcome::Applied
    ));
    // The reservation ledger's own folded picture (the SOURCE the projection
    // must match exactly).
    let ledger_spend = store
        .cost_task_row(sess.id(), task.task_id)
        .unwrap()
        .unwrap()
        .spent_cost_micro;
    assert_eq!(
        ledger_spend, expected_cost,
        "the source ledger folds the sum"
    );

    // ---- the route folds the SAME numbers
    let resp = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "usage route body: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    let fold = &body["fold"];
    assert_money_encoding(&body);
    assert_eq!(
        money(&fold["totals"]["provider_cost_micro"]),
        expected_cost,
        "the fold matches the reservation ledger byte for byte: {body}"
    );
    assert_eq!(money(&fold["totals"]["managed_cost_micro"]), expected_cost);
    assert_eq!(money(&fold["totals"]["byok_cost_micro"]), 0);
    assert_eq!(fold["totals"]["input_tokens"].as_u64(), Some(expected_in));
    assert_eq!(fold["totals"]["output_tokens"].as_u64(), Some(expected_out));
    assert_eq!(
        fold["totals"]["events"].as_u64(),
        Some(8),
        "two token rows + one cost row per settled reservation, zeros skipped"
    );
    let per_task = fold["per_task"].as_array().unwrap();
    assert_eq!(per_task.len(), 1);
    assert_eq!(per_task[0]["task_id"].as_u64(), Some(task.task_id.raw()));
    assert_eq!(
        money(&per_task[0]["totals"]["provider_cost_micro"]),
        expected_cost
    );
    assert_eq!(
        per_task[0]["totals"]["input_tokens"].as_u64(),
        Some(expected_in)
    );
    // The per-session view follows the same encoding: camelCase money fields
    // are decimal strings too, and the route emits no money number anywhere.
    let resp = client
        .get(format!("{base}/native/session/{}/usage", sess.id()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let usage: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&usage);
    assert_eq!(
        money(&usage["tasks"][0]["budget"]["spentCostMicro"]),
        expected_cost
    );
    assert_eq!(
        money(&usage["tasks"][0]["reservations"]["settled"]["spentMicro"]),
        expected_cost
    );
    assert!(
        usage["tasks"][0]["budget"]["spentTokens"].is_number(),
        "token counters stay JSON numbers"
    );
    // Re-reading is idempotent (a re-projection appends nothing).
    let resp = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body2: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body2["fold"], body["fold"]);
    assert_eq!(
        body2["items"].as_array().unwrap().len(),
        body["items"].as_array().unwrap().len()
    );
}

#[tokio::test]
async fn credits_grant_is_admin_only_idempotency_keyed_and_strict() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);
    // This test service has NO configured billing organization, so the
    // configured default plan applies to NOBODY: the snapshot is unentitled.
    // (The configured-organization case is covered by
    // `foreign_tenant_gets_an_unentitled_snapshot_not_the_configured_default_plan`.)
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let unentitled: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        unentitled["entitlements"]["plan_id"],
        serde_json::Value::Null
    );
    assert_eq!(unentitled["entitlements"]["plan_found"], false);
    assert!(unentitled["entitlements"]["limits"]
        .as_object()
        .unwrap()
        .is_empty());
    // An active subscription ALWAYS wins, regardless of organization: set
    // it for the rest of the test (it is replaced by the expired one below).
    billing
        .set_subscription(&Subscription {
            id: faktor_cloud::SubscriptionId::try_new("sub_active").unwrap(),
            organization: organization.clone(),
            plan_id: "pro".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: NOW_MS,
        })
        .unwrap();
    // A member-scoped service account: role Member + no billing scope.
    let owner_principal = control_plane.authenticate(&owner_token).unwrap();
    let member_token = control_plane
        .create_service_account(
            &owner_principal,
            &organization,
            "ci",
            faktor_cloud::Role::Member,
            vec![faktor_cloud::Action::RepositoryRead],
        )
        .unwrap()
        .token
        .expect("issued once")
        .expose()
        .to_string();
    // Missing Idempotency-Key: a strict 400.
    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .json(&serde_json::json!({"amount_micro": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // A member (admin not reached) is denied.
    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &member_token)
        .header("idempotency-key", "k-member")
        .json(&serde_json::json!({"amount_micro": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    // A strict body: unknown fields are refused.
    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-unknown")
        .json(&serde_json::json!({"amount_micro": 100, "price": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // The owner (admin and above) grants; the replay is idempotent.
    let body = serde_json::json!({"amount_micro": 250_000, "reason": "seed"});
    let first = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-grant-1")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first_json: serde_json::Value = first.json().await.unwrap();
    assert_money_encoding(&first_json);
    assert_eq!(first_json["duplicate"], false);
    assert_eq!(money(&first_json["credits"]["granted_micro"]), 250_000);
    let replay = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-grant-1")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    let replay_json: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(replay_json["duplicate"], true);
    assert_eq!(
        money(&replay_json["credits"]["granted_micro"]),
        250_000,
        "a replay never double-grants"
    );
    // The entitlements snapshot reflects the grant and the configured plan.
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let snapshot: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&snapshot);
    assert_eq!(snapshot["entitlements"]["plan_id"], "pro");
    assert_eq!(snapshot["entitlements"]["plan_found"], true);
    assert_eq!(
        snapshot["entitlements"]["limits"][LIMIT_MAX_ACTIVE_TASKS],
        3
    );
    assert_eq!(
        money(&snapshot["entitlements"]["limits"][LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD]),
        5_000_000,
        "a money-named limit is a decimal string"
    );
    assert_eq!(
        money(&snapshot["entitlements"]["credits"]["granted_micro"]),
        250_000
    );
    // An expired subscription mid-transaction: the continuation is admitted,
    // the NEXT new-task admission is denied naming the exact cause.
    billing
        .set_subscription(&Subscription {
            id: faktor_cloud::SubscriptionId::try_new("sub_x").unwrap(),
            organization: organization.clone(),
            plan_id: "pro".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: Some(NOW_MS - 1),
            updated_ms: NOW_MS,
        })
        .unwrap();
    let in_flight = billing
        .begin_in_flight(&organization, faktor_cloud::InFlightKind::Rollback, "run-x")
        .unwrap();
    assert_eq!(
        billing
            .check_admission(
                &organization,
                &faktor_cloud::AdmissionRequest::boundary(
                    faktor_cloud::AdmissionBoundary::RollbackContinuation
                )
            )
            .unwrap(),
        faktor_cloud::Admission::InFlightContinuation,
        "the in-flight rollback continues"
    );
    assert!(billing
        .finish_in_flight(&organization, &in_flight.id)
        .unwrap());
    let denied = billing
        .check_admission(
            &organization,
            &faktor_cloud::AdmissionRequest::boundary(faktor_cloud::AdmissionBoundary::NewTask),
        )
        .unwrap_err();
    assert_eq!(denied.limit, CAUSE_SUBSCRIPTION_ACTIVE);
}

/// The money encoding over the real routes: every money field is a quoted
/// decimal string at the 2^53 precision boundary and at the i64::MAX domain
/// bound; legacy JSON numbers are still accepted on input; malformed,
/// negative and overflowing inputs are typed 400s; and non-money numbers
/// (counts, tokens, ids) stay JSON numbers.
#[tokio::test]
async fn money_fields_are_decimal_strings_and_legacy_numbers_still_decode() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);

    let grant = |body: serde_json::Value, key: &'static str| {
        let client = client.clone();
        let base = base.clone();
        let token = token.clone();
        let owner_token = owner_token.clone();
        async move {
            client
                .post(format!("{base}/native/credits/grant"))
                .bearer_auth(token.as_str())
                .header("x-faktor-control-token", &owner_token)
                .header("idempotency-key", key)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    // Every boundary value is served as a quoted decimal string: the minimum
    // (zero is refused by the credit domain, so the smallest grant is 1), the
    // 2^53-1/2^53 precision boundary and the i64::MAX domain bound.
    let big = 1u64 << 53;
    let boundary = (1u64 << 53) - 1;
    let max = i64::MAX as u64;
    let cases = [
        (serde_json::json!("1"), 1u64),
        (serde_json::json!(boundary), boundary),
        (serde_json::json!(big.to_string()), big),
        (serde_json::json!(max.to_string()), max),
    ];
    let mut total = 0u64;
    for (index, (body, amount)) in cases.into_iter().enumerate() {
        total = total.checked_add(amount).unwrap();
        let resp = grant(
            serde_json::json!({"amount_micro": body, "reason": "boundary"}),
            match index {
                0 => "k-min",
                1 => "k-2p53m1",
                2 => "k-2p53",
                _ => "k-max",
            },
        )
        .await;
        assert_eq!(resp.status(), 200, "case {index}");
        let value: serde_json::Value = resp.json().await.unwrap();
        assert_money_encoding(&value);
        assert_eq!(
            money(&value["credits"]["granted_micro"]),
            total,
            "the running grant total is exact at case {index}"
        );
        assert_eq!(
            value["credits"]["granted_micro"].as_str().unwrap(),
            total.to_string(),
            "the wire form is the exact decimal string"
        );
    }

    // Malformed, negative and overflowing inputs are typed 400s; nothing is
    // written (the balance above is unchanged).
    for (index, bad) in [
        serde_json::json!("abc"),
        serde_json::json!(""),
        serde_json::json!("-1"),
        serde_json::json!("+1"),
        serde_json::json!("1.5"),
        serde_json::json!(" 1"),
        serde_json::json!("1e3"),
        serde_json::json!("18446744073709551616"),
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!(true),
        serde_json::json!(null),
    ]
    .into_iter()
    .enumerate()
    {
        let resp = grant(
            serde_json::json!({"amount_micro": bad, "reason": "hostile"}),
            match index {
                0 => "bad-0",
                1 => "bad-1",
                2 => "bad-2",
                3 => "bad-3",
                4 => "bad-4",
                5 => "bad-5",
                6 => "bad-6",
                7 => "bad-7",
                8 => "bad-8",
                9 => "bad-9",
                10 => "bad-10",
                _ => "bad-11",
            },
        )
        .await;
        assert_eq!(resp.status(), 400, "hostile amount {index} must be a 400");
    }
    // Above the durable domain bound (i64::MAX) is refused even though it
    // parses as a u64.
    let resp = grant(
        serde_json::json!({"amount_micro": "9223372036854775808"}),
        "over-domain",
    )
    .await;
    assert_eq!(resp.status(), 400);

    // The derived snapshot and the usage fold carry the same encoding, with
    // token/count numbers untouched.
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let snapshot: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&snapshot);
    assert_eq!(
        money(&snapshot["entitlements"]["credits"]["granted_micro"]),
        total
    );
    assert!(
        snapshot["entitlements"]["total_tokens"].is_number(),
        "token counters stay JSON numbers"
    );
    let resp = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let usage: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&usage);
    assert_eq!(
        money(&usage["credits"]["granted_micro"]),
        total,
        "the usage route folds the same exact grants"
    );
    assert!(
        usage["fold"]["totals"]["events"].is_number(),
        "event counts stay JSON numbers"
    );
}

/// Adversarial: an authoritative usage fold that leaves the `u64` domain is
/// surfaced as the typed `ledger_refused` 500 on the real routes — never a
/// saturated total served as a readable fold, and never flattened into a
/// generic `internal`.
#[tokio::test]
async fn usage_overflow_is_a_typed_ledger_refusal_on_the_routes() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    let account = provision_account(&billing, &organization);
    let max = i64::MAX as u64;
    for index in 0..3u64 {
        let event = faktor_cloud::UsageEvent {
            id: faktor_cloud::UsageEventId::try_new(format!("uev_of{index}")).unwrap(),
            organization_id: organization.clone(),
            billing_account_id: account.clone(),
            task_id: 7,
            run_id: "1".into(),
            attempt_id: "a".into(),
            provider: "managed-provider".into(),
            model: "m".into(),
            unit: faktor_cloud::UsageUnit::ProviderCostMicro,
            quantity: 1,
            provider_cost_micro: max,
            source_operation: "session.cost_reservation.settled".into(),
            occurred_at_ms: NOW_MS,
            reconciliation_state: faktor_cloud::ReconciliationState::Reconciled,
            correction_of: None,
            category: faktor_cloud::SpendCategory::Managed,
            source_key: format!("manual:overflow:{index}"),
        };
        billing.record_usage(&event).unwrap();
    }
    for path in [
        format!("{base}/native/usage?org={org}"),
        format!("{base}/native/entitlements"),
    ] {
        let resp = client
            .get(&path)
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", &owner_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500, "{path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "ledger_refused", "{path}: {body}");
        assert_eq!(body["error"]["retryable"], false, "{path}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("provider_cost_micro"), "{path}: {message}");
        assert!(
            !message.contains("18446744073709551615"),
            "no saturated total is ever reported: {message}"
        );
    }
}

/// Adversarial route test of the default-plan tenant isolation: with
/// `[billing] organization = A`, org A's token resolves the configured
/// default plan, while a foreign/unprovisioned tenant's token gets an
/// UNENTITLED snapshot (`plan_found: false`, no plan id, empty
/// features/limits) — the configured tenant's plan is never handed down. A
/// subscription always wins, regardless of organization.
#[tokio::test]
async fn foreign_tenant_gets_an_unentitled_snapshot_not_the_configured_default_plan() {
    let dir = tempfile::tempdir().unwrap();
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let (org_a, token_a) = bootstrap(&control_plane, "A", "a@a.test");
    let (org_b, token_b) = bootstrap(&control_plane, "B", "b@b.test");
    // The configured billing organization is A; B exists but is not it.
    let billing = EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        billing_config(),
        Some(OrganizationId::try_new(org_a.clone()).unwrap()),
    )
    .unwrap();
    let deps = test_deps(dir.path())
        .with_control_plane(control_plane)
        .with_billing(billing.clone());
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // The configured organization's token resolves the default plan.
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let configured: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(configured["entitlements"]["plan_id"], "pro");
    assert_eq!(configured["entitlements"]["plan_found"], true);
    assert!(!configured["entitlements"]["limits"]
        .as_object()
        .unwrap()
        .is_empty());

    // The foreign tenant's token: 200 with an unentitled snapshot, never the
    // configured plan.
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &token_b)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&body);
    assert_eq!(body["entitlements"]["organization_id"], org_b.as_str());
    assert_eq!(body["entitlements"]["plan_id"], serde_json::Value::Null);
    assert_eq!(body["entitlements"]["plan_found"], false);
    assert!(
        body["entitlements"]["features"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no inherited features: {body}"
    );
    assert!(
        body["entitlements"]["limits"]
            .as_object()
            .unwrap()
            .is_empty(),
        "no inherited limits: {body}"
    );
    assert_eq!(body["entitlements"]["subscription_active"], false);

    // A subscription ALWAYS wins: B's own active subscription resolves its
    // plan, still without inheriting anything from the configured tenant.
    let organization_b = OrganizationId::try_new(org_b.clone()).unwrap();
    billing
        .set_subscription(&Subscription {
            id: faktor_cloud::SubscriptionId::try_new("sub_b").unwrap(),
            organization: organization_b,
            plan_id: "pro".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: NOW_MS,
        })
        .unwrap();
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &token_b)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["entitlements"]["plan_id"], "pro");
    assert_eq!(body["entitlements"]["plan_found"], true);
    assert_eq!(body["entitlements"]["limits"][LIMIT_MAX_ACTIVE_TASKS], 3);
}

/// Tenant isolation is decided BEFORE plan/billing availability: with the
/// control plane wired but `[billing]` disabled, a foreign `?org=` is the
/// same typed 404 a missing organization answers, never the
/// `billing_disabled` oracle that would reveal the caller's own tenancy.
#[tokio::test]
async fn outcomes_foreign_organization_is_404_before_billing_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path()).with_control_plane(control_plane);
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let bootstrap = |name: &'static str, email: &'static str, key: &'static str| {
        let client = client.clone();
        let base = base.clone();
        let token = token.as_str().to_string();
        async move {
            let response = client
                .post(format!("{base}/native/orgs"))
                .bearer_auth(&token)
                .header("idempotency-key", key)
                .json(&serde_json::json!({
                    "name": name,
                    "owner_email": email,
                    "display_name": name,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{name} bootstrap");
            let body: serde_json::Value = response.json().await.unwrap();
            (
                body["organization"]["id"].as_str().unwrap().to_string(),
                body["token"].as_str().unwrap().to_string(),
            )
        }
    };
    let (org_a, token_a) = bootstrap("Outcome A", "a@outcome.test", "outcome-a").await;
    let (org_b, _token_b) = bootstrap("Outcome B", "b@outcome.test", "outcome-b").await;

    let foreign = client
        .get(format!("{base}/native/outcomes?org={org_b}"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(
        foreign.status(),
        404,
        "a foreign organization must not reveal billing availability"
    );
    let body: serde_json::Value = foreign.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
    assert!(!body.to_string().contains(&org_b));

    let own = client
        .get(format!("{base}/native/outcomes?org={org_a}"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(
        own.status(),
        409,
        "the caller's own organization may learn that billing is disabled"
    );
    let body: serde_json::Value = own.json().await.unwrap();
    assert_eq!(body["error"]["code"], "billing_disabled");
    let _ = handle.request_shutdown();
}

/// The commercial account overview and the bounded usage download: typed
/// availability, tenant isolation, strict queries and attachment headers.
#[tokio::test]
async fn billing_overview_and_usage_export_are_typed_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    let account = provision_account(&billing, &organization);
    let event = faktor_cloud::UsageEvent {
        id: faktor_cloud::UsageEventId::try_new("uev_export_1").unwrap(),
        organization_id: organization.clone(),
        billing_account_id: account,
        task_id: 7,
        run_id: "1".into(),
        attempt_id: "a".into(),
        provider: "managed-provider".into(),
        model: "m".into(),
        unit: faktor_cloud::UsageUnit::InputTokens,
        quantity: 12,
        provider_cost_micro: 0,
        source_operation: "session.cost_reservation.settled".into(),
        occurred_at_ms: NOW_MS,
        reconciliation_state: faktor_cloud::ReconciliationState::Reconciled,
        correction_of: None,
        category: faktor_cloud::SpendCategory::Managed,
        source_key: "manual:export:1".into(),
    };
    billing.record_usage(&event).unwrap();
    let mut second = event.clone();
    second.id = faktor_cloud::UsageEventId::try_new("uev_export_2").unwrap();
    second.source_key = "manual:export:2".into();
    billing.record_usage(&second).unwrap();

    let resp = client
        .get(format!("{base}/native/billing/overview"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["billing"]["payment"]["state"], "unavailable");
    assert_eq!(
        body["billing"]["payment"]["cause"],
        faktor_cloud::CAUSE_PAYMENT_PROVIDER_NOT_CONFIGURED
    );
    assert_eq!(body["billing"]["invoices"]["state"], "unavailable");
    assert_eq!(body["billing"]["seats"]["state"], "unavailable");
    assert_eq!(body["billing"]["tax"]["state"], "unavailable");
    assert_eq!(body["billing"]["currency"]["state"], "unavailable");
    assert_eq!(body["billing"]["usage_export"]["state"], "available");
    assert_eq!(
        body["billing"]["usage_export_formats"],
        serde_json::json!(["csv", "json"])
    );

    let resp = client
        .get(format!("{base}/native/usage/export?org={org}&format=csv"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/csv; charset=utf-8"
    );
    assert!(resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("attachment"));
    let csv = resp.text().await.unwrap();
    assert!(csv.starts_with("cursor,event_id,"), "{csv}");
    assert!(csv.contains("uev_export_1"), "{csv}");

    let resp = client
        .get(format!(
            "{base}/native/usage/export?org={org}&format=json&limit=1"
        ))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["truncated"], true);

    // Strict query and tenant isolation.
    for query in ["format=xml", "limit=0", "bogus=1", "limit=5001"] {
        let resp = client
            .get(format!("{base}/native/usage/export?org={org}&{query}"))
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", &owner_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "query {query}");
    }
    let resp = client
        .get(format!("{base}/native/usage/export?org=org_foreign"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Plan change/cancel is an owner-only durable effect: the entitlement
/// snapshot changes immediately, replay is idempotent, and proration is
/// reported typed unavailable (plans carry no price).
#[tokio::test]
async fn subscription_change_is_owner_only_idempotent_and_effective() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);
    billing
        .set_subscription(&Subscription {
            id: faktor_cloud::SubscriptionId::try_new("sub_change").unwrap(),
            organization: organization.clone(),
            plan_id: "pro".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: NOW_MS,
        })
        .unwrap();
    let owner_principal = control_plane.authenticate(&owner_token).unwrap();
    let admin_token = control_plane
        .create_service_account(
            &owner_principal,
            &organization,
            "admin",
            faktor_cloud::Role::Admin,
            vec![faktor_cloud::Action::BillingWrite],
        )
        .unwrap()
        .token
        .expect("issued once")
        .expose()
        .to_string();

    // Admin is not enough: the change is owner-only.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &admin_token)
        .header("idempotency-key", "k-admin")
        .json(&serde_json::json!({"action": "change", "plan_id": "pro"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Missing idempotency key and strict body.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .json(&serde_json::json!({"action": "change", "plan_id": "pro"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-strict")
        .json(&serde_json::json!({"action": "change", "plan_id": "pro", "price": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // A trial requires an expiry; a valid trial applies immediately.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-trial-no-expiry")
        .json(&serde_json::json!({"action": "change", "plan_id": "pro", "status": "trialing"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let trial_body = serde_json::json!({
        "action": "change",
        "plan_id": "pro",
        "status": "trialing",
        "expires_ms": NOW_MS + 1_000,
    });
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-trial")
        .json(&trial_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["replayed"], false);
    assert_eq!(body["subscription"]["status"], "trialing");
    assert_eq!(body["proration"]["state"], "unavailable");
    assert_eq!(
        body["proration"]["cause"],
        faktor_cloud::CAUSE_PLAN_PRICES_NOT_CONFIGURED
    );
    assert_eq!(body["billing"]["trial"]["state"], "active");
    assert_eq!(body["billing"]["subscription_active"], true);

    // The replay is idempotent.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-trial")
        .json(&trial_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let replay_body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(replay_body["replayed"], true, "{replay_body}");

    // The derived snapshot reflects the trial immediately.
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["entitlements"]["subscription_status"], "trialing");
    assert_eq!(body["entitlements"]["subscription_active"], true);

    // Cancellation requires an explicit confirmation and is effective.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-cancel-no-confirm")
        .json(&serde_json::json!({"action": "cancel"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-cancel")
        .json(&serde_json::json!({"action": "cancel", "confirm": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["subscription"]["status"], "canceled");
    assert_eq!(body["billing"]["subscription_active"], false);
    assert_eq!(body["billing"]["trial"]["state"], "none");

    // Unknown plans are refused typed.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-unknown-plan")
        .json(&serde_json::json!({"action": "change", "plan_id": "absent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "malformed");
}

/// Adversarial: role authorization precedes BOTH request-shape validation and
/// the billing feature state. An underprivileged principal gets 403 for a
/// malformed body/query (never 400), never learns that billing is disabled
/// (never 409), and triggers no ingestion.
#[tokio::test]
async fn commercial_authorization_precedes_request_shape_and_feature_state() {
    let dir = tempfile::tempdir().unwrap();
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path()).with_control_plane(control_plane.clone());
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    let owner = control_plane.authenticate(&owner_token).unwrap();
    let viewer = control_plane
        .create_service_account(
            &owner,
            &organization,
            "viewer",
            faktor_cloud::Role::Viewer,
            vec![faktor_cloud::Action::RepositoryRead],
        )
        .unwrap()
        .token
        .expect("issued once")
        .expose()
        .to_string();

    // Malformed grant body: the viewer is refused on the ROLE, not the DTO.
    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &viewer)
        .header("idempotency-key", "viewer-grant")
        .json(&serde_json::json!({"amount_micro": "not-money"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "permission_denied"
    );
    // Malformed subscription body: same role-first refusal.
    let resp = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &viewer)
        .header("idempotency-key", "viewer-sub")
        .json(&serde_json::json!({"action": 42}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    // Malformed usage limit and export format: role-first, not 400.
    for path in [
        format!("/native/usage?org={org}&limit=0"),
        format!("/native/usage/export?org={org}&format=bogus"),
    ] {
        let resp = client
            .get(format!("{base}{path}"))
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", &viewer)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{path} must be refused on the role");
    }
    // Entitlements: 403, never the billing_disabled oracle.
    let resp = client
        .get(format!("{base}/native/entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &viewer)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    // Control: the owner (authorized) does learn that billing is disabled.
    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "owner-grant")
        .json(&serde_json::json!({"amount_micro": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "billing_disabled"
    );
}

/// Adversarial: a commercial idempotency key is request-bound. Reusing it for
/// a DIFFERENT change is a typed 409 with the durable state unchanged, and a
/// malformed/undefined-plan request never burns a key (so the corrected retry
/// is not falsely replayed).
#[tokio::test]
async fn subscription_idempotency_key_is_request_bound_and_never_burned_by_validation() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);
    let post = |key: &'static str, body: serde_json::Value| {
        client
            .post(format!("{base}/native/billing/subscription"))
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", &owner_token)
            .header("idempotency-key", key)
            .json(&body)
    };
    let resp = post(
        "k-bound",
        serde_json::json!({"action": "change", "plan_id": "pro"}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    // Same key, different request: 409, nothing applied.
    let resp = post(
        "k-bound",
        serde_json::json!({"action": "change", "plan_id": "pro", "status": "expired"}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "idempotency_conflict"
    );
    assert_eq!(
        billing
            .entitlement_snapshot(&organization)
            .unwrap()
            .subscription_status,
        Some(SubscriptionStatus::Active),
        "the conflicting request never mutated the subscription"
    );
    // An undefined plan is validated BEFORE the key is claimed: the
    // corrected retry under the same key is accepted, not replayed/conflicted.
    let resp = post(
        "k-retry",
        serde_json::json!({"action": "change", "plan_id": "absent"}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = post(
        "k-retry",
        serde_json::json!({"action": "change", "plan_id": "pro", "status": "expired"}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the failed validation must not burn the key"
    );
    assert_eq!(
        billing
            .entitlement_snapshot(&organization)
            .unwrap()
            .subscription_status,
        Some(SubscriptionStatus::Expired)
    );
}

/// Adversarial: a usage export is a CSV download, so formula-shaped provider/
/// model text must be neutralized (`'=` prefix), while RFC 4180 quoting still
/// applies. A hostile model recorded in the durable ledger can never execute
/// when the export is opened in a spreadsheet.
#[tokio::test]
async fn usage_export_neutralizes_spreadsheet_formula_injection() {
    // Pure field rules first.
    assert_eq!(super::csv_field("=1+1"), "'=1+1");
    assert_eq!(super::csv_field("+SUM(A1)"), "'+SUM(A1)");
    assert_eq!(super::csv_field("-1"), "'-1");
    assert_eq!(super::csv_field("@cmd"), "'@cmd");
    assert_eq!(super::csv_field("a,b"), "\"a,b\"");
    assert_eq!(super::csv_field("\"q\""), "\"\"\"q\"\"\"");
    assert_eq!(super::csv_field("plain"), "plain");

    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    let account = provision_account(&billing, &organization);
    billing
        .record_usage(&faktor_cloud::UsageEvent {
            id: faktor_cloud::UsageEventId::try_new("uev_csv").unwrap(),
            organization_id: organization,
            billing_account_id: account,
            task_id: 7,
            run_id: "1".into(),
            attempt_id: "a".into(),
            provider: "managed-provider".into(),
            model: "=1+1".into(),
            unit: faktor_cloud::UsageUnit::InputTokens,
            quantity: 1,
            provider_cost_micro: 0,
            source_operation: "manual".into(),
            occurred_at_ms: NOW_MS,
            reconciliation_state: faktor_cloud::ReconciliationState::Reconciled,
            correction_of: None,
            category: faktor_cloud::SpendCategory::Managed,
            source_key: "manual:csv:1".into(),
        })
        .unwrap();
    let resp = client
        .get(format!("{base}/native/usage/export?org={org}&format=csv"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let csv = resp.text().await.unwrap();
    assert!(csv.contains("'=1+1"), "formula neutralized: {csv}");
    assert!(!csv.contains(",=1+1,"), "raw formula never emitted: {csv}");
}

/// Adversarial: read-through ingestion is scoped to the CONFIGURED billing
/// organization. A second, fully provisioned tenant reading `/native/usage`
/// never claims the daemon's local session spend (cross-tenant attribution),
/// while the configured tenant still folds it exactly.
#[tokio::test]
async fn read_through_ingestion_never_attributes_local_sessions_to_a_foreign_org() {
    let dir = tempfile::tempdir().unwrap();
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let (org_a, token_a) = bootstrap(&control_plane, "A", "a@a.test");
    let (org_b, token_b) = bootstrap(&control_plane, "B", "b@b.test");
    let organization_a = OrganizationId::try_new(org_a.clone()).unwrap();
    let organization_b = OrganizationId::try_new(org_b.clone()).unwrap();
    let billing = EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        billing_config(),
        Some(organization_a.clone()),
    )
    .unwrap();
    provision_account(&billing, &organization_a);
    billing
        .ensure_account(
            &organization_b,
            &faktor_cloud::BillingAccountId::try_new("acct_b").unwrap(),
            "b",
            true,
        )
        .unwrap();
    let deps = test_deps(dir.path())
        .with_control_plane(control_plane)
        .with_billing(billing.clone());
    let token = deps.auth_token.clone();
    let session = deps.session.clone();
    // One settled local reservation = real spend the configured tenant owns.
    let ws = session.create_workspace("/w").unwrap();
    let sess = session
        .create_session(ws, "t", "managed-provider", "m")
        .unwrap();
    let task = sess
        .create_task(faktor_session::Task {
            task_id: sess.task_id().unwrap(),
            session_id: sess.id(),
            goal: "metered".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            contract: Default::default(),
            budget: Default::default(),
            state: faktor_core::state::TaskState::Running,
            created_ms: NOW_MS,
            updated_ms: NOW_MS,
        })
        .unwrap();
    let store = session.store();
    let attempt = faktor_core::op::ModelCallAttempt::new(OpId::new(1), OpId::new(2), 1).unwrap();
    let reservation = match store
        .cost_reserve_attempt(
            sess.id(),
            task.task_id,
            &attempt,
            1_000,
            NOW_MS,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .unwrap()
    {
        faktor_store::CostReserveOutcome::Granted(id) => id,
        other => panic!("reservation refused: {other:?}"),
    };
    assert!(matches!(
        store.cost_mark_dispatched(reservation, NOW_MS).unwrap(),
        faktor_store::CostReservationState::Applied
    ));
    store
        .record_provider_call_attempt(
            sess.id(),
            &attempt,
            Some(reservation),
            "managed-provider",
            "m",
            "completed",
            Some(10),
            Some(20),
            None,
        )
        .unwrap();
    assert!(matches!(
        store
            .cost_settle_usage(reservation, 10, 0, 0, 20, Some(321), None, NOW_MS)
            .unwrap(),
        faktor_store::CostSettleOutcome::Applied { .. }
    ));
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let usage = |org: &str, control: &str| {
        client
            .get(format!("{base}/native/usage?org={org}"))
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", control.to_string())
    };
    let resp = usage(&org_a, &token_a).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(money(&body["fold"]["totals"]["provider_cost_micro"]), 321);
    let resp = usage(&org_b, &token_b).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["fold"]["totals"]["events"].as_u64(),
        Some(0),
        "the foreign tenant never ingests the configured tenant's sessions: {body}"
    );
    assert!(
        billing
            .store()
            .usage_events(&organization_b, 0, 10)
            .unwrap()
            .is_empty(),
        "no cross-tenant rows were written"
    );
    assert!(!billing
        .store()
        .usage_events(&organization_a, 0, 10)
        .unwrap()
        .is_empty());
}

/// Adversarial: an ingestion window past the bound is a typed refusal BEFORE
/// any write; the ledger stays byte-identical (no silently under-reported
/// commercial prefix).
#[tokio::test]
async fn usage_ingestion_bound_refusal_is_typed_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let billing = billing_service();
    let deps = test_deps(dir.path()).with_billing(billing.clone());
    let organization = OrganizationId::try_new("org_bound").unwrap();
    provision_account(&billing, &organization);
    let ws = deps.session.create_workspace("/w").unwrap();
    let _sess = deps
        .session
        .create_session(ws, "t", "managed-provider", "m")
        .unwrap();
    let state = crate::api::AppState {
        deps: Arc::new(deps),
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let err = super::backfill_local_spend_with_bounds(&state, &organization, 0, 500).unwrap_err();
    assert_eq!(err.code, "usage_ingest_truncated");
    assert_eq!(err.http_status, 409);
    assert!(
        billing
            .store()
            .usage_events(&organization, 0, 10)
            .unwrap()
            .is_empty(),
        "a bounded-window refusal wrote nothing"
    );
    let (appended, scanned) =
        super::backfill_local_spend_with_bounds(&state, &organization, 2_000, 500).unwrap();
    assert_eq!(appended, 0);
    assert_eq!(scanned, 1);
}

/// Adversarial: once the commercial plane is wired, the org-less legacy
/// aliases would bypass tenant scope. They require `?org=`; the pre-billing
/// shape stays frozen when billing is disabled.
#[tokio::test]
async fn legacy_usage_and_outcomes_require_org_when_billing_is_wired() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, _control_plane, _billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    for path in ["/native/usage", "/native/outcomes"] {
        let resp = client
            .get(format!("{base}{path}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{path} must require ?org=");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "org_required");
    }
}

/// A grant may name a NON-default account of the organization: the route
/// must resolve that exact account (not redirect the money to the first
/// one), and an unknown/foreign account is a typed 404 with no write.
#[tokio::test]
async fn credits_grant_targets_a_named_account_and_never_redirects() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "named@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    let default_account = provision_account(&billing, &organization);
    let second = faktor_cloud::BillingAccountId::try_new("acct_second").unwrap();
    billing
        .ensure_account(&organization, &second, "second", true)
        .unwrap();

    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-named")
        .json(&serde_json::json!({
            "account_id": second.as_str(),
            "amount_micro": 750,
            "reason": "named"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the named account is grantable");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(money(&body["credits"]["granted_micro"]), 750);
    let entries = billing.credit_entries_page(&organization, 0, 10).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].entry.billing_account_id, second);
    assert_ne!(entries[0].entry.billing_account_id, default_account);

    let resp = client
        .post(format!("{base}/native/credits/grant"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-missing")
        .json(&serde_json::json!({
            "account_id": "acct_absent",
            "amount_micro": 10
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(
        billing
            .credit_entries_page(&organization, 0, 10)
            .unwrap()
            .len(),
        1,
        "the refused grant wrote nothing"
    );
}

/// A missing billing account must not burn the idempotency key: the 404
/// happens before the claim, so a later (even different) request under the
/// same key is not falsely a conflict for an effect that never happened.
#[tokio::test]
async fn missing_account_does_not_burn_the_subscription_idempotency_key() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, owner_token) = bootstrap(&control_plane, "A", "noacct@a.test");
    let organization = OrganizationId::try_new(org).unwrap();

    let denied = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-before-provision")
        .json(&serde_json::json!({"action": "cancel", "confirm": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);

    // Provision the account, then reuse the SAME key with a different
    // action: it must be a fresh mutation, not a conflict with the 404.
    provision_account(&billing, &organization);
    let applied = client
        .post(format!("{base}/native/billing/subscription"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_token)
        .header("idempotency-key", "k-before-provision")
        .json(&serde_json::json!({"action": "change", "plan_id": "pro"}))
        .send()
        .await
        .unwrap();
    assert_eq!(applied.status(), 200);
    let body: serde_json::Value = applied.json().await.unwrap();
    assert_eq!(body["subscription"]["plan_id"], "pro");
    assert_eq!(body["replayed"], false);
}

/// The request-side cacheable-prefix observation is NOT cache-read usage:
/// the durable settled row already folds cache reads/writes into its input
/// total. Projecting the prefix size as `cache_read_tokens` would double
/// count every cached call in the token quota and fabricate a cache line.
#[tokio::test]
async fn usage_ingestion_never_counts_the_request_prefix_as_cache_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "A", "prefix@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);

    let ws = session.create_workspace("/w").unwrap();
    let sess = session
        .create_session(ws, "t", "managed-provider", "m")
        .unwrap();
    let task = sess
        .create_task(faktor_session::Task {
            task_id: sess.task_id().unwrap(),
            session_id: sess.id(),
            goal: "prefix".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            contract: Default::default(),
            budget: Default::default(),
            state: faktor_core::state::TaskState::Running,
            created_ms: NOW_MS,
            updated_ms: NOW_MS,
        })
        .unwrap();
    let store = session.store();
    let attempt = faktor_core::op::ModelCallAttempt::new(OpId::new(1), OpId::new(2), 1).unwrap();
    let reservation = match store
        .cost_reserve_attempt(
            sess.id(),
            task.task_id,
            &attempt,
            10_000,
            NOW_MS,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .unwrap()
    {
        faktor_store::CostReserveOutcome::Granted(id) => id,
        other => panic!("reservation refused: {other:?}"),
    };
    assert!(matches!(
        store.cost_mark_dispatched(reservation, NOW_MS).unwrap(),
        faktor_store::CostReservationState::Applied
    ));
    // The completed call carries a 64-token CACHEABLE PREFIX. Its settled
    // input total of 100 already includes whatever cache reads occurred.
    store
        .record_provider_call_with_prefix_segments(
            sess.id(),
            OpId::new(1),
            "managed-provider",
            "m",
            "completed",
            Some(100),
            Some(20),
            None,
            Some([7u8; 32]),
            Some(64),
            Some(1.0),
            None,
        )
        .unwrap();
    assert!(matches!(
        store
            .cost_settle_usage(reservation, 100, 0, 0, 20, Some(500), None, NOW_MS)
            .unwrap(),
        faktor_store::CostSettleOutcome::Applied { actual_micro: 500 }
    ));

    let resp = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let totals = &body["fold"]["totals"];
    assert_eq!(totals["input_tokens"].as_u64(), Some(100));
    assert_eq!(totals["output_tokens"].as_u64(), Some(20));
    assert_eq!(
        totals["cache_read_tokens"].as_u64(),
        Some(0),
        "the prefix size is not a cache-read count"
    );
    assert_eq!(totals["cache_write_tokens"].as_u64(), Some(0));
    let total_tokens = totals["input_tokens"].as_u64().unwrap()
        + totals["output_tokens"].as_u64().unwrap()
        + totals["cache_read_tokens"].as_u64().unwrap()
        + totals["cache_write_tokens"].as_u64().unwrap()
        + totals["reasoning_tokens"].as_u64().unwrap();
    assert_eq!(total_tokens, 120, "no double count: {body}");
    assert_eq!(totals["events"].as_u64(), Some(3));
    assert_eq!(money(&totals["provider_cost_micro"]), 500);
}

#[tokio::test]
async fn a_faulted_list_tasks_refuses_the_legacy_projection_instead_of_underreporting() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);

    let ws = session.create_workspace("/w").unwrap();
    let sess = session
        .create_session(ws, "t", "managed-provider", "m")
        .unwrap();
    sess.create_task(faktor_session::Task {
        task_id: sess.task_id().unwrap(),
        session_id: sess.id(),
        goal: "metered".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        contract: Default::default(),
        budget: Default::default(),
        state: faktor_core::state::TaskState::Running,
        created_ms: NOW_MS,
        updated_ms: NOW_MS,
    })
    .unwrap();

    // Corrupt one task row behind the store's back so `list_tasks()` returns
    // a store/corruption error (NOT a typed NotFound, which is the only
    // skippable case).
    let conn = rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE task SET acceptance_criteria = 'not-json' WHERE session_id = ?1",
            rusqlite::params![sess.id().raw() as i64],
        )
        .unwrap();
    assert_eq!(changed, 1);

    let response = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        500,
        "a faulted task listing must refuse the projection with a store failure, got {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "store_error", "{body}");
    assert_eq!(
        session.store().billing_outbox_after(0, 10).unwrap().len(),
        0,
        "the refusal happens before any outbox write (no partial underreport)"
    );
}

/// Independent final-wave probe: a task row that decodes as CORRUPT (not a
/// vanished session) must refuse the legacy projection with a typed 500 and
/// write nothing. The wave's own test corrupts `acceptance_criteria`; this
/// probe corrupts the plan column instead, so a partial underreport can never
/// hide behind the only skippable NotFound arm.
#[tokio::test]
async fn a_corrupt_task_plan_refuses_usage_with_a_typed_500_and_no_outbox_write() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, session) = billing_deps(dir.path()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "A", "a@a.test");
    let organization = OrganizationId::try_new(org.clone()).unwrap();
    provision_account(&billing, &organization);

    let ws = session.create_workspace("/w").unwrap();
    let sess = session
        .create_session(ws, "t", "managed-provider", "m")
        .unwrap();
    sess.create_task(faktor_session::Task {
        task_id: sess.task_id().unwrap(),
        session_id: sess.id(),
        goal: "metered".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        contract: Default::default(),
        budget: Default::default(),
        state: faktor_core::state::TaskState::Running,
        created_ms: NOW_MS,
        updated_ms: NOW_MS,
    })
    .unwrap();

    let conn = rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE task SET plan = 'not-json' WHERE session_id = ?1",
            rusqlite::params![sess.id().raw() as i64],
        )
        .unwrap();
    assert_eq!(changed, 1);

    let response = client
        .get(format!("{base}/native/usage?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        500,
        "a corrupt task row is a store failure, not a skippable absence"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "store_error", "{body}");
    assert_eq!(
        session.store().billing_outbox_after(0, 10).unwrap().len(),
        0,
        "the refusal happens before any outbox write"
    );
}
