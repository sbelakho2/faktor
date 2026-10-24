use axum::response::Response;

use crate::error::html_escape;
use crate::web::layout::{csrf_input, hidden, public_page};

fn auth_shell(kicker: &str, title: &str, intro: &str, body: &str) -> String {
    format!(
        "<section class=\"auth-shell\"><header class=\"auth-head\"><p class=\"kicker\">{}</p><h1>{}</h1><p>{}</p></header>{}</section>",
        html_escape(kicker),
        html_escape(title),
        html_escape(intro),
        body
    )
}

fn error_block(error: Option<&str>) -> String {
    match error {
        Some(message) => format!(
            "<div class=\"form-error\" role=\"alert\">{}</div>",
            html_escape(message)
        ),
        None => String::new(),
    }
}

pub fn login_page(
    csrf: &str,
    next: &str,
    error: Option<&str>,
    notice: Option<&str>,
    cookies: &[String],
) -> Response {
    let notice_block = notice
        .map(|message| {
            format!(
                "<div class=\"form-success\" role=\"status\">{}</div>",
                html_escape(message)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<form class=\"auth-form\" method=\"post\" action=\"/login/\" novalidate>{}{}{}{}<div class=\"field\"><label for=\"email\">Email</label><input id=\"email\" name=\"email\" type=\"email\" autocomplete=\"username\" required maxlength=\"254\"></div><div class=\"field\"><label for=\"password\">Password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required maxlength=\"256\"></div><div class=\"form-actions\"><button class=\"btn btn-ink\" type=\"submit\">Sign in</button><a class=\"muted small\" href=\"/forgot-password/\">Forgot password?</a></div></form><p class=\"form-meta\">New to Faktor? <a href=\"/signup/\">Create an account</a>. Billing requires a verified email address.</p>",
        csrf_input(csrf),
        hidden("next", next),
        notice_block,
        error_block(error)
    );
    public_page(
        "Sign in",
        &auth_shell(
            "Account / Sign in",
            "Sign in.",
            "Use the email and password attached to your Faktor account.",
            &body,
        ),
        cookies,
    )
}

pub fn signup_page(
    csrf: &str,
    email: &str,
    name: &str,
    plan: &str,
    error: Option<&str>,
    cookies: &[String],
) -> Response {
    let plan_note = if plan.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"form-success\">Selected plan: {}. You can review it before checkout.</p>",
            html_escape(plan)
        )
    };
    let body = format!(
        "<form class=\"auth-form\" method=\"post\" action=\"/signup/\" novalidate>{}<div class=\"field-row\"><div class=\"field\"><label for=\"name\">Display name</label><input id=\"name\" name=\"name\" type=\"text\" autocomplete=\"name\" required maxlength=\"80\" value=\"{}\"></div><div class=\"field\"><label for=\"email\">Email</label><input id=\"email\" name=\"email\" type=\"email\" autocomplete=\"email\" required maxlength=\"254\" value=\"{}\"></div></div>{}{}<div class=\"field\"><label for=\"password\">Password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"256\" required aria-describedby=\"password-hint\"><p class=\"hint\" id=\"password-hint\">12–256 bytes. Store it in your password manager.</p></div><div class=\"field\"><label for=\"confirm\">Confirm password</label><input id=\"confirm\" name=\"confirm\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"256\" required></div><label class=\"checkbox-field\"><input type=\"checkbox\" name=\"accept_terms\" value=\"1\" required><span>I agree to the <a href=\"/terms/\">terms</a> and <a href=\"/privacy/\">privacy policy</a>.</span></label>{}<div class=\"form-actions\"><button class=\"btn btn-ink\" type=\"submit\">Create account</button><a class=\"muted small\" href=\"/login/\">I already have an account</a></div></form><p class=\"form-meta\">Signup creates a personal workspace. Email verification is required before checkout.</p>",
        csrf_input(csrf),
        html_escape(name),
        html_escape(email),
        hidden("plan", plan),
        plan_note,
        error_block(error)
    );
    public_page(
        "Create account",
        &auth_shell(
            "Account / Create",
            "Create an account.",
            "Start with a personal workspace. Organization membership and billing come next.",
            &body,
        ),
        cookies,
    )
}

