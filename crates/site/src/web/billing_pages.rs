use axum::response::Response;
use faktor_payments::{
    BillingInterval, Invoice, PaymentMethodView, PaymentService, PaymentsOverview,
};

use crate::auth::AuthSession;
use crate::billing::{plan, PLANS};
use crate::error::html_escape;
use crate::web::layout::{account_page, csrf_input};
use crate::web::{date, money, status_badge};

pub struct BillingData<'a> {
    pub payments: &'a PaymentService,
    pub overview: &'a PaymentsOverview,
    pub invoices: &'a [Invoice],
    pub methods: &'a [PaymentMethodView],
    pub request_id: &'a str,
    pub notice: Option<(&'a str, &'a str)>,
}

pub fn billing(
    auth: &AuthSession,
    csrf: &str,
    data: &BillingData<'_>,
    cookies: &[String],
) -> Response {
    let notice = data
        .notice
        .map(|(kind, message)| crate::web::alert(kind, "Billing record updated", message))
        .unwrap_or_default();
    let pending = if data.overview.subscription.is_some() && data.overview.open_invoice_count > 0 {
        crate::web::alert(
            "warn",
            "Payment pending",
            "The subscription record exists, but an invoice is still open. Only a verified provider webhook settles it; a browser return does not.",
        )
    } else {
        String::new()
    };
    let subscription = data
        .overview
        .subscription
        .as_ref()
        .filter(|subscription| subscription.is_live());
    let current = if let Some(subscription) = subscription {
        current_subscription(
            auth,
            csrf,
            data,
            subscription,
            subscription.plan_id.as_str(),
            subscription.interval,
        )
    } else {
        "<article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>No live subscription</h2><p class=\"panel-sub\">Choose a plan. Faktor creates an exact invoice, then hands the payment to the provider.</p></div></div></article>".to_string()
    };
    let provider = format!(
        "<article class=\"panel span-4\"><div class=\"panel-head\"><div><h2>Payment provider</h2><p class=\"panel-sub\">Provider-hosted checkout; no card data reaches Faktor.</p></div>{}</div><dl class=\"kv\"><dt>Provider</dt><dd>{}</dd><dt>State</dt><dd>{}</dd><dt>Tax</dt><dd>{}</dd></dl><p class=\"notice mt-18\">A browser return does not mark a payment paid. Only a verified provider webhook settles the invoice.</p></article>",
        status_badge(data.overview.provider.state),
        html_escape(&data.overview.provider.provider),
        html_escape(data.overview.provider.state),
        html_escape(data.overview.tax.default_jurisdiction.as_deref().unwrap_or("unconfigured"))
    );
    let stats = format!(
        "<article class=\"panel span-4\"><div class=\"panel-head\"><div><h2>Record</h2><p class=\"panel-sub\">Exact minor units, no floating point.</p></div></div><div class=\"row-between\"><div><p class=\"stat\">{}</p><p class=\"stat-label\">Paid total</p></div><div><p class=\"stat\">{}</p><p class=\"stat-label\">Outstanding</p></div></div><dl class=\"kv mt-22\"><dt>Refunded</dt><dd>{}</dd><dt>Invoices</dt><dd>{} / {} open</dd><dt>Seats</dt><dd>{}</dd><dt>Currency</dt><dd>{}</dd><dt>Payment methods</dt><dd>{}</dd></dl></article>",
        money(data.overview.paid_minor, data.overview.currency.as_deref().unwrap_or("EUR")),
        money(data.overview.outstanding_minor, data.overview.currency.as_deref().unwrap_or("EUR")),
        money(data.overview.refunded_minor, data.overview.currency.as_deref().unwrap_or("EUR")),
        data.overview.invoice_count,
        data.overview.open_invoice_count,
        data.overview.seats.map_or_else(|| "unconfigured".to_string(), |seats| seats.to_string()),
        html_escape(data.overview.currency.as_deref().unwrap_or("unconfigured")),
        data.overview.payment_method_count
    );
    let plan_disabled_reason = if subscription.is_some() {
        "<p class=\"plan-note\">A live subscription exists; use <strong>Change plan</strong> above. The plan cards are disabled so a second checkout cannot be started.</p>"
    } else if data.overview.provider.state != "available" {
        &format!(
            "<p class=\"plan-note\">Checkout is disabled: the payment provider is {}. No payment can start until an operator restores it.</p>",
            html_escape(data.overview.provider.state)
        )
    } else {
        ""
    };
    let plan_cards = format!(
        "{}<div class=\"plan-grid\">{}</div>",
        plan_disabled_reason,
        plan_cards(auth, csrf, data, subscription.is_some())
    );
    let invoices = invoice_rows(auth, csrf, data.invoices);
    let methods = payment_methods(
        csrf,
        data.methods,
        &data.overview.provider.provider,
        data.overview.provider.state,
    );
    let content = format!(
        "{}{}<div class=\"app-grid\">{}{}{}</div><div class=\"app-grid mt-22\">{}</div><section class=\"page-section no-rule\"><div class=\"section-head\"><div><p class=\"kicker\">Plans</p><h2 class=\"h2\">Choose the record you need.</h2></div><p class=\"lead\">Annual prices are stored independently. Switching plans records a proration on the next invoice; cancellation takes effect at the chosen boundary.</p></div>{}</section><div class=\"app-grid mt-22\">{}</div>",
        notice,
        pending,
        current,
        provider,
        stats,
        invoices,
        plan_cards,
        methods
    );
    account_page("Plans & billing", "billing", auth, &content, cookies)
}

