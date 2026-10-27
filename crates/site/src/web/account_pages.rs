use axum::response::Response;
use faktor_payments::{Invoice, PaymentsOverview};

use crate::auth::{AuthSession, MemberRow, MembershipRow, Role, SessionRow};
use crate::error::html_escape;
use crate::web::layout::{account_page, csrf_input};
use crate::web::{date, datetime, money, status_badge};

#[allow(clippy::too_many_arguments)]
pub fn overview(
    auth: &AuthSession,
    csrf: &str,
    display_name: &str,
    memberships: &[MembershipRow],
    overview: &PaymentsOverview,
    invoices: &[Invoice],
    notice_message: Option<&str>,
    cookies: &[String],
) -> Response {
    let notice = notice_message
        .map(|message| crate::web::alert("ok", "Account updated", message))
        .unwrap_or_default();
    let plan = overview
        .subscription
        .as_ref()
        .map(|subscription| subscription.plan_id.clone())
        .unwrap_or_else(|| "No plan".to_string());
    let state = overview
        .subscription
        .as_ref()
        .map(|subscription| subscription.state.as_str())
        .unwrap_or("inactive");
    let outstanding = money(overview.outstanding_minor, "EUR");
    let billing = format!(
        "<article class=\"panel span-7\"><div class=\"panel-head\"><div><h2>Plan and payment state</h2><p class=\"panel-sub\">Provider truth is upstream; this is the Faktor billing record.</p></div>{}</div><div class=\"row-between\"><div><p class=\"stat\">{}</p><p class=\"stat-label\">Current plan</p></div><div><p class=\"stat\">{}</p><p class=\"stat-label\">Outstanding</p></div></div><dl class=\"kv mt-22\"><dt>State</dt><dd>{}</dd><dt>Invoices</dt><dd>{} recorded / {} open</dd><dt>Payment methods</dt><dd>{}</dd></dl><div class=\"actions\"><a class=\"btn btn-ink btn-small\" href=\"/account/billing/\">Open billing</a><a class=\"btn btn-outline btn-small\" href=\"/account/invoices/\">Invoices</a></div></article>",
        status_badge(state),
        html_escape(&plan),
        html_escape(&outstanding),
        status_badge(state),
        overview.invoice_count,
        overview.open_invoice_count,
        overview.payment_method_count
    );
    let profile = format!(
        "<article class=\"panel span-5\"><div class=\"panel-head\"><div><h2>Your account</h2><p class=\"panel-sub\">Identity attached to every session.</p></div></div><dl class=\"kv\"><dt>Email</dt><dd>{}</dd><dt>Display name</dt><dd>{}</dd><dt>Email state</dt><dd>{}</dd><dt>Role here</dt><dd>{}</dd></dl><form class=\"auth-form mt-22\" method=\"post\" action=\"/account/profile/\">{}<div class=\"field\"><label for=\"display_name\">Display name</label><input id=\"display_name\" name=\"display_name\" value=\"{}\" maxlength=\"80\" required></div><div class=\"form-actions\"><button class=\"btn btn-ink btn-small\" type=\"submit\">Update profile</button></div></form></article>",
        html_escape(auth.email.as_str()),
        html_escape(display_name),
        if auth.email_verified { status_badge("active") } else { status_badge("pending") },
        html_escape(auth.role.as_str()),
        csrf_input(csrf),
        html_escape(display_name)
    );
    let invoice_table = recent_invoices(invoices);
    let orgs = memberships
        .iter()
        .map(|membership| {
            let current = membership.organization_id == auth.organization_id;
            format!(
                "<div class=\"member-row\"><div class=\"who\"><strong>{}</strong><span>{} / {}</span></div>{}</div>",
                html_escape(membership.organization_name.as_str()),
                html_escape(membership.role.as_str()),
                html_escape(membership.organization_id.as_str()),
                if current { status_badge("active") } else { String::new() }
            )
        })
        .collect::<String>();
    let workspace = format!(
        "<article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Workspace access</h2><p class=\"panel-sub\">Every account is scoped to one active organization per session.</p></div><a class=\"link-arrow\" href=\"/account/organization/\">Manage organization</a></div>{}{}</article>",
        orgs,
        organization_switch(auth, csrf, memberships)
    );
    let admin = if auth.is_platform_admin {
        "<div class=\"alert warn\"><p class=\"alert-title\">Platform administration is available.</p><p><a href=\"/admin/\">Open the platform admin surface</a>. Administrative actions are audited.</p></div>".to_string()
    } else {
        String::new()
    };
    let content = format!(
        "{}{}<div class=\"app-grid\">{}{}</div><div class=\"app-grid mt-22\">{}</div><div class=\"app-grid mt-22\"><article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Recent invoices</h2><p class=\"panel-sub\">Exact minor-unit amounts. A paid invoice has a receipt.</p></div><a class=\"link-arrow\" href=\"/account/invoices/\">All invoices</a></div>{}</article></div>",
        notice, admin, profile, billing, workspace, invoice_table
    );
    account_page("Account", "overview", auth, &content, cookies)
}