pub fn forgot_page(csrf: &str, sent: bool, error: Option<&str>, cookies: &[String]) -> Response {
    let body = if sent {
        "<div class=\"form-success\" role=\"status\"><p class=\"alert-title\">Check your email.</p><p>If an eligible account exists, a single-use reset link has been sent. The link expires in one hour.</p></div><p class=\"form-meta\"><a href=\"/login/\">Return to sign in</a></p>".to_string()
    } else {
        format!(
            "<form class=\"auth-form\" method=\"post\" action=\"/forgot-password/\" novalidate>{}<div class=\"field\"><label for=\"email\">Email</label><input id=\"email\" name=\"email\" type=\"email\" autocomplete=\"email\" required maxlength=\"254\"></div>{}<div class=\"form-actions\"><button class=\"btn btn-ink\" type=\"submit\">Send reset link</button></div></form><p class=\"form-meta\">The response is the same whether or not an account exists.</p>",
            csrf_input(csrf),
            error_block(error)
        )
    };
    public_page(
        "Reset password",
        &auth_shell(
            "Account / Recovery",
            "Reset your password.",
            "A single-use link will let you choose a new password and revoke every old session.",
            &body,
        ),
        cookies,
    )
}

pub fn reset_page(csrf: &str, secret: &str, error: Option<&str>, cookies: &[String]) -> Response {
    let body = format!(
        "<form class=\"auth-form\" method=\"post\" action=\"/reset-password/\" novalidate>{}<div class=\"field\"><label for=\"password\">New password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"256\" required></div><div class=\"field\"><label for=\"confirm\">Confirm new password</label><input id=\"confirm\" name=\"confirm\" type=\"password\" autocomplete=\"new-password\" minlength=\"12\" maxlength=\"256\" required></div>{}<div class=\"form-actions\"><button class=\"btn btn-ink\" type=\"submit\">Set new password</button></div></form><p class=\"form-meta\">This invalidates every existing session for the account.</p>",
        csrf_input(csrf),
        error_block(error)
    );
    let with_secret = body.replace(
        "<div class=\"field\"><label for=\"password\">",
        &format!(
            "{}<div class=\"field\"><label for=\"password\">",
            hidden("secret", secret)
        ),
    );
    public_page(
        "Choose a new password",
        &auth_shell(
            "Account / New password",
            "Choose a new password.",
            "The reset link is single-use. After the change, sign in with the new password.",
            &with_secret,
        ),
        cookies,
    )
}

pub fn verify_page(csrf: &str, secret: &str, error: Option<&str>, cookies: &[String]) -> Response {
    let body = format!(
        "<form class=\"auth-form\" method=\"post\" action=\"/verify-email/\" novalidate>{}<div class=\"field\"><label for=\"token\">Verification link</label><input id=\"token\" class=\"code-input\" name=\"secret\" type=\"text\" value=\"{}\" readonly></div>{}<div class=\"form-actions\"><button class=\"btn btn-ink\" type=\"submit\">Verify email</button><a class=\"btn btn-outline\" href=\"/account/\">Not now</a></div></form><p class=\"form-meta\">Verification is required before a plan checkout. The link is single-use and expires in 24 hours.</p>",
        csrf_input(csrf),
        html_escape(secret),
        error_block(error)
    );
    public_page(
        "Verify email",
        &auth_shell(
            "Account / Verify email",
            "Confirm your email.",
            "This proves the account can receive recovery and billing messages.",
            &body,
        ),
        cookies,
    )
}

pub fn verify_sent_page(cookies: &[String]) -> Response {
    let body = "<div class=\"form-success\" role=\"status\"><p class=\"alert-title\">Verification sent.</p><p>Open the link in the email to verify this address. In local development, the message is written to the configured outbox directory.</p></div><p class=\"form-meta\"><a href=\"/account/\">Continue to account</a> &nbsp; <a href=\"/login/\">Sign in</a></p>".to_string();
    public_page(
        "Verification sent",
        &auth_shell(
            "Account / Verify email",
            "Verification sent.",
            "The account exists. Billing stays locked until the link is confirmed.",
            &body,
        ),
        cookies,
    )
}

pub fn reset_done_page(cookies: &[String]) -> Response {
    let body = "<div class=\"form-success\" role=\"status\"><p class=\"alert-title\">Password changed.</p><p>Every previous session was revoked. Sign in with the new password.</p></div><p class=\"form-meta\"><a class=\"btn btn-ink\" href=\"/login/\">Sign in</a></p>".to_string();
    public_page(
        "Password changed",
        &auth_shell(
            "Account / Recovery",
            "Password changed.",
            "The reset is complete.",
            &body,
        ),
        cookies,
    )
}

pub fn verify_done_page(
    auth: &crate::auth::AuthSession,
    csrf: &str,
    cookies: &[String],
) -> Response {
    let body = format!(
        "<div class=\"form-success\" role=\"status\"><p class=\"alert-title\">Email verified.</p><p>{} is now verified. Billing actions are available.</p></div><p class=\"form-meta\"><a class=\"btn btn-ink\" href=\"/account/billing/\">Open billing</a></p>",
        html_escape(auth.email.as_str())
    );
    let content = crate::web::layout::account_page(
        "Email verified",
        "overview",
        auth,
        &format!("<div class=\"panel span-8\">{}</div>", body),
        cookies,
    );
    let _ = csrf;
    content
}