fn current_subscription(
    auth: &AuthSession,
    csrf: &str,
    data: &BillingData<'_>,
    subscription: &faktor_payments::PaymentSubscription,
    plan_id: &str,
    interval: BillingInterval,
) -> String {
    let plan_name = plan(plan_id).map(|plan| plan.name).unwrap_or(plan_id);
    let price = data
        .payments
        .catalogue_plan(plan_id)
        .map(|price| match interval {
            BillingInterval::Monthly => price.monthly_minor,
            BillingInterval::Annual => price.annual_minor,
        })
        .unwrap_or(0);
    let cancel_note = if subscription.cancel_at_period_end {
        "<div class=\"alert warn\"><p class=\"alert-title\">Cancellation scheduled.</p><p>The plan remains available until the recorded period end.</p></div>"
    } else {
        ""
    };
    let plan_options = PLANS
        .iter()
        .map(|plan| {
            let selected = if plan.id == plan_id { " selected" } else { "" };
            format!(
                "<option value=\"{}\"{}>{}</option>",
                plan.id, selected, plan.name
            )
        })
        .collect::<String>();
    let change = format!(
        "<form class=\"auth-form mt-22\" method=\"post\" action=\"/account/billing/change-plan/\">{}<div class=\"field-row\"><div class=\"field\"><label for=\"change_plan\">Plan</label><select id=\"change_plan\" name=\"plan_id\">{}</select></div><div class=\"field\"><label for=\"change_interval\">Interval</label><select id=\"change_interval\" name=\"interval\"><option value=\"monthly\"{}>Monthly</option><option value=\"annual\"{}>Annual</option></select></div></div><div class=\"field\"><label class=\"checkbox-field\"><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required> I understand the change applies immediately and any proration is recorded on the next invoice.</label></div><div class=\"form-actions\"><button class=\"btn btn-outline btn-small\" type=\"submit\">Change plan</button></div></form>",
        csrf_input(csrf),
        plan_options,
        if interval == BillingInterval::Monthly { " selected" } else { "" },
        if interval == BillingInterval::Annual { " selected" } else { "" }
    );
    let cancel = format!(
        "<form class=\"auth-form mt-22\" method=\"post\" action=\"/account/billing/cancel/\">{}<div class=\"field\"><label for=\"cancel_mode\">Cancellation</label><select id=\"cancel_mode\" name=\"at_period_end\"><option value=\"1\" selected>At period end</option><option value=\"0\">Immediately</option></select></div><div class=\"field\"><label class=\"checkbox-field\"><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required> I understand that cancellation denies new admissions; immediate cancellation denies them now.</label></div><div class=\"form-actions\"><button class=\"btn btn-outline btn-small\" type=\"submit\">Cancel subscription</button></div></form>",
        csrf_input(csrf)
    );
    let requested_by = format!(
        "<p class=\"notice\">Requested by {}. Billing actions are recorded under the signed-in organization.</p>",
        html_escape(auth.email.as_str())
    );
    format!(
        "<article class=\"panel span-12\"><div class=\"panel-head\"><div><p class=\"kicker\">Current subscription</p><h2>{} / {}</h2><p class=\"panel-sub\">Renews {}</p></div>{}</div>{}<div class=\"row-between\"><div><p class=\"stat\">{} / {}</p><p class=\"stat-label\">Recorded recurring amount</p></div><div><p class=\"stat\">{}</p><p class=\"stat-label\">Seats</p></div></div><dl class=\"kv mt-22\"><dt>Plan</dt><dd>{}</dd><dt>Interval</dt><dd>{}</dd><dt>Period</dt><dd>{} → {}</dd><dt>Cancel at period end</dt><dd>{}</dd></dl>{}{}{}</article>",
        html_escape(plan_name),
        html_escape(subscription.state.as_str()),
        date(subscription.current_period_end_ms),
        status_badge(subscription.state.as_str()),
        cancel_note,
        money(price, &subscription.currency),
        if interval == BillingInterval::Monthly { "monthly" } else { "annual" },
        subscription.quantity,
        html_escape(plan_id),
        if interval == BillingInterval::Monthly { "monthly" } else { "annual" },
        date(subscription.current_period_start_ms),
        date(subscription.current_period_end_ms),
        if subscription.cancel_at_period_end { "yes" } else { "no" },
        change,
        cancel,
        requested_by
    )
}