fn organization_switch(auth: &AuthSession, csrf: &str, memberships: &[MembershipRow]) -> String {
    if memberships.len() <= 1 {
        return String::new();
    }
    let options = memberships
        .iter()
        .map(|membership| {
            let selected = if membership.organization_id == auth.organization_id {
                " selected"
            } else {
                ""
            };
            format!(
                "<option value=\"{}\"{}>{}</option>",
                html_escape(membership.organization_id.as_str()),
                selected,
                html_escape(membership.organization_name.as_str())
            )
        })
        .collect::<String>();
    format!(
        "<form class=\"account-switch mt-18\" method=\"post\" action=\"/account/organization/switch/\">{}<label class=\"sr-only\" for=\"org_id\">Active organization</label><select id=\"org_id\" name=\"org_id\">{}</select><button class=\"btn btn-outline btn-small\" type=\"submit\">Switch workspace</button></form>",
        csrf_input(csrf),
        options
    )
}

fn recent_invoices(invoices: &[Invoice]) -> String {
    if invoices.is_empty() {
        return "<div class=\"empty\"><strong>No invoices yet.</strong>Choose a plan to create the first invoice and hosted checkout.</div>".to_string();
    }
    let rows = invoices
        .iter()
        .take(6)
        .map(|invoice| {
            let id = html_escape(invoice.id.as_str());
            let primary = if invoice.status == faktor_payments::InvoiceStatus::Paid {
                format!("<a href=\"/account/invoices/{id}/receipt/\">Receipt</a>")
            } else {
                format!("<a href=\"/account/invoices/{id}/\">Open</a>")
            };
            let action = format!(
                "{primary} · <a href=\"/account/invoices/{id}/download/?format=csv\">CSV</a>"
            );
            format!(
                "<tr><td><a href=\"/account/invoices/{id}/\">{}</a></td><td>{}</td><td>{}</td><td class=\"num\">{}</td><td>{action}</td></tr>",
                html_escape(invoice.number.as_str()),
                date(invoice.issued_ms),
                money(invoice.total_minor, &invoice.currency),
                status_badge(invoice.status.as_str())
            )
        })
        .collect::<String>();
    format!(
        "<div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Recent invoices</caption><thead><tr><th scope=\"col\">Invoice</th><th scope=\"col\">Issued</th><th scope=\"col\">Total</th><th scope=\"col\" class=\"num\">Status</th><th scope=\"col\">Action</th></tr></thead><tbody>{}</tbody></table></div>",
        rows
    )
}

pub struct OrganizationData<'a> {
    pub members: &'a [MemberRow],
    pub memberships: &'a [MembershipRow],
}

