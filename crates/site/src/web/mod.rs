pub mod account_pages;
pub mod auth_pages;
pub mod billing_pages;
pub mod layout;

use chrono::{DateTime, Utc};

pub use layout::{
    account_page, alert, csrf_input, hidden, html_result, primary_nav, public_page, see_other,
    text_response,
};

pub fn money(minor: i64, currency: &str) -> String {
    let sign = if minor < 0 { "-" } else { "" };
    let absolute = minor.unsigned_abs();
    let whole = group_thousands(absolute / 100);
    let cents = absolute % 100;
    if currency == "EUR" {
        format!("{sign}€{whole}.{cents:02}")
    } else {
        format!("{sign}{whole}.{cents:02} {currency}")
    }
}

/// Group an exact integer with commas; the same text is copyable and never a
/// float. Used for money and counters (1,990 not 1990).
fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

pub fn date(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|time| time.format("%d %b %Y").to_string())
        .unwrap_or_else(|| "Unavailable".to_string())
}

pub fn datetime(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|time| time.format("%d %b %Y, %H:%M UTC").to_string())
        .unwrap_or_else(|| "Unavailable".to_string())
}

pub fn percent(part: i64, total: i64) -> u32 {
    if total <= 0 {
        return 0;
    }
    ((part.max(0) as i128 * 100) / total as i128).min(100) as u32
}

pub fn status_badge(status: &str) -> String {
    let class = match status {
        "active" | "paid" | "succeeded" | "passed" => "ok",
        "open" | "pending" | "trialing" | "processing" | "requires_action" | "past_due" => {
            "pending"
        }
        "failed" | "canceled" | "void" => "bad",
        _ => "neutral",
    };
    format!(
        "<span class=\"badge {}\">{}</span>",
        class,
        crate::error::html_escape(status)
    )
}

#[cfg(test)]
mod money_tests {
    use super::money;

    #[test]
    fn exact_minor_units_are_grouped_without_a_second_currency_code() {
        assert_eq!(money(199_000, "EUR"), "€1,990.00");
        assert_eq!(money(2_299, "EUR"), "€22.99");
        assert_eq!(money(-5, "EUR"), "-€0.05");
        assert_eq!(money(123_456_789, "USD"), "1,234,567.89 USD");
    }
}