fn plan_cards(
    auth: &AuthSession,
    csrf: &str,
    data: &BillingData<'_>,
    has_subscription: bool,
) -> String {
    let _ = auth;
    let disabled = has_subscription || data.overview.provider.state != "available";
    PLANS
        .iter()
        .map(|plan| {
            let monthly = format!(
                "<form method=\"post\" action=\"/account/billing/checkout/\">{}<input type=\"hidden\" name=\"plan_id\" value=\"{}\"><input type=\"hidden\" name=\"interval\" value=\"monthly\"><input type=\"hidden\" name=\"request_id\" value=\"{}\"><button class=\"btn btn-outline btn-block btn-small\" type=\"submit\"{}>Choose monthly {}</button></form>",
                csrf_input(csrf),
                plan.id,
                html_escape(data.request_id),
                if disabled { " disabled" } else { "" },
                money(plan.monthly_minor, "EUR")
            );
            let annual = format!(
                "<form method=\"post\" action=\"/account/billing/checkout/\">{}<input type=\"hidden\" name=\"plan_id\" value=\"{}\"><input type=\"hidden\" name=\"interval\" value=\"annual\"><input type=\"hidden\" name=\"request_id\" value=\"{}\"><button class=\"btn btn-ink btn-block btn-small\" type=\"submit\"{}>Choose annual {}</button></form>",
                csrf_input(csrf),
                plan.id,
                html_escape(data.request_id),
                if disabled { " disabled" } else { "" },
                money(plan.annual_minor, "EUR")
            );
            let features = plan
                .features
                .iter()
                .map(|feature| format!("<li>{}</li>", html_escape(feature)))
                .collect::<String>();
            format!(
                "<article class=\"plan-card{}\"><p class=\"plan-eyebrow\">Plan / {}</p><h2>{}</h2><p class=\"price\">{} <small>/ month</small></p><p class=\"plan-desc\">{}</p><ul>{}</ul><div class=\"stack-sm mt-22\">{}{}</div></article>",
                if plan.id == "team" { " featured" } else { "" },
                html_escape(plan.id),
                plan.name,
                money(plan.monthly_minor, "EUR"),
                html_escape(plan.description),
                features,
                monthly,
                annual
            )
        })
        .collect::<String>()
}