pub fn organization(
    auth: &AuthSession,
    csrf: &str,
    data: &OrganizationData<'_>,
    cookies: &[String],
) -> Response {
    let can_manage = auth.role.can_manage_members();
    let members = data
        .members
        .iter()
        .map(|member| {
            let controls = if can_manage {
                format!(
                    "<form class=\"inline-form\" method=\"post\" action=\"/account/organization/members/update/\">{}<input type=\"hidden\" name=\"user_id\" value=\"{}\"><label class=\"sr-only\" for=\"role-{}\">Role for {}</label><select id=\"role-{}\" name=\"role\">{}</select><button class=\"btn btn-outline btn-small\" type=\"submit\">Update</button></form><form class=\"inline-form\" method=\"post\" action=\"/account/organization/members/remove/\">{}<input type=\"hidden\" name=\"user_id\" value=\"{}\"><button class=\"btn btn-outline btn-small\" type=\"submit\">Remove</button></form>",
                    csrf_input(csrf),
                    html_escape(member.user_id.as_str()),
                    html_escape(member.user_id.as_str()),
                    html_escape(member.email.as_str()),
                    html_escape(member.user_id.as_str()),
                    role_options(member.role),
                    csrf_input(csrf),
                    html_escape(member.user_id.as_str())
                )
            } else {
                status_badge(member.role.as_str())
            };
            format!(
                "<div class=\"member-row\"><div class=\"who\"><strong>{}</strong><span>{} / joined {}</span></div><div class=\"controls\">{}</div></div>",
                html_escape(member.display_name.as_str()),
                html_escape(member.email.as_str()),
                date(member.created_ms),
                controls
            )
        })
        .collect::<String>();
    let add = if can_manage {
        format!(
            "<form class=\"auth-form mt-22\" method=\"post\" action=\"/account/organization/members/add/\">{}<div class=\"field-row\"><div class=\"field\"><label for=\"member_email\">Existing account email</label><input id=\"member_email\" name=\"email\" type=\"email\" maxlength=\"254\" required></div><div class=\"field\"><label for=\"member_role\">Role</label><select id=\"member_role\" name=\"role\"><option value=\"member\">Member</option><option value=\"admin\">Admin</option><option value=\"owner\">Owner</option></select></div></div><div class=\"form-actions\"><button class=\"btn btn-ink btn-small\" type=\"submit\">Add member</button></div></form>",
            csrf_input(csrf)
        )
    } else {
        "<p class=\"notice\">Only an owner can add, remove or re-role members.</p>".to_string()
    };
    let rename = if can_manage {
        format!(
            "<form class=\"auth-form mt-18\" method=\"post\" action=\"/account/organization/name/\">{}<div class=\"field\"><label for=\"org_name\">Organization name</label><input id=\"org_name\" name=\"name\" value=\"{}\" maxlength=\"120\" required></div><div class=\"form-actions\"><button class=\"btn btn-outline btn-small\" type=\"submit\">Rename</button></div></form>",
            csrf_input(csrf),
            html_escape(auth.organization_name.as_str())
        )
    } else {
        String::new()
    };
    let switch = organization_switch(auth, csrf, data.memberships);
    let content = format!(
        "<div class=\"app-grid\"><article class=\"panel span-8\"><div class=\"panel-head\"><div><h2>Members</h2><p class=\"panel-sub\">Membership is scoped to this organization. Removing a member revokes their sessions in it.</p></div>{}</div>{}{}</article><article class=\"panel span-4\"><div class=\"panel-head\"><div><h2>Workspace</h2><p class=\"panel-sub\">At least one owner is always retained.</p></div></div><dl class=\"kv\"><dt>Name</dt><dd>{}</dd><dt>Id</dt><dd class=\"mono\">{}</dd><dt>Your role</dt><dd>{}</dd><dt>Members</dt><dd>{}</dd></dl>{}{}</article></div>",
        status_badge(auth.role.as_str()),
        members,
        add,
        html_escape(auth.organization_name.as_str()),
        html_escape(auth.organization_id.as_str()),
        html_escape(auth.role.as_str()),
        data.members.len(),
        rename,
        switch
    );
    account_page("Organization", "organization", auth, &content, cookies)
}

fn role_options(selected: Role) -> String {
    [Role::Member, Role::Admin, Role::Owner]
        .iter()
        .map(|role| {
            let selected = if *role == selected { " selected" } else { "" };
            format!(
                "<option value=\"{}\"{}>{}</option>",
                role.as_str(),
                selected,
                role.as_str()
            )
        })
        .collect()
}

pub struct SecurityData<'a> {
    pub sessions: &'a [SessionRow],
}

