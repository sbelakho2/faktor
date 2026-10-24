use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::AuthSession;
use crate::error::html_escape;

pub const APP_NAV: &[(&str, &str, &str)] = &[
    ("overview", "/account/", "Overview"),
    ("organization", "/account/organization/", "Organization"),
    ("billing", "/account/billing/", "Billing"),
    ("invoices", "/account/invoices/", "Invoices"),
    ("usage", "/account/usage/", "Usage"),
    ("security", "/account/security/", "Security"),
];

pub fn public_page(title: &str, content: &str, cookies: &[String]) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{} — Faktor</title><meta name=\"description\" content=\"Faktor account and billing.\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><link rel=\"stylesheet\" href=\"/css/site.css?v=20261024h\"></head><body>{}<header class=\"site-header\"><div class=\"header-inner\"><a class=\"brand\" href=\"/\" aria-label=\"Faktor home\">{}{}</a>{}</div></header><main id=\"main\">{}</main>{}</body></html>",
        html_escape(title),
        chrome(None),
        mark(),
        "<span class=\"wordmark\">faktor</span>",
        primary_nav(""),
        content,
        footer()
    );
    html_result(StatusCode::OK, html, cookies)
}

pub fn account_page(
    title: &str,
    active: &str,
    auth: &AuthSession,
    content: &str,
    cookies: &[String],
) -> Response {
    let nav = format!(
        "<header class=\"site-header\"><div class=\"header-inner\"><a class=\"brand\" href=\"/\" aria-label=\"Faktor home\">{}{}</a><div class=\"account-head\"><span class=\"signed-in-as\">{}</span><span class=\"account-mail\">{}</span></div></div></header>",
        mark(),
        "<span class=\"wordmark\">faktor</span>",
        html_escape(auth.organization_name.as_str()),
        html_escape(auth.email.as_str())
    );
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{} — Faktor</title><meta name=\"robots\" content=\"noindex,nofollow\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><link rel=\"stylesheet\" href=\"/css/site.css?v=20261024h\"></head><body class=\"app-body\">{}<a class=\"skip-link\" href=\"#main\">Skip to content</a>{}<main id=\"main\" class=\"app-shell\"><div class=\"app-topbar\"><div><p class=\"kicker\">{}</p><h1>{}</h1></div><div class=\"app-actions\">{}</div></div><nav class=\"app-nav\" aria-label=\"Account\">{}</nav>{}{}</main>{}</body></html>",
        html_escape(title),
        chrome(Some(auth)),
        nav,
        html_escape(auth.organization_name.as_str()),
        html_escape(title),
        account_actions(auth),
        app_nav(active, auth),
        verification_banner(auth),
        content,
        account_footer(auth)
    );
    html_result(StatusCode::OK, html, cookies)
}

fn chrome(auth: Option<&AuthSession>) -> String {
    match auth {
        Some(auth) => format!(
            "<div class=\"site-chrome\"><div class=\"chrome-inner\"><span class=\"chrome-brand\">Bel Consulting</span><span class=\"chrome-note\">{}</span></div></div>",
            html_escape(auth.email.as_str())
        ),
        None => "<div class=\"site-chrome\"><div class=\"chrome-inner\"><span class=\"chrome-brand\">Bel Consulting</span><span class=\"chrome-note\">Native engineering agent</span></div></div>".to_string(),
    }
}

pub fn primary_nav(active: &str) -> String {
    let links = [
        ("approach", "/approach/", "Approach"),
        ("editor", "/editor/", "In your editor"),
        ("documentation", "/documentation/", "Documentation"),
        ("identity", "/identity/", "Identity"),
    ];
    let items = links
        .iter()
        .map(|(key, href, label)| {
            let current = if *key == active {
                " aria-current=\"page\""
            } else {
                ""
            };
            format!("<a href=\"{href}\"{current}>{label}</a>")
        })
        .collect::<Vec<_>>()
        .join("");
    format!("<nav class=\"primary-nav\" aria-label=\"Primary\">{items}</nav>")
}

pub fn app_nav(active: &str, auth: &AuthSession) -> String {
    let mut items = APP_NAV
        .iter()
        .map(|(key, href, label)| {
            let current = if *key == active {
                " aria-current=\"page\""
            } else {
                ""
            };
            format!("<a href=\"{href}\"{current}>{label}</a>")
        })
        .collect::<Vec<_>>();
    if auth.is_platform_admin {
        items.push("<a href=\"/admin/\">Platform admin</a>".to_string());
    }
    items.join("")
}