fn invoice_rows(auth: &AuthSession, csrf: &str, invoices: &[Invoice]) -> String {
    if invoices.is_empty() {
        return "<article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Invoices</h2><p class=\"panel-sub\">No invoice has been issued.</p></div></div><div class=\"empty\"><strong>Checkout creates the first invoice.</strong>A plan selection never creates entitlement without a verified payment.</div></article>".to_string();
    }
    let rows = invoices
        .iter()
        .take(10)
        .map(|invoice| {
            let action = if invoice.status == faktor_payments::InvoiceStatus::Open {
                format!(
                    "<form class=\"inline-form\" method=\"post\" action=\"/account/billing/invoices/{}/pay/\">{}<button class=\"btn btn-outline btn-small\" type=\"submit\">Pay</button></form>",
                    html_escape(invoice.id.as_str()),
                    csrf_input(csrf)
                )
            } else {
                format!(
                    "<a href=\"/account/invoices/{}/\">View</a>",
                    html_escape(invoice.id.as_str())
                )
            };
            format!(
                "<tr><td><a href=\"/account/invoices/{}/\">{}</a></td><td>{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(invoice.id.as_str()),
                html_escape(invoice.number.as_str()),
                date(invoice.issued_ms),
                money(invoice.total_minor, &invoice.currency),
                status_badge(invoice.status.as_str()),
                action
            )
        })
        .collect::<String>();
    let _ = auth;
    format!(
        "<article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Invoices</h2><p class=\"panel-sub\">Open invoices show the exact outstanding amount.</p></div><a class=\"link-arrow\" href=\"/account/invoices/\">All invoices</a></div><div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Invoices</caption><thead><tr><th scope=\"col\">Invoice</th><th scope=\"col\">Issued</th><th scope=\"col\" class=\"num\">Total</th><th scope=\"col\">Status</th><th scope=\"col\">Action</th></tr></thead><tbody>{rows}</tbody></table></div></article>"
    )
}

pub(crate) fn payment_methods(
    csrf: &str,
    methods: &[PaymentMethodView],
    provider: &str,
    provider_state: &str,
) -> String {
    let list = if methods.is_empty() {
        "<div class=\"empty\"><strong>No stored payment method.</strong>Attach an opaque provider token below. Do not enter a card number or security code.</div>".to_string()
    } else {
        methods
            .iter()
            .map(|method| {
                let default = if method.is_default { status_badge("active") } else { String::new() };
                let remove = format!(
                    "<form class=\"inline-form\" method=\"post\" action=\"/account/billing/payment-methods/{}/remove/\">{}<label class=\"checkbox-field\"><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required> Confirm</label><button class=\"btn btn-outline btn-small\" type=\"submit\">Remove</button></form>",
                    html_escape(method.id.as_str()),
                    csrf_input(csrf)
                );
                format!(
                    "<div class=\"member-row\"><div class=\"who\"><strong>{} •••• {}</strong><span>{} / expires {:02}/{} / {}</span></div><div class=\"controls\">{}</div></div>",
                    html_escape(&method.brand),
                    html_escape(&method.last4),
                    html_escape(&method.provider),
                    method.exp_month,
                    method.exp_year,
                    default,
                    remove
                )
            })
            .collect::<String>()
    };
    let disabled = if provider_state != "available" {
        " disabled"
    } else {
        ""
    };
    let form = format!(
        "<form class=\"auth-form mt-22\" method=\"post\" action=\"/account/billing/payment-methods/add/\">{}<div class=\"field\"><label for=\"provider_token\">Opaque provider token</label><input id=\"provider_token\" name=\"provider_token\" class=\"code-input\" maxlength=\"512\" required autocomplete=\"off\"><p class=\"hint\">Starts with a provider prefix; 13–19 consecutive digits are refused.</p></div><div class=\"field-row\"><div class=\"field\"><label for=\"brand\">Brand</label><input id=\"brand\" name=\"brand\" maxlength=\"40\" required></div><div class=\"field\"><label for=\"last4\">Last four</label><input id=\"last4\" name=\"last4\" inputmode=\"numeric\" pattern=\"[0-9]{{4}}\" maxlength=\"4\" required></div><div class=\"field\"><label for=\"exp_month\">Expiry month</label><input id=\"exp_month\" name=\"exp_month\" inputmode=\"numeric\" type=\"number\" min=\"1\" max=\"12\" required></div><div class=\"field\"><label for=\"exp_year\">Expiry year</label><input id=\"exp_year\" name=\"exp_year\" inputmode=\"numeric\" type=\"number\" min=\"2026\" max=\"2099\" required></div></div><div class=\"form-actions\"><button class=\"btn btn-ink btn-small\" type=\"submit\"{}>Attach token</button></div></form>",
        csrf_input(csrf),
        disabled
    );
    format!(
        "<article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Payment methods</h2><p class=\"panel-sub\">Provider tokens only. Faktor never accepts a PAN, CVV or magnetic-stripe data.</p></div>{}</div><div class=\"table-wrap\">{}</div>{}<p class=\"notice mt-18\">Provider: {}. Stored token material is sealed by the billing vault; only brand and last four are rendered.</p></article>",
        status_badge(provider_state),
        list,
        form,
        html_escape(provider)
    )
}