pub fn security(
    auth: &AuthSession,
    csrf: &str,
    data: &SecurityData<'_>,
    cookies: &[String],
) -> Response {
    let sessions = data
        .sessions
        .iter()
        .map(|session| {
            let current = if session.id == auth.session_id {
                status_badge("active")
            } else {
                String::new()
            };
            format!(
                "<tr><td class=\"mono\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(&session.id.chars().take(12).collect::<String>()),
                datetime(session.last_seen_ms),
                date(session.expires_ms),
                current
            )
        })
        .collect::<String>();
    let content = format!(
        "<div class=\"app-grid\"><article class=\"panel span-7\"><div class=\"panel-head\"><div><h2>Password</h2><p class=\"panel-sub\">Changing it revokes every other session.</p></div></div><form class=\"auth-form\" method=\"post\" action=\"/account/security/password/\">{}<div class=\"field\"><label for=\"current_password\">Current password</label><input id=\"current_password\" name=\"current_password\" type=\"password\" autocomplete=\"current-password\" required maxlength=\"256\"></div><div class=\"field\"><label for=\"new_password\">New password</label><input id=\"new_password\" name=\"new_password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"256\" required></div><div class=\"field\"><label for=\"confirm_password\">Confirm new password</label><input id=\"confirm_password\" name=\"confirm_password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"256\" required></div><div class=\"form-actions\"><button class=\"btn btn-ink\" type=\"submit\">Change password</button></div></form></article><article class=\"panel span-5\"><div class=\"panel-head\"><div><h2>Revoke access</h2><p class=\"panel-sub\">Use this after losing a device.</p></div></div><p>Signing out revokes the current session. Revoking all other sessions leaves this browser signed in.</p><form class=\"actions\" method=\"post\" action=\"/account/security/sessions/revoke/\">{}<button class=\"btn btn-outline\" type=\"submit\">Revoke all other sessions</button></form></article><article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Active sessions</h2><p class=\"panel-sub\">Token hashes are stored; raw session secrets are not.</p></div></div><div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Active sessions</caption><thead><tr><th scope=\"col\">Session</th><th scope=\"col\">Last seen</th><th scope=\"col\">Expires</th><th scope=\"col\">Current</th></tr></thead><tbody>{}</tbody></table></div></article></div>",
        csrf_input(csrf),
        csrf_input(csrf),
        sessions
    );
    account_page("Security", "security", auth, &content, cookies)
}

pub struct AdminData<'a> {
    pub users: &'a [crate::auth::AdminUserRow],
    pub orgs: &'a [crate::auth::AdminOrgRow],
}

pub fn admin(
    auth: &AuthSession,
    csrf: &str,
    data: &AdminData<'_>,
    message: Option<&str>,
    cookies: &[String],
) -> Response {
    let users = data
        .users
        .iter()
        .map(|user| {
            let action = if user.id == auth.user_id {
                status_badge("active")
            } else {
                format!(
                    "<form class=\"inline-form\" method=\"post\" action=\"/admin/users/disable/\">{}<input type=\"hidden\" name=\"user_id\" value=\"{}\"><input type=\"hidden\" name=\"disabled\" value=\"{}\"><button class=\"inline-form\" type=\"submit\">{}</button></form>",
                    csrf_input(csrf),
                    html_escape(user.id.as_str()),
                    if user.disabled { "0" } else { "1" },
                    if user.disabled { "Enable" } else { "Disable" }
                )
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(user.email.as_str()),
                html_escape(user.display_name.as_str()),
                if user.email_verified { "yes" } else { "no" },
                if user.is_platform_admin { "admin" } else { "user" },
                if user.disabled { status_badge("failed") } else { status_badge("active") },
                action
            )
        })
        .collect::<String>();
    let orgs = data
        .orgs
        .iter()
        .map(|org| {
            format!(
                "<tr><td>{}</td><td class=\"mono\">{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(org.name.as_str()),
                html_escape(org.id.as_str()),
                org.member_count,
                date(org.created_ms)
            )
        })
        .collect::<String>();
    let notice = message
        .map(|message| crate::web::alert("ok", "Admin action recorded", message))
        .unwrap_or_default();
    let org_options = data
        .orgs
        .iter()
        .map(|org| {
            format!(
                "<option value=\"{}\">{}</option>",
                html_escape(org.id.as_str()),
                html_escape(org.name.as_str())
            )
        })
        .collect::<String>();
    let usage_form = format!(
        "<article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Record a usage invoice</h2><p class=\"panel-sub\">Usage is an operator-authorized invoice line. There is no client-side meter.</p></div></div><form class=\"auth-form\" method=\"post\" action=\"/admin/usage/\">{}<div class=\"field\"><label for=\"usage_org\">Organization</label><select id=\"usage_org\" name=\"organization_id\" required>{}</select></div><div class=\"field\"><label for=\"usage_description\">Description</label><input id=\"usage_description\" name=\"description\" maxlength=\"200\" required></div><div class=\"field-row\"><div class=\"field\"><label for=\"usage_quantity\">Quantity</label><input id=\"usage_quantity\" name=\"quantity\" type=\"number\" min=\"1\" max=\"1000000\" required></div><div class=\"field\"><label for=\"usage_unit\">Unit amount (minor)</label><input id=\"usage_unit\" name=\"unit_amount_minor\" type=\"number\" min=\"0\" max=\"100000000\" required></div></div><div class=\"form-actions\"><button class=\"btn btn-ink btn-small\" type=\"submit\">Issue usage invoice</button></div></form></article>",
        csrf_input(csrf),
        org_options
    );
    let content = format!(
        "{}<div class=\"app-grid\"><article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Organizations</h2><p class=\"panel-sub\">Read-only tenant inventory. Billing records stay scoped to each organization.</p></div></div><div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Organizations</caption><thead><tr><th scope=\"col\">Name</th><th scope=\"col\">Id</th><th scope=\"col\">Members</th><th scope=\"col\">Created</th></tr></thead><tbody>{}</tbody></table></div></article><article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Users</h2><p class=\"panel-sub\">Disabling an account revokes every session. You cannot disable yourself.</p></div></div><div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Users</caption><thead><tr><th scope=\"col\">Email</th><th scope=\"col\">Name</th><th scope=\"col\">Verified</th><th scope=\"col\">Flags</th><th scope=\"col\">State</th><th scope=\"col\">Action</th></tr></thead><tbody>{}</tbody></table></div></article>{}</div>",
        notice, orgs, users, usage_form
    );
    account_page("Platform admin", "admin", auth, &content, cookies)
}

