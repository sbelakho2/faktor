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
        .cost_reserve_attempt(sess.id(), task.task_id, &refund_attempt, 777, NOW_MS, None)
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
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
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