pub fn checkout_return(
    auth: &AuthSession,
    csrf: &str,
    invoice: Option<&Invoice>,
    cookies: &[String],
) -> Response {
    let invoice = invoice.map(|invoice| {
        format!(
            "<div class=\"panel span-8\"><div class=\"panel-head\"><div><h2>Checkout return</h2><p class=\"panel-sub\">The browser has returned. Payment is only settled by a verified provider webhook.</p></div>{}</div><dl class=\"kv\"><dt>Invoice</dt><dd>{}</dd><dt>Total</dt><dd>{}</dd><dt>Paid</dt><dd>{}</dd><dt>Status</dt><dd>{}</dd></dl><div class=\"actions\"><a class=\"btn btn-ink btn-small\" href=\"/account/billing/\">Back to billing</a>{}</div></div>",
            status_badge(invoice.status.as_str()),
            html_escape(invoice.number.as_str()),
            money(invoice.total_minor, &invoice.currency),
            money(invoice.amount_paid_minor, &invoice.currency),
            status_badge(invoice.status.as_str()),
            if invoice.status == faktor_payments::InvoiceStatus::Open {
                format!(
                    "<form method=\"post\" action=\"/account/billing/invoices/{}/pay/\">{}<button class=\"btn btn-outline btn-small\" type=\"submit\">Retry checkout</button></form>",
                    html_escape(invoice.id.as_str()),
                    csrf_input(csrf)
                )
            } else {
                format!(
                    "<a class=\"btn btn-outline btn-small\" href=\"/account/invoices/{}/receipt/\">Receipt</a>",
                    html_escape(invoice.id.as_str())
                )
            }
        )
    }).unwrap_or_else(|| "<div class=\"panel span-8\"><div class=\"alert warn\"><p class=\"alert-title\">Invoice not found.</p><p>The checkout return did not identify an invoice in this organization.</p></div><a class=\"btn btn-outline btn-small\" href=\"/account/billing/\">Back to billing</a></div>".to_string());
    let _ = auth;
    account_page(
        "Checkout return",
        "billing",
        auth,
        &format!("<div class=\"app-grid\">{}</div>", invoice),
        cookies,
    )
}

pub fn local_checkout(
    auth: &AuthSession,
    csrf: &str,
    checkout_id: &str,
    invoice: &Invoice,
    cookies: &[String],
) -> Response {
    let content = format!(
        "<div class=\"app-grid\"><article class=\"panel span-8\"><div class=\"panel-head\"><div><p class=\"kicker\">Local provider / hosted checkout</p><h2>Test payment</h2><p class=\"panel-sub\">This page emulates the provider surface in local mode. No card data is requested or stored.</p></div>{}</div><dl class=\"kv\"><dt>Invoice</dt><dd>{}</dd><dt>Amount</dt><dd>{}</dd><dt>Checkout</dt><dd class=\"mono\">{}</dd></dl><p class=\"notice mt-18\">Press the button to deliver one signed local provider webhook. The billing service verifies the signature, timestamp and event id before settling the invoice.</p><form class=\"actions\" method=\"post\" action=\"/account/billing/local/{}/pay/\">{}<button class=\"btn btn-ochre\" type=\"submit\">Authorize local test payment</button><a class=\"btn btn-outline\" href=\"/account/billing/\">Cancel</a></form></article></div>",
        status_badge(invoice.status.as_str()),
        html_escape(invoice.number.as_str()),
        money(invoice.balance_minor(), &invoice.currency),
        html_escape(checkout_id),
        html_escape(checkout_id),
        csrf_input(csrf)
    );
    let _ = auth;
    account_page("Local checkout", "billing", auth, &content, cookies)
}