pub fn invoices_page(
    auth: &AuthSession,
    csrf: &str,
    invoices: &[Invoice],
    next_cursor: Option<&str>,
    since: Option<&str>,
    cookies: &[String],
) -> Response {
    let rows = invoices
        .iter()
        .map(|invoice| {
            format!(
                "<tr><td><a href=\"/account/invoices/{}/\">{}</a></td><td>{}</td><td>{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(invoice.id.as_str()),
                html_escape(invoice.number.as_str()),
                date(invoice.issued_ms),
                date(invoice.due_ms),
                money(invoice.total_minor, &invoice.currency),
                status_badge(invoice.status.as_str()),
                if invoice.status == faktor_payments::InvoiceStatus::Paid {
                    format!("<a href=\"/account/invoices/{}/receipt/\">Receipt</a>", html_escape(invoice.id.as_str()))
                } else {
                    format!("<a href=\"/account/invoices/{}/\">Open</a>", html_escape(invoice.id.as_str()))
                }
            )
        })
        .collect::<String>();
    let bound_note = if next_cursor.is_some() {
        " Showing the latest 100 invoices on this page; continue with the older-invoice link."
    } else if since.is_some() {
        " Showing the oldest 100 invoices in this cursor page."
    } else {
        ""
    };
    let mut pager = String::new();
    if since.is_some() {
        pager.push_str(
            "<a class=\"btn btn-outline btn-small\" href=\"/account/invoices/\">Newest invoices</a>",
        );
    }
    if let Some(cursor) = next_cursor {
        pager.push_str(&format!(
            "<a class=\"btn btn-outline btn-small\" href=\"/account/invoices/?since={}\">Older invoices</a>",
            html_escape(cursor)
        ));
    }
    let content = format!(
        "<div class=\"actions no-print\"><a class=\"btn btn-outline btn-small\" href=\"/account/invoices/export/?format=csv\">Export CSV</a><a class=\"btn btn-outline btn-small\" href=\"/account/invoices/export/?format=json\">Export JSON</a>{}</div><div class=\"panel span-12 mt-22\"><div class=\"panel-head\"><div><h2>Invoices and receipts</h2><p class=\"panel-sub\">Every amount is the recorded exact minor-unit value.{}.</p></div></div>{}</div>",
        pager,
        bound_note,
        if rows.is_empty() {
            "<div class=\"empty\"><strong>No invoices.</strong>Start a plan checkout to create an invoice.</div>".to_string()
        } else {
            format!("<div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Invoices</caption><thead><tr><th scope=\"col\">Invoice</th><th scope=\"col\">Issued</th><th scope=\"col\">Due</th><th scope=\"col\" class=\"num\">Total</th><th scope=\"col\">Status</th><th scope=\"col\">Receipt</th></tr></thead><tbody>{rows}</tbody></table></div>")
        }
    );
    let content = format!("<div class=\"app-grid\">{}</div>", content);
    let _ = csrf;
    account_page("Invoices", "invoices", auth, &content, cookies)
}