fn account_actions(auth: &AuthSession) -> String {
    format!(
        "<span class=\"badge\">{}</span><form class=\"inline-form\" method=\"post\" action=\"/logout/\">{}</form>",
        html_escape(auth.role.as_str()),
        logout_form(auth)
    )
}

pub fn logout_form(auth: &AuthSession) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf\" value=\"{}\"><button type=\"submit\" class=\"btn btn-ink btn-small\">Sign out</button>",
        html_escape(auth.csrf.as_str())
    )
}

pub fn verification_banner(auth: &AuthSession) -> String {
    if auth.email_verified {
        return String::new();
    }
    let resend = format!(
        "<input type=\"hidden\" name=\"csrf\" value=\"{}\"><button type=\"submit\">Resend verification email</button>",
        html_escape(auth.csrf.as_str())
    );
    format!(
        "<div class=\"alert warn\" role=\"status\"><p class=\"alert-title\">Verify your email to use billing.</p><p>We sent a verification link to {}.</p><form class=\"inline-form\" method=\"post\" action=\"/account/verify-email/resend/\">{}</form></div>",
        html_escape(auth.email.as_str()),
        resend
    )
}

pub fn alert(kind: &str, title: &str, message: &str) -> String {
    format!(
        "<div class=\"alert {}\" role=\"status\"><p class=\"alert-title\">{}</p><p>{}</p></div>",
        html_escape(kind),
        html_escape(title),
        html_escape(message)
    )
}

pub fn footer() -> String {
    format!(
        "<footer class=\"site-footer\"><div class=\"footer-inner\"><div class=\"footer-brand\"><a class=\"brand brand-footer\" href=\"/\" aria-label=\"Faktor home\">{}{}</a><p class=\"footer-statement\">An engineering agent for changes you can inspect.</p></div><nav class=\"footer-links\" aria-label=\"Footer\"><a href=\"https://github.com/sbelakho2/faktor\">Source &amp; documentation <span aria-hidden=\"true\">↗</span></a><a href=\"/identity/\">Identity &amp; downloads</a><a href=\"/research/\">Research &amp; prior edition</a></nav><div class=\"footer-product\"><p>An independent Bel Consulting product.</p><p>Also in the family: <span class=\"footer-family\">KiwiCaptcha</span> · <span class=\"footer-family\">Kiwi CI</span></p></div></div></footer>",
        mark(),
        "<span class=\"wordmark\">faktor</span>"
    )
}

fn account_footer(auth: &AuthSession) -> String {
    format!(
        "<footer class=\"site-footer\"><div class=\"footer-inner\"><div class=\"footer-brand\"><p class=\"footer-statement\">An engineering agent for changes you can inspect.</p><p class=\"footer-small\">{}</p></div><nav class=\"footer-links\" aria-label=\"Account footer\"><div><h2 class=\"footer-heading\">Billing</h2><a href=\"/account/billing/\">Plans &amp; billing</a><a href=\"/account/invoices/\">Invoices</a><a href=\"/account/usage/\">Usage</a></div><div><h2 class=\"footer-heading\">Security</h2><a href=\"/account/security/\">Sessions &amp; password</a><a href=\"/account/organization/\">Organization</a></div><div><h2 class=\"footer-heading\">Legal</h2><a href=\"/terms/\">Terms</a><a href=\"/privacy/\">Privacy</a></div></nav></div><div class=\"footer-base\"><span>{}</span><span>No JavaScript. Forms only.</span></div></footer>",
        html_escape(auth.organization_name.as_str()),
        html_escape(auth.email.as_str())
    )
}

fn mark() -> &'static str {
    "<svg class=\"mark\" viewBox=\"0 0 64 64\" width=\"56\" height=\"56\" aria-hidden=\"true\" focusable=\"false\"><path fill=\"currentColor\" d=\"M5 2h55v16H32v12h23v16H32v28H5z\"/></svg>"
}

pub fn html_result(status: StatusCode, html: String, cookies: &[String]) -> Response {
    let mut response = (status, html).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

pub fn see_other(location: &str, cookies: &[String]) -> Response {
    let mut response = (StatusCode::SEE_OTHER, "").into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

pub fn text_response(status: StatusCode, content_type: &'static str, body: String) -> Response {
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn hidden(name: &str, value: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
        html_escape(name),
        html_escape(value)
    )
}

pub fn csrf_input(secret: &str) -> String {
    hidden("csrf", secret)
}
