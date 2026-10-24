use std::sync::Arc;

use axum::http::StatusCode;
use faktor_payments::InvoiceStatus;
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_provider::testing::{MockAction, MockServer};

use crate::test_support::{field_value, Client, TestApp, TEST_NOW_MS, TEST_SECRET};

async fn signup_unverified(app: &TestApp, email: &str, name: &str) -> Client {
    let mut client = app.client();
    let page = client.get("/signup/").await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(
            "/signup/",
            &[
                ("csrf", csrf.as_str()),
                ("name", name),
                ("email", email),
                ("password", "correct horse battery staple"),
                ("confirm", "correct horse battery staple"),
                ("accept_terms", "1"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    client
}

async fn verify_client(app: &TestApp, client: &mut Client, email: &str) {
    let mail = app
        .all_mails()
        .into_iter()
        .rev()
        .find(|mail| mail.to == email && mail.subject.contains("Verify"))
        .expect("verification mail");
    let secret = crate::test_support::extract_secret(&mail.body);
    let page = client.get(&format!("/verify-email/?secret={secret}")).await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(
            "/verify-email/",
            &[("csrf", csrf.as_str()), ("secret", secret.as_str())],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

struct CheckoutData {
    csrf: String,
    request_id: String,
}

async fn signup_verified(app: &TestApp, email: &str) -> Client {
    let mut client = signup_unverified(app, email, "Test User").await;
    verify_client(app, &mut client, email).await;
    client
}

async fn billing_form(client: &mut Client) -> CheckoutData {
    let page = client.get("/account/billing/").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    CheckoutData {
        csrf: client.form_csrf(&page.body),
        request_id: field_value(&page.body, "request_id").expect("checkout request id"),
    }
}

async fn start_local_checkout(client: &mut Client) -> String {
    let form = billing_form(client).await;
    let response = client
        .post(
            "/account/billing/checkout/",
            &[
                ("csrf", form.csrf.as_str()),
                ("plan_id", "solo"),
                ("interval", "monthly"),
                ("request_id", form.request_id.as_str()),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    response
        .headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

async fn pay_local(client: &mut Client, location: &str) -> String {
    let page = client.get(location).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(&format!("{location}pay/"), &[("csrf", csrf.as_str())])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    response
        .headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn invoice_for(app: &TestApp, org: &str) -> faktor_payments::Invoice {
    let org = faktor_cloud::OrganizationId::try_new(org.to_string()).unwrap();
    app.payments()
        .invoices(&org, None, 10)
        .unwrap()
        .items
        .into_iter()
        .next()
        .expect("an invoice")
}

fn local_webhook_body(
    org: &str,
    invoice: &faktor_payments::Invoice,
    event_id: &str,
    provider_id: &str,
) -> String {
    serde_json::json!({
        "id": event_id,
        "type": "payment.succeeded",
        "created": TEST_NOW_MS,
        "data": {"object": {
            "id": provider_id,
            "status": "succeeded",
            "amount_minor": invoice.total_minor,
            "currency": invoice.currency,
            "metadata": {
                "organization_id": org,
                "local_invoice_id": invoice.id,
            }
        }}
    })
    .to_string()
}

#[tokio::test]
async fn signup_login_logout_and_verification_gate() {
    let app = TestApp::build();
    let mut client = signup_unverified(&app, "person@example.com", "Person Example").await;
    let signup_cookies = app
        .all_mails()
        .into_iter()
        .find(|mail| mail.to == "person@example.com")
        .map(|mail| mail.body)
        .unwrap();
    assert!(signup_cookies.contains("verify-email"));
    let account = client.get("/account/").await;
    eprintln!(
        "ACCOUNT {:?} {:?}",
        account.status,
        client.cookie("faktor_site_session")
    );
    assert_eq!(account.status, StatusCode::OK);
    assert!(account.body.contains("Verify your email"));
    assert!(!account.body.contains("<script"));
    assert!(!account.body.contains(" style="));

    let form = billing_form(&mut client).await;
    let blocked = client
        .post(
            "/account/billing/checkout/",
            &[
                ("csrf", form.csrf.as_str()),
                ("plan_id", "solo"),
                ("interval", "monthly"),
                ("request_id", form.request_id.as_str()),
            ],
        )
        .await;
    assert_eq!(blocked.status, StatusCode::FORBIDDEN);

    verify_client(&app, &mut client, "person@example.com").await;
    let account = client.get("/account/").await;
    assert_eq!(account.status, StatusCode::OK);
    assert!(!account.body.contains("Verify your email to use billing"));

    let location = start_local_checkout(&mut client).await;
    assert!(
        location.starts_with("/account/billing/local/chk_"),
        "{location}"
    );
    let return_url = pay_local(&mut client, &location).await;
    assert!(return_url.contains("invoice="));

    let logout_csrf = client.form_csrf(&account.body);
    let response = client.post("/logout/", &[("csrf", &logout_csrf)]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        client.get("/account/").await.status,
        StatusCode::UNAUTHORIZED
    );

    let login = client.get("/login/").await;
    let csrf = client.form_csrf(&login.body);
    let response = client
        .post(
            "/login/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "person@example.com"),
                ("password", "correct horse battery staple"),
                ("next", "/account/"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let set_cookie = response
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.contains("faktor_site_session"))
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    assert!(set_cookie.contains("Path=/"));
    assert_eq!(client.get("/account/").await.status, StatusCode::OK);
}

#[tokio::test]
async fn csrf_and_rate_limit_are_enforced_with_retry_after() {
    let app = TestApp::build();
    let mut client = app.client();
    let page = client.get("/login/").await;
    let bad = "A".repeat(43);
    let response = client
        .post(
            "/login/",
            &[
                ("csrf", &bad),
                ("email", "nobody@example.com"),
                ("password", "wrong password value"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    let csrf = client.form_csrf(&page.body);
    let mut limited = None;
    for attempt in 0..6 {
        let response = client
            .post(
                "/login/",
                &[
                    ("csrf", csrf.as_str()),
                    ("email", "limit@example.com"),
                    ("password", "wrong password value"),
                ],
            )
            .await;
        if attempt == 5 {
            limited = Some(response);
        }
    }
    let limited = limited.unwrap();
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.contains_key("retry-after"));
}

#[tokio::test]
async fn password_reset_is_single_use_and_revokes_sessions() {
    let app = TestApp::build();
    let mut first = signup_verified(&app, "reset@example.com").await;
    let mut second = app.client();
    let login = second.get("/login/").await;
    let csrf = second.form_csrf(&login.body);
    let response = second
        .post(
            "/login/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "reset@example.com"),
                ("password", "correct horse battery staple"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(second.get("/account/").await.status, StatusCode::OK);

    let forgot = first.get("/forgot-password/").await;
    let csrf = first.form_csrf(&forgot.body);
    let response = first
        .post(
            "/forgot-password/",
            &[("csrf", csrf.as_str()), ("email", "reset@example.com")],
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let mail = app
        .all_mails()
        .into_iter()
        .rev()
        .find(|mail| mail.to == "reset@example.com" && mail.subject.contains("reset"))
        .expect("reset mail");
    let secret = crate::test_support::extract_secret(&mail.body);

    let page = first
        .get(&format!("/reset-password/?secret={secret}"))
        .await;
    let csrf = first.form_csrf(&page.body);
    let response = first
        .post(
            "/reset-password/",
            &[
                ("csrf", csrf.as_str()),
                ("secret", secret.as_str()),
                ("password", "a new correct password"),
                ("confirm", "a new correct password"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        first.get("/account/").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        second.get("/account/").await.status,
        StatusCode::UNAUTHORIZED
    );

    let reuse = app.client();
    let mut reuse = reuse;
    let page = reuse
        .get(&format!("/reset-password/?secret={secret}"))
        .await;
    let csrf = reuse.form_csrf(&page.body);
    let response = reuse
        .post(
            "/reset-password/",
            &[
                ("csrf", csrf.as_str()),
                ("secret", secret.as_str()),
                ("password", "another correct password"),
                ("confirm", "another correct password"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.contains("invalid, expired or already used"));

    let login = reuse.get("/login/").await;
    let csrf = reuse.form_csrf(&login.body);
    let response = reuse
        .post(
            "/login/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "reset@example.com"),
                ("password", "a new correct password"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn local_billing_flow_records_exact_money_receipts_methods_changes_and_cancel() {
    let app = TestApp::build();
    let mut client = signup_verified(&app, "billing@example.com").await;
    let location = start_local_checkout(&mut client).await;
    let return_url = pay_local(&mut client, &location).await;
    let page = client.get(&return_url).await;
    assert_eq!(page.status, StatusCode::OK);

    let org = app.org_for_email("billing@example.com");
    let invoice = invoice_for(&app, &org);
    assert_eq!(invoice.total_minor, 2_299);
    assert_eq!(invoice.tax_minor, 399);
    assert_eq!(invoice.amount_paid_minor, 2_299);
    assert_eq!(invoice.status, InvoiceStatus::Paid);
    let receipt = client
        .get(&format!("/account/invoices/{}/receipt/", invoice.id))
        .await;
    assert_eq!(receipt.status, StatusCode::OK);
    assert!(receipt.body.contains("Receipt"));

    let billing = client.get("/account/billing/").await;
    assert!(!billing.body.contains("<script"));
    assert!(!billing.body.contains(" style="));
    let csrf = client.form_csrf(&billing.body);
    let response = client
        .post(
            "/account/billing/payment-methods/add/",
            &[
                ("csrf", csrf.as_str()),
                ("provider_token", "tok_test_visa_4242"),
                ("brand", "Visa"),
                ("last4", "4242"),
                ("exp_month", "12"),
                ("exp_year", "2030"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let methods = app
        .payments()
        .payment_methods(&faktor_cloud::OrganizationId::try_new(org.clone()).unwrap())
        .unwrap();
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].last4, "4242");
    let remove = client
        .post(
            &format!("/account/billing/payment-methods/{}/remove/", methods[0].id),
            &[("csrf", csrf.as_str())],
        )
        .await;
    assert_eq!(remove.status, StatusCode::SEE_OTHER);

    let response = client
        .post(
            "/account/billing/change-plan/",
            &[
                ("csrf", csrf.as_str()),
                ("plan_id", "team"),
                ("interval", "annual"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let org_id = faktor_cloud::OrganizationId::try_new(org.clone()).unwrap();
    let overview = app.payments().overview(&org_id).unwrap();
    assert_eq!(overview.subscription.unwrap().plan_id, "team");

    let response = client
        .post(
            "/account/billing/cancel/",
            &[("csrf", csrf.as_str()), ("at_period_end", "1")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let overview = app.payments().overview(&org_id).unwrap();
    assert!(overview.subscription.unwrap().cancel_at_period_end);
}

#[tokio::test]
async fn tenant_isolation_and_owner_invariants_hold() {
    let app = TestApp::build();
    let mut alice = signup_verified(&app, "alice@example.com").await;
    let mut bob = signup_verified(&app, "bob@example.com").await;
    let location = start_local_checkout(&mut alice).await;
    let _ = pay_local(&mut alice, &location).await;
    let alice_org = app.org_for_email("alice@example.com");
    let invoice = invoice_for(&app, &alice_org);

    assert_eq!(
        bob.get(&format!("/account/invoices/{}/", invoice.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.get(&format!("/account/invoices/{}/receipt/", invoice.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let page = bob.get("/account/billing/").await;
    let csrf = bob.form_csrf(&page.body);
    let response = bob
        .post(
            &format!("/account/billing/invoices/{}/pay/", invoice.id),
            &[("csrf", csrf.as_str())],
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    let page = alice.get("/account/organization/").await;
    let csrf = alice.form_csrf(&page.body);
    let response = alice
        .post(
            "/account/organization/members/add/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "bob@example.com"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);

    let page = bob.get("/account/").await;
    let csrf = bob.form_csrf(&page.body);
    let response = bob
        .post(
            "/account/organization/switch/",
            &[("csrf", csrf.as_str()), ("org_id", alice_org.as_str())],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);

    let form = billing_form(&mut bob).await;
    let blocked = bob
        .post(
            "/account/billing/checkout/",
            &[
                ("csrf", form.csrf.as_str()),
                ("plan_id", "solo"),
                ("interval", "monthly"),
                ("request_id", form.request_id.as_str()),
            ],
        )
        .await;
    assert_eq!(blocked.status, StatusCode::FORBIDDEN);
    let member_add = bob
        .post(
            "/account/organization/members/add/",
            &[
                ("csrf", form.csrf.as_str()),
                ("email", "eve@example.com"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(member_add.status, StatusCode::FORBIDDEN);

    let page = alice.get("/account/organization/").await;
    let csrf = alice.form_csrf(&page.body);
    let alice_id = app.user_id_for_email("alice@example.com");
    let response = alice
        .post(
            "/account/organization/members/remove/",
            &[("csrf", csrf.as_str()), ("user_id", alice_id.as_str())],
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);

    let bob_id = app.user_id_for_email("bob@example.com");
    let response = alice
        .post(
            "/account/organization/members/update/",
            &[
                ("csrf", csrf.as_str()),
                ("user_id", bob_id.as_str()),
                ("role", "admin"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let page = bob.get("/account/").await;
    assert!(page.body.contains("admin"));
    let add_after_admin = bob
        .post(
            "/account/organization/members/add/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "eve@example.com"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(add_after_admin.status, StatusCode::FORBIDDEN);
}

fn checkout_provider_id(app: &TestApp, org: &str, invoice_id: &str) -> String {
    let conn = app.state.db.conn().unwrap();
    conn.query_row(
        "SELECT provider_checkout_id FROM billing_checkouts WHERE organization_id=?1 AND invoice_id=?2 ORDER BY created_ms DESC LIMIT 1",
        rusqlite::params![org, invoice_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn signature_header(seconds: i64, body: &str) -> String {
    faktor_payments::sign_local_webhook(TEST_SECRET, seconds, body.as_bytes())
}

#[tokio::test]
async fn webhook_signature_timestamp_idempotency_and_bounds() {
    let app = TestApp::build();
    let mut client = signup_verified(&app, "webhook@example.com").await;
    let _ = start_local_checkout(&mut client).await;
    let org = app.org_for_email("webhook@example.com");
    let invoice = invoice_for(&app, &org);
    let provider_id = checkout_provider_id(&app, &org, &invoice.id);
    let body = local_webhook_body(&org, &invoice, "evt_local_route_1", &provider_id);

    let invalid = format!("t={},v1={}", TEST_NOW_MS / 1000, "0".repeat(64));
    let response = client
        .post_raw_with(
            "/webhooks/payments",
            &body,
            "application/json",
            &[("x-faktor-signature", invalid.as_str())],
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        response.body
    );

    let stale_header = signature_header(TEST_NOW_MS / 1000 - 3_600, &body);
    let stale = client
        .post_raw_with(
            "/webhooks/payments",
            &body,
            "application/json",
            &[("x-faktor-signature", stale_header.as_str())],
        )
        .await;
    assert_eq!(stale.status, StatusCode::UNAUTHORIZED, "{}", stale.body);

    let header = signature_header(TEST_NOW_MS / 1000, &body);
    let valid = client
        .post_raw_with(
            "/webhooks/payments",
            &body,
            "application/json",
            &[("x-faktor-signature", header.as_str())],
        )
        .await;
    assert_eq!(valid.status, StatusCode::OK, "{}", valid.body);
    assert_eq!(valid.body, "[accepted]");
    let paid = invoice_for(&app, &org);
    assert_eq!(paid.status, InvoiceStatus::Paid);
    assert_eq!(paid.amount_paid_minor, paid.total_minor);

    let replay = client
        .post_raw_with(
            "/webhooks/payments",
            &body,
            "application/json",
            &[("x-faktor-signature", header.as_str())],
        )
        .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body, "[duplicate]");

    let malformed = "{";
    let malformed_header = signature_header(TEST_NOW_MS / 1000, malformed);
    let malformed_response = client
        .post_raw_with(
            "/webhooks/payments",
            malformed,
            "application/json",
            &[("x-faktor-signature", malformed_header.as_str())],
        )
        .await;
    assert_eq!(malformed_response.status, StatusCode::BAD_REQUEST);

    let oversized = "x".repeat(300 * 1024);
    let oversized_response = client
        .post_raw("/webhooks/payments", &oversized, "application/json")
        .await;
    assert_eq!(oversized_response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

async fn start_mollie_checkout(client: &mut Client) -> String {
    let form = billing_form(client).await;
    let response = client
        .post(
            "/account/billing/checkout/",
            &[
                ("csrf", form.csrf.as_str()),
                ("plan_id", "solo"),
                ("interval", "monthly"),
                ("request_id", form.request_id.as_str()),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    response
        .headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn mollie_handoff_and_id_only_webhook_fetch_settle_the_invoice() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v2/customers",
        MockAction::Respond {
            status: 201,
            body: include_str!("../../payments/tests/fixtures/mollie/customer.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v2/customers/cst_customer_1/subscriptions",
        MockAction::Respond {
            status: 201,
            body: include_str!("../../payments/tests/fixtures/mollie/subscription_active.json")
                .into(),
        },
    );
    mock.route(
        "POST",
        "/v2/payments",
        MockAction::Respond {
            status: 201,
            body: include_str!("../../payments/tests/fixtures/mollie/payment_open.json").into(),
        },
    );
    let base = mock.base_url().await;
    let transport = Arc::new(PolicyCheckedHttpTransport::permissive());
    let app = TestApp::build_with_mollie(transport, format!("{base}/v2"));
    let mut client = signup_verified(&app, "mollie@example.com").await;
    let location = start_mollie_checkout(&mut client).await;
    assert_eq!(location, "https://checkout.mollie.test/tr_payment_1");

    let org = app.org_for_email("mollie@example.com");
    let invoice = invoice_for(&app, &org);
    let amount_value = format!(
        "{}.{:02}",
        invoice.total_minor / 100,
        invoice.total_minor % 100
    );
    let paid = include_str!("../../payments/tests/fixtures/mollie/payment_paid.json")
        .replace("\"org_1\"", &format!("\"{org}\""))
        .replace("\"inv_1\"", &format!("\"{}\"", invoice.id))
        .replace("\"USD\"", &format!("\"{}\"", invoice.currency))
        .replace("\"100.00\"", &format!("\"{amount_value}\""));
    mock.route(
        "GET",
        "/v2/payments/tr_payment_1",
        MockAction::Respond {
            status: 200,
            body: paid,
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_forged",
        MockAction::Respond {
            status: 404,
            body: include_str!("../../payments/tests/fixtures/mollie/error_not_found.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_foreign",
        MockAction::Respond {
            status: 200,
            body: include_str!("../../payments/tests/fixtures/mollie/payment_foreign.json").into(),
        },
    );

    // The posted `status=failed` is untrusted; the fetched resource is paid.
    let body = "id=tr_payment_1&status=failed";
    let response = client
        .post_raw(
            "/webhooks/payments",
            body,
            "application/x-www-form-urlencoded",
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body, "[accepted]");
    let paid_invoice = invoice_for(&app, &org);
    assert_eq!(paid_invoice.status, InvoiceStatus::Paid);
    assert_eq!(paid_invoice.amount_paid_minor, paid_invoice.total_minor);

    let replay = client
        .post_raw(
            "/webhooks/payments",
            body,
            "application/x-www-form-urlencoded",
        )
        .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body, "[duplicate]");

    let forged = client
        .post_raw(
            "/webhooks/payments",
            "id=tr_forged",
            "application/x-www-form-urlencoded",
        )
        .await;
    assert_eq!(forged.status, StatusCode::NOT_FOUND, "{}", forged.body);
    let foreign = client
        .post_raw(
            "/webhooks/payments",
            "id=tr_foreign",
            "application/x-www-form-urlencoded",
        )
        .await;
    assert_eq!(foreign.status, StatusCode::UNAUTHORIZED, "{}", foreign.body);
}

#[tokio::test]
async fn platform_admin_usage_and_disable_are_authorized_and_audited() {
    let app = TestApp::build();
    let mut admin = signup_verified(&app, "admin@example.com").await;
    let mut user = signup_verified(&app, "managed@example.com").await;
    let location = start_local_checkout(&mut user).await;
    let _ = pay_local(&mut user, &location).await;
    let user_org = app.org_for_email("managed@example.com");
    let user_id = app.user_id_for_email("managed@example.com");

    assert_eq!(user.get("/admin/").await.status, StatusCode::NOT_FOUND);
    let page = admin.get("/admin/").await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = admin.form_csrf(&page.body);
    let response = admin
        .post(
            "/admin/usage/",
            &[
                ("csrf", csrf.as_str()),
                ("organization_id", user_org.as_str()),
                ("description", "audited task minute"),
                ("quantity", "5"),
                ("unit_amount_minor", "100"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let invoices = app
        .payments()
        .invoices(
            &faktor_cloud::OrganizationId::try_new(user_org.clone()).unwrap(),
            None,
            10,
        )
        .unwrap();
    assert!(invoices
        .items
        .iter()
        .any(|invoice| invoice.lines.iter().any(|line| {
            line.kind == faktor_payments::InvoiceLineKind::Usage
                && line.description == "audited task minute"
        })));

    let page = admin.get("/admin/").await;
    let csrf = admin.form_csrf(&page.body);
    let response = admin
        .post(
            "/admin/users/disable/",
            &[
                ("csrf", csrf.as_str()),
                ("user_id", user_id.as_str()),
                ("disabled", "1"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(user.get("/account/").await.status, StatusCode::UNAUTHORIZED);
    let audit_count: i64 = app
        .state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM admin_audit WHERE action='admin.user.disable'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 1);
}

#[test]
fn zero_javascript_design_tokens_and_baselines_are_intact() {
    use std::path::Path;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let site = root.join("site");
    let templates = site.join("templates");
    let static_dir = site.join("static");
    let mut scanned = 0usize;
    for dir in [&templates, &static_dir] {
        for entry in walk(dir) {
            let extension = entry.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            if !matches!(extension, "html" | "css" | "svg" | "json" | "txt" | "md") {
                continue;
            }
            let text = std::fs::read_to_string(&entry).unwrap();
            scanned += 1;
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("<script"), "script in {}", entry.display());
            assert!(
                !lower.contains("javascript:"),
                "javascript URI in {}",
                entry.display()
            );
            assert!(
                !lower.contains(" onclick="),
                "inline handler in {}",
                entry.display()
            );
            assert!(
                !lower.contains(" onload="),
                "inline handler in {}",
                entry.display()
            );
            assert!(
                !lower.contains(" style="),
                "inline style in {}",
                entry.display()
            );
            let mut search = lower.as_str();
            while let Some(index) = search.find("<form") {
                let rest = &search[index..];
                let tag_end = rest.find('>').unwrap_or(rest.len());
                let tag = &rest[..tag_end];
                assert!(
                    tag.contains("method=\"post\""),
                    "non-POST form in {}",
                    entry.display()
                );
                search = &rest[tag_end..];
            }
        }
    }
    assert!(scanned > 10);
    for entry in walk(&site) {
        assert_ne!(
            entry.extension().and_then(|ext| ext.to_str()),
            Some("js"),
            "JavaScript file in {}",
            entry.display()
        );
    }
    let css = std::fs::read_to_string(static_dir.join("css/site.css")).unwrap();
    for token in [
        "--ink: #08110C",
        "--forest: #143D2B",
        "--paper: #F3F5EF",
        "--sage: #92AA99",
        "--ochre: #D9BA75",
        "--ochre-deep: #72541E",
        "--fs-display: clamp(56px, 7.5vw, 108px)",
        "--fs-page-title: clamp(48px, 7.5vw, 108px)",
        "--fs-h2: clamp(38px, 4.03vw, 58px)",
        "--fs-lead: clamp(19px, 1.54vw, 22px)",
        "--gutter: clamp(22px, 2.5vw, 36px)",
    ] {
        assert!(css.contains(token), "missing token {token}");
    }
    for breakpoint in ["1180px", "1023px", "820px", "640px"] {
        assert!(
            css.contains(&format!("max-width: {breakpoint}")),
            "missing breakpoint {breakpoint}"
        );
    }
    for font in [
        "barlow-400.woff2",
        "barlow-500.woff2",
        "barlow-600.woff2",
        "barlow-sc-600.woff2",
        "plex-mono-400.woff2",
        "plex-mono-500.woff2",
    ] {
        assert!(
            static_dir.join("fonts").join(font).is_file(),
            "missing font {font}"
        );
    }
    let tokens: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(static_dir.join("downloads/faktor-design-tokens.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tokens["color"]["ink"], "#08110C");
    assert_eq!(tokens["layout"]["gutterDesktopPx"], 36);
    assert_eq!(tokens["layout"]["designWidthPx"], 1440);
    let built = site.join("public");
    if built.exists() {
        for entry in walk(&built) {
            if entry.extension().and_then(|ext| ext.to_str()) == Some("html") {
                let text = std::fs::read_to_string(&entry).unwrap();
                assert!(
                    !text.contains("<script"),
                    "script in built {}",
                    entry.display()
                );
                assert!(
                    !text.contains(" onclick="),
                    "handler in built {}",
                    entry.display()
                );
            }
        }
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn display_names_are_escaped_and_session_idle_expiry_revokes() {
    let app = TestApp::build();
    let mut client =
        signup_unverified(&app, "escape@example.com", "<script>alert(1)</script>").await;
    let page = client.get("/account/").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("<script>alert(1)</script>"));
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    app.clock.advance(8 * 24 * 60 * 60 * 1000);
    assert_eq!(
        client.get("/account/").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn pan_shaped_payment_tokens_are_refused_and_no_card_fields_exist() {
    let app = TestApp::build();
    let mut client = signup_verified(&app, "pan@example.com").await;
    let page = client.get("/account/billing/").await;
    let csrf = client.form_csrf(&page.body);
    for forbidden in [
        "name=\"pan\"",
        "name=\"cvc\"",
        "name=\"cvv\"",
        "name=\"card_number\"",
    ] {
        assert!(
            !page.body.contains(forbidden),
            "unexpected field {forbidden}"
        );
    }
    let response = client
        .post(
            "/account/billing/payment-methods/add/",
            &[
                ("csrf", csrf.as_str()),
                ("provider_token", "4111 1111 1111 1111"),
                ("brand", "Visa"),
                ("last4", "1111"),
                ("exp_month", "12"),
                ("exp_year", "2030"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    let org = app.org_for_email("pan@example.com");
    let methods = app
        .payments()
        .payment_methods(&faktor_cloud::OrganizationId::try_new(org).unwrap())
        .unwrap();
    assert!(methods.is_empty());
}

#[tokio::test]
async fn forgot_password_does_not_disclose_account_existence() {
    let app = TestApp::build();
    let mut unknown = app.client();
    let page = unknown.get("/forgot-password/").await;
    let csrf = unknown.form_csrf(&page.body);
    let unknown_response = unknown
        .post(
            "/forgot-password/",
            &[("csrf", csrf.as_str()), ("email", "nobody@example.com")],
        )
        .await;
    assert_eq!(unknown_response.status, StatusCode::OK);
    assert!(app.all_mails().is_empty());

    let mut client = signup_verified(&app, "known@example.com").await;
    let page = client.get("/forgot-password/").await;
    let csrf = client.form_csrf(&page.body);
    let known_response = client
        .post(
            "/forgot-password/",
            &[("csrf", csrf.as_str()), ("email", "known@example.com")],
        )
        .await;
    assert_eq!(known_response.status, StatusCode::OK);
    assert!(app
        .all_mails()
        .iter()
        .any(|mail| mail.to == "known@example.com" && mail.subject.contains("reset")));
    let normalize = |body: &str| body.split("name=\"csrf\"").next().unwrap_or("").to_string();
    assert_eq!(
        normalize(&unknown_response.body),
        normalize(&known_response.body)
    );
}

#[tokio::test]
async fn oversized_forms_are_bounded_before_parsing() {
    let app = TestApp::build();
    let mut client = app.client();
    let body = format!("csrf={}&email={}", "A".repeat(43), "a".repeat(70 * 1024));
    let response = client
        .post_raw(
            "/forgot-password/",
            &body,
            "application/x-www-form-urlencoded",
        )
        .await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn visual_review_artifacts_exist_at_target_widths() {
    use std::path::Path;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let visual = root.join("site/visual");
    for route in [
        "home",
        "approach",
        "documentation",
        "evidence",
        "outcomes",
        "workflow",
        "patterns",
        "identity",
        "research",
        "editor",
        "plans",
        "login",
        "signup",
        "prior",
        "prior-approach",
        "prior-identity",
    ] {
        for width in ["1440", "1024", "390"] {
            let path = visual.join(format!("baselines/{route}-{width}.webp"));
            assert!(path.is_file(), "missing visual baseline {}", path.display());
            assert!(
                std::fs::metadata(&path).unwrap().len() > 1_000,
                "empty visual baseline {}",
                path.display()
            );
        }
    }
    assert!(visual.join("pdf-reference.webp").is_file());
    for page in 2..=57 {
        assert!(
            visual.join(format!("pdf/page-{page:02}.webp")).is_file(),
            "missing PDF page reference {page}"
        );
    }
    for name in [
        "home",
        "approach",
        "documentation",
        "evidence",
        "outcomes",
        "workflow",
        "patterns",
        "identity",
        "research",
        "editor",
        "prior",
        "prior-approach",
        "prior-identity",
    ] {
        assert!(visual.join(format!("parity/{name}.webp")).is_file());
    }
}

#[test]
fn displayed_plans_match_the_billing_catalogue_exactly() {
    let config = crate::test_support::test_config();
    for view in crate::billing::PLANS {
        let price = config
            .payments
            .catalogue
            .plans
            .get(view.id)
            .unwrap_or_else(|| panic!("billing catalogue is missing {}", view.id));
        assert_eq!(price.monthly_minor, view.monthly_minor, "{}", view.id);
        assert_eq!(price.annual_minor, view.annual_minor, "{}", view.id);
        assert_eq!(price.currency, "EUR");
        assert_eq!(price.per_seat_monthly_minor, 0);
        assert_eq!(price.per_seat_annual_minor, 0);
    }
}

#[tokio::test]
async fn adding_an_existing_owner_cannot_demote_the_last_owner() {
    let app = TestApp::build();
    let mut client = signup_verified(&app, "owner@example.com").await;
    let page = client.get("/account/organization/").await;
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(
            "/account/organization/members/add/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "owner@example.com"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    let org = app.org_for_email("owner@example.com");
    let role: String = app
        .state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT role FROM memberships WHERE organization_id=?1",
            rusqlite::params![org],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role, "owner");
}

#[test]
fn provider_checkout_returns_to_the_registered_return_route() {
    let invoice = faktor_payments::Invoice {
        id: "inv_test".into(),
        organization_id: "org_test".into(),
        number: "INV-1".into(),
        subscription_id: Some("sub_test".into()),
        provider_invoice_id: None,
        provider_idempotency_key: None,
        reconciliation: Default::default(),
        currency: "EUR".into(),
        status: faktor_payments::InvoiceStatus::Open,
        lines: Vec::new(),
        subtotal_minor: 100,
        discount_minor: 0,
        credits_minor: 0,
        tax_minor: 21,
        tax_jurisdiction: Some("NL".into()),
        tax_inclusive: false,
        total_minor: 121,
        amount_paid_minor: 0,
        amount_refunded_minor: 0,
        period_start_ms: None,
        period_end_ms: None,
        issued_ms: 1,
        due_ms: 2,
        paid_ms: None,
        voided_ms: None,
        created_ms: 1,
    };
    let org = faktor_cloud::OrganizationId::try_new("org_test".to_string()).unwrap();
    let request = crate::billing::checkout_request(
        &org,
        "customer",
        &invoice,
        "solo",
        "https://faktor.example",
        "idem".into(),
    )
    .unwrap();
    assert_eq!(
        request.success_url,
        "https://faktor.example/account/billing/checkout/return/?invoice=inv_test"
    );
    assert_eq!(request.amount_minor, 121);
}

#[tokio::test]
async fn password_change_revokes_other_sessions_but_keeps_the_current_one() {
    let app = TestApp::build();
    let mut first = signup_verified(&app, "password@example.com").await;
    let mut second = app.client();
    let page = second.get("/login/").await;
    let csrf = second.form_csrf(&page.body);
    let response = second
        .post(
            "/login/",
            &[
                ("csrf", csrf.as_str()),
                ("email", "password@example.com"),
                ("password", "correct horse battery staple"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(second.get("/account/").await.status, StatusCode::OK);

    let page = first.get("/account/security/").await;
    let csrf = first.form_csrf(&page.body);
    let response = first
        .post(
            "/account/security/password/",
            &[
                ("csrf", csrf.as_str()),
                ("current_password", "correct horse battery staple"),
                ("new_password", "a different correct password"),
                ("confirm_password", "a different correct password"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        second.get("/account/").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(first.get("/account/").await.status, StatusCode::OK);
}

fn normalized_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn route_file(route: &str) -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/public");
    let trimmed = route.trim_matches('/');
    if trimmed.is_empty() {
        root.join("index.html")
    } else {
        root.join(trimmed).join("index.html")
    }
}

#[test]
fn pdf_page_copy_is_present_page_by_page() {
    let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/public");
    if !public.join("index.html").is_file() {
        eprintln!("skipping PDF structural check: run `zola --root site build` first");
        return;
    }
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../site/visual/pdf-structure.tsv"),
    )
    .expect("PDF structure manifest");
    let mut routes = std::collections::BTreeSet::new();
    let mut pages = std::collections::BTreeSet::new();
    let mut checked = 0usize;
    for (index, line) in manifest.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            fields.len(),
            3,
            "manifest line {} must have 3 fields",
            index + 1
        );
        let (route, page, phrase) = (fields[0], fields[1], fields[2]);
        let path = route_file(route);
        let html = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let text = normalized_text(&html);
        assert!(
            text.contains(phrase),
            "{} (PDF page {page}) is missing phrase {phrase:?}",
            path.display()
        );
        routes.insert(route.to_string());
        pages.insert(page.parse::<u32>().expect("numeric PDF page"));
        checked += 1;
    }
    assert!(
        checked >= 100,
        "structural manifest is too small: {checked}"
    );
    assert_eq!(
        pages.len(),
        56,
        "the manifest must cover every content PDF page"
    );
    assert_eq!(pages.iter().copied().min(), Some(2));
    assert_eq!(pages.iter().copied().max(), Some(57));
    assert_eq!(routes.len(), 13, "all designed page groups must be checked");

    let home = std::fs::read_to_string(route_file("/")).expect("home page");
    let chrome_start = home.find("site-chrome").expect("site chrome");
    let chrome_end = home[chrome_start..]
        .find("</div>")
        .map(|offset| chrome_start + offset)
        .expect("chrome end");
    let chrome = &home[chrome_start..chrome_end];
    for absent in ["Plans", "Sign in", "Account"] {
        assert!(
            !chrome.contains(absent),
            "PDF chrome must not add the auth entry point {absent:?}"
        );
    }
    assert!(
        !home.contains("<script"),
        "the marketing site must stay JavaScript-free"
    );
}

#[test]
fn prior_edition_routes_are_complete_pages_not_stubs() {
    let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/public");
    if !public.join("index.html").is_file() {
        return;
    }
    for (route, required) in [
        (
            "/prior/",
            vec!["Spend. Rework. Outcome.", "What stays in the IDE?"],
        ),
        (
            "/prior/approach/",
            vec!["A green result has a scope.", "Start from the source."],
        ),
        (
            "/prior/identity/",
            vec!["Ink, Paper and Ochre.", "Everything in its own project."],
        ),
    ] {
        let path = route_file(route);
        let html = std::fs::read_to_string(&path).expect("prior page");
        let text = normalized_text(&html);
        for phrase in required {
            assert!(
                text.contains(phrase),
                "{} missing {phrase:?}",
                path.display()
            );
        }
        assert!(
            html.contains("prior-banner"),
            "{route} lacks the dated prior-edition banner"
        );
    }
    for removed in [
        "prior/evidence",
        "prior/outcomes",
        "prior/workflow",
        "prior/patterns",
        "prior/editor",
    ] {
        assert!(
            !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../site/public")
                .join(removed)
                .exists(),
            "undesigned prior route {removed} must not be published"
        );
    }
}

#[tokio::test]
async fn public_auth_pages_keep_chrome_free_of_added_auth_links() {
    let app = TestApp::build();
    let mut client = app.client();
    for route in [
        "/login/",
        "/signup/",
        "/forgot-password/",
        "/reset-password/",
    ] {
        let page = client.get(route).await;
        let start = page
            .body
            .find("site-chrome")
            .unwrap_or_else(|| panic!("{route} has no site chrome"));
        let end = page.body[start..]
            .find("</div></div>")
            .map(|offset| start + offset)
            .expect("chrome end");
        let chrome = &page.body[start..end];
        for absent in ["/plans/", "/login/", "/signup/", "/account/"] {
            assert!(
                !chrome.contains(absent),
                "{route} adds the auth chrome link {absent}"
            );
        }
    }
}