pub fn invoice_detail(
    auth: &AuthSession,
    csrf: &str,
    invoice: &Invoice,
    payment_info: Option<&str>,
    cookies: &[String],
) -> Response {
    let lines = invoice
        .lines
        .iter()
        .map(|line| {
            format!(
                "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                html_escape(line.description.as_str()),
                line.quantity,
                money(line.unit_amount_minor, &invoice.currency),
                money(line.amount_minor, &invoice.currency)
            )
        })
        .collect::<String>();
    let download = format!(
        "<a class=\"btn btn-outline\" href=\"/account/invoices/{}/download/?format=csv\">Download CSV</a>",
        html_escape(invoice.id.as_str())
    );
    let actions = if invoice.status == faktor_payments::InvoiceStatus::Open {
        format!(
            "<div class=\"actions no-print\"><form method=\"post\" action=\"/account/billing/invoices/{}/pay/\">{}<button class=\"btn btn-ink\" type=\"submit\">Pay {}</button></form>{}</div>",
            html_escape(invoice.id.as_str()),
            csrf_input(csrf),
            money(invoice.balance_minor(), &invoice.currency),
            download
        )
    } else if invoice.status == faktor_payments::InvoiceStatus::Paid {
        format!(
            "<div class=\"actions no-print\"><a class=\"btn btn-outline\" href=\"/account/invoices/{}/receipt/\">Open receipt</a>{}</div>",
            html_escape(invoice.id.as_str()),
            download
        )
    } else {
        format!("<div class=\"actions no-print\">{download}</div>")
    };
    let content = format!(
        "{}<div class=\"app-grid\"><article class=\"panel span-8\"><div class=\"panel-head\"><div><h2>{}</h2><p class=\"panel-sub\">Issued {} / due {}</p></div>{}</div><div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Invoice lines</caption><thead><tr><th scope=\"col\">Description</th><th scope=\"col\" class=\"num\">Qty</th><th scope=\"col\" class=\"num\">Unit</th><th scope=\"col\" class=\"num\">Amount</th></tr></thead><tbody>{}</tbody></table></div><div class=\"invoice-total\"><div><span>Subtotal</span><span>{}</span></div><div><span>Tax</span><span>{}</span></div><div><span>Paid</span><span>{}</span></div><div><span>Refunded</span><span>{}</span></div><div class=\"total\"><span>Total</span><span>{}</span></div></div>{}</article><aside class=\"panel span-4\"><div class=\"panel-head\"><div><h2>Record state</h2></div></div><dl class=\"kv\"><dt>Status</dt><dd>{}</dd><dt>Invoice</dt><dd class=\"mono\">{}</dd><dt>Subscription</dt><dd class=\"mono\">{}</dd><dt>Tax basis</dt><dd>{}</dd><dt>Payment basis</dt><dd>{}</dd></dl></aside></div>",
        payment_info.map(|message| crate::web::alert("ok", "Provider update", message)).unwrap_or_default(),
        html_escape(invoice.number.as_str()),
        date(invoice.issued_ms),
        date(invoice.due_ms),
        status_badge(invoice.status.as_str()),
        lines,
        money(invoice.subtotal_minor, &invoice.currency),
        money(invoice.tax_minor, &invoice.currency),
        money(invoice.amount_paid_minor, &invoice.currency),
        money(invoice.amount_refunded_minor, &invoice.currency),
        money(invoice.total_minor, &invoice.currency),
        actions,
        status_badge(invoice.status.as_str()),
        html_escape(invoice.id.as_str()),
        html_escape(invoice.subscription_id.as_deref().unwrap_or("unattached")),
        match invoice.tax_jurisdiction.as_deref() {
            Some(jurisdiction) => format!(
                "{} · {}",
                html_escape(jurisdiction),
                if invoice.tax_inclusive {
                    "inclusive"
                } else {
                    "exclusive"
                }
            ),
            None => (if invoice.tax_inclusive {
                "inclusive"
            } else {
                "exclusive"
            })
            .to_string(),
        },
        if invoice.amount_paid_minor > 0 { "Provider webhook recorded" } else { "Awaiting verified payment" }
    );
    account_page("Invoice", "invoices", auth, &content, cookies)
}

