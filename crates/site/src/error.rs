use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use faktor_payments::PaymentError;
use thiserror::Error;

pub type SiteResult<T> = Result<T, SiteError>;

#[derive(Debug, Error)]
pub enum SiteError {
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    #[error("unauthenticated")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("unprocessable: {0}")]
    Unprocessable(&'static str),
    #[error("rate limited")]
    RateLimited { retry_after_secs: u64 },
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("temporarily unavailable: {0}")]
    Unavailable(&'static str),
    #[error("internal error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl SiteError {
    pub fn internal(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Internal(Box::new(error))
    }

    pub fn internal_message(message: impl Into<String>) -> Self {
        Self::Internal(Box::new(InternalMessage(message.into())))
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::Unauthorized => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Unprocessable(_) => "unprocessable",
            Self::RateLimited { .. } => "rate_limited",
            Self::PayloadTooLarge => "payload_too_large",
            Self::Unavailable(_) => "temporarily_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn public_message(&self) -> &'static str {
        match self {
            Self::BadRequest(message) | Self::Unprocessable(message) | Self::Conflict(message) => {
                message
            }
            Self::Unauthorized => "Sign in to continue.",
            Self::Forbidden => "You do not have access to this operation.",
            Self::NotFound => "That record is not available.",
            Self::RateLimited { .. } => "Too many attempts. Try again later.",
            Self::PayloadTooLarge => "The request is larger than this endpoint accepts.",
            Self::Unavailable(message) => message,
            Self::Internal(_) => {
                "The request could not be completed. No partial billing change was made."
            }
        }
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct InternalMessage(String);

impl IntoResponse for SiteError {
    fn into_response(self) -> Response {
        if let Self::Internal(error) = &self {
            tracing::error!(error = %error, "site request failed");
        }
        let status = self.status();
        let code = self.code();
        let message = self.public_message();
        let retry = match &self {
            Self::RateLimited { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        };
        let html = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{} — Faktor</title><link rel=\"stylesheet\" href=\"/css/site.css\"></head><body><main class=\"error-page\"><p class=\"kicker\">Faktor / {}</p><h1>{}</h1><p>{}</p><p><a class=\"link-arrow\" href=\"/\">Return home</a> &nbsp; <a class=\"link-arrow\" href=\"/account/\">Account</a></p></main></body></html>",
            html_escape(message),
            html_escape(code),
            html_escape(message),
            html_escape(match status {
                StatusCode::UNAUTHORIZED => "The session is missing, expired or was revoked.",
                StatusCode::FORBIDDEN => "The operation was refused before any feature data was read.",
                StatusCode::NOT_FOUND => "No record is exposed for this address.",
                StatusCode::TOO_MANY_REQUESTS => "The bounded attempt window is exhausted.",
                StatusCode::SERVICE_UNAVAILABLE => "A required provider capability is not configured.",
                _ => "The operation did not complete.",
            })
        );
        let mut response = (status, html).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if let Some(seconds) = retry {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

impl From<rusqlite::Error> for SiteError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(error)
    }
}

impl From<std::io::Error> for SiteError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error)
    }
}

impl From<crate::config::ConfigError> for SiteError {
    fn from(error: crate::config::ConfigError) -> Self {
        match error {
            crate::config::ConfigError::Invalid(message) => {
                Self::internal_message(format!("configuration: {message}"))
            }
            crate::config::ConfigError::Io(error) => Self::internal(error),
        }
    }
}

impl From<PaymentError> for SiteError {
    fn from(error: PaymentError) -> Self {
        match error {
            PaymentError::NotFound(_) => Self::NotFound,
            PaymentError::Conflict(_) | PaymentError::IdempotencyConflict(_) => {
                Self::Conflict("That billing operation conflicts with the recorded state.")
            }
            PaymentError::IllegalState(_) => {
                Self::Conflict("That action is not available in the recorded billing state.")
            }
            PaymentError::Malformed(_) => Self::Unprocessable("The billing request was not valid."),
            PaymentError::Declined(_) => Self::Unprocessable("The payment was declined."),
            PaymentError::NothingDue => Self::Conflict("There is no outstanding balance."),
            PaymentError::ProviderUnconfigured(_) => {
                Self::Unavailable("The payment provider is not configured for this deployment.")
            }
            PaymentError::WebhookUnauthorized(_) | PaymentError::WebhookStaleTimestamp => {
                Self::Forbidden
            }
            other => {
                tracing::error!(error = %other, "payment operation failed");
                Self::internal_message("payment provider operation failed")
            }
        }
    }
}

pub fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_all_html_delimiters() {
        assert_eq!(
            html_escape("<script>\"x\"&'"),
            "&lt;script&gt;&quot;x&quot;&amp;&#39;"
        );
    }

    #[test]
    fn rate_limit_exposes_retry_after() {
        let response = SiteError::RateLimited {
            retry_after_secs: 7,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "7");
    }
}