pub fn receipt(auth: &AuthSession, invoice: &Invoice, cookies: &[String]) -> Response {
    let lines = invoice
        .lines
        .iter()
        .map(|line| {
            format!(
                "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                html_escape(line.description.as_str()),
                line.quantity,
                money(line.amount_minor, &invoice.currency)
            )
        })
        .collect::<String>();
    let paid = invoice
        .paid_ms
        .map(date)
        .unwrap_or_else(|| "Unavailable".into());
    let content = format!(
        "<article class=\"panel receipt\"><header class=\"receipt-head\"><div><p class=\"kicker\">Faktor / Receipt</p><h1>{}</h1></div><div class=\"receipt-meta\">Paid {}<br>Receipt {}</div></header><div class=\"receipt-parties\"><div><h2>From</h2><p>Bel Consulting<br>Faktor<br>Billing contact: support channel in your order</p></div><div><h2>Account</h2><p>{}<br>{}</p></div></div><div class=\"table-wrap receipt-lines\"><table class=\"app-table\"><caption class=\"sr-only\">Receipt lines</caption><thead><tr><th scope=\"col\">Description</th><th scope=\"col\" class=\"num\">Qty</th><th scope=\"col\" class=\"num\">Amount</th></tr></thead><tbody>{}</tbody></table></div><div class=\"invoice-total\"><div><span>Paid</span><span>{}</span></div><div><span>Refunded</span><span>{}</span></div><div><span>Tax</span><span>{}</span></div><div class=\"total\"><span>Total</span><span>{}</span></div></div><p class=\"receipt-foot\">Invoice {} / Subscription {} / Exact minor units {} / This receipt records the Faktor billing state. Provider references are never rendered here.</p><p class=\"notice no-print\">Print this page from the browser’s File menu. <a href=\"/account/invoices/\">Back to invoices</a></p></article>",
        html_escape(invoice.number.as_str()),
        paid,
        html_escape(invoice.number.as_str()),
        html_escape(auth.email.as_str()),
        html_escape(auth.organization_name.as_str()),
        lines,
        money(invoice.amount_paid_minor, &invoice.currency),
        money(invoice.amount_refunded_minor, &invoice.currency),
        money(invoice.tax_minor, &invoice.currency),
        money(invoice.total_minor, &invoice.currency),
        html_escape(invoice.id.as_str()),
        html_escape(invoice.subscription_id.as_deref().unwrap_or("unattached")),
        invoice.total_minor
    );
    account_page("Receipt", "invoices", auth, &content, cookies)
}

pub struct UsageSummary {
    pub descriptions: Vec<(String, i64, i64)>,
    pub invoices: usize,
    pub seats: u32,
    /// Number of invoices the page sampled (the source is bounded).
    pub scanned_invoices: usize,
}

pub fn usage_page(auth: &AuthSession, summary: &UsageSummary, cookies: &[String]) -> Response {
    let rows = if summary.descriptions.is_empty() {
        "<div class=\"empty\"><strong>No usage lines recorded.</strong>Usage is bounded by the invoice record. Platform administrators can issue a usage invoice; no client-side counter is trusted.</div>".to_string()
    } else {
        let rows = summary
            .descriptions
            .iter()
            .map(|(description, quantity, amount)| {
                format!(
                    "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                    html_escape(description),
                    quantity,
                    money(*amount, "EUR")
                )
            })
            .collect::<String>();
        format!("<div class=\"table-wrap\"><table class=\"app-table\"><caption class=\"sr-only\">Usage lines</caption><thead><tr><th scope=\"col\">Metric</th><th scope=\"col\" class=\"num\">Quantity</th><th scope=\"col\" class=\"num\">Amount</th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    let source_note = if summary.scanned_invoices >= 200 {
        "Aggregated from the newest 200 invoices (the same bound as the ledger page). Older invoice rows remain available through the bounded exports."
    } else {
        "Aggregated from every issued invoice in this organization. No unbounded meter is kept on the website."
    };
    let content = format!(
        "<div class=\"app-grid\"><article class=\"panel span-12\"><div class=\"panel-head\"><div><h2>Recorded usage</h2><p class=\"panel-sub\">{}</p></div></div><dl class=\"kv\"><dt>Seats</dt><dd>{}</dd><dt>Invoices with usage</dt><dd>{}</dd><dt>Source</dt><dd>Invoice line items of kind usage</dd></dl>{}</article></div>",
        source_note,
        summary.seats,
        summary.invoices,
        rows
    );
    account_page("Usage", "usage", auth, &content, cookies)
}
