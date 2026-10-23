//! Exact integer money. Every amount is a signed count of minor units of one
//! ISO-4217-shaped currency code; there is no float anywhere, and every
//! arithmetic operation is checked (a saturated invoice total would hide
//! debt, so it is refused instead).

use crate::error::PaymentError;

/// Largest absolute amount accepted at any boundary (10^15 minor units).
/// It keeps `amount * quantity` and `amount * bps` inside `i128` comfortably
/// and keeps a formatted amount human-scaled.
pub const MAX_MONEY_MINOR: i64 = 1_000_000_000_000_000;

/// Largest currency exponent accepted (0..=4).
pub const MAX_CURRENCY_EXPONENT: u32 = 4;

/// Validate an ISO-4217-shaped code: exactly three ASCII uppercase letters.
pub fn valid_currency(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())
}

/// Validate one amount.
pub fn valid_amount(amount: i64) -> bool {
    (-MAX_MONEY_MINOR..=MAX_MONEY_MINOR).contains(&amount)
}

/// Checked addition of two bounded amounts.
pub fn checked_add(left: i64, right: i64) -> Result<i64, PaymentError> {
    let value = i128::from(left) + i128::from(right);
    let value = i64::try_from(value).map_err(|_| {
        PaymentError::Config(format!(
            "money overflow: {left} + {right} leaves the i64 domain"
        ))
    })?;
    if !valid_amount(value) {
        return Err(PaymentError::Config(format!(
            "money overflow: {left} + {right} exceeds the bounded amount domain"
        )));
    }
    Ok(value)
}

/// Checked subtraction.
pub fn checked_sub(left: i64, right: i64) -> Result<i64, PaymentError> {
    let value = i128::from(left) - i128::from(right);
    let value = i64::try_from(value).map_err(|_| {
        PaymentError::Config(format!(
            "money overflow: {left} - {right} leaves the i64 domain"
        ))
    })?;
    if !valid_amount(value) {
        return Err(PaymentError::Config(format!(
            "money overflow: {left} - {right} exceeds the bounded amount domain"
        )));
    }
    Ok(value)
}

/// Checked `quantity * unit_amount` (quantity is a signed count; a negative
/// quantity is a credit line).
pub fn checked_mul(quantity: i64, unit_amount: i64) -> Result<i64, PaymentError> {
    let value = i128::from(quantity) * i128::from(unit_amount);
    let value = i64::try_from(value).map_err(|_| {
        PaymentError::Config(format!(
            "money overflow: {quantity} * {unit_amount} leaves the i64 domain"
        ))
    })?;
    if !valid_amount(value) {
        return Err(PaymentError::Config(format!(
            "money overflow: {quantity} * {unit_amount} exceeds the bounded amount domain"
        )));
    }
    Ok(value)
}

/// Round `numerator / denominator` half away from zero using exact integer
/// arithmetic. `denominator` must be positive.
pub fn round_div(numerator: i128, denominator: i128) -> Result<i128, PaymentError> {
    if denominator <= 0 {
        return Err(PaymentError::Config(
            "money rounding denominator must be positive".into(),
        ));
    }
    let sign = if numerator < 0 { -1i128 } else { 1i128 };
    let absolute = numerator.abs();
    let quotient = (absolute + denominator / 2) / denominator;
    Ok(sign * quotient)
}

/// Prorate `amount` over `remaining_ms` of `period_ms`, half away from zero.
pub fn prorate(amount_minor: i64, remaining_ms: i64, period_ms: i64) -> Result<i64, PaymentError> {
    if remaining_ms < 0 || period_ms <= 0 {
        return Err(PaymentError::Malformed(
            "proration requires remaining_ms >= 0 and period_ms > 0".into(),
        ));
    }
    let remaining = remaining_ms.min(period_ms);
    let value = round_div(
        i128::from(amount_minor) * i128::from(remaining),
        i128::from(period_ms),
    )?;
    let value = i64::try_from(value)
        .map_err(|_| PaymentError::Config("proration result leaves the i64 money domain".into()))?;
    if !valid_amount(value) {
        return Err(PaymentError::Config(
            "proration result exceeds the bounded amount domain".into(),
        ));
    }
    Ok(value)
}

/// Convert an amount through an explicitly configured basis-point rate
/// (`10_000` = 1:1). Callers use this only when the rate exists; an absent
/// rate is a typed [`PaymentError::FxUnconfigured`], never an implicit 1:1.
pub fn convert(amount_minor: i64, rate_bps: i64) -> Result<i64, PaymentError> {
    if rate_bps <= 0 {
        return Err(PaymentError::Config(
            "FX rate must be a positive basis-point value".into(),
        ));
    }
    let value = round_div(i128::from(amount_minor) * i128::from(rate_bps), 10_000)?;
    let value = i64::try_from(value)
        .map_err(|_| PaymentError::Config("FX conversion leaves the i64 domain".into()))?;
    if !valid_amount(value) {
        return Err(PaymentError::Config(
            "FX conversion exceeds the bounded amount domain".into(),
        ));
    }
    Ok(value)
}

/// Exclusive tax: added to the taxable base. `rate_bps` is 0..=10_000.
pub fn exclusive_tax(base_minor: i64, rate_bps: i64) -> Result<i64, PaymentError> {
    if !(0..=10_000).contains(&rate_bps) {
        return Err(PaymentError::Config(
            "tax rate must be 0..=10000 basis points".into(),
        ));
    }
    let value = round_div(i128::from(base_minor) * i128::from(rate_bps), 10_000)?;
    i64::try_from(value)
        .map_err(|_| PaymentError::Config("tax amount leaves the i64 domain".into()))
}

/// Inclusive tax extracted from a tax-inclusive base: the portion of `base`
/// that is tax. Exact integer arithmetic, half away from zero.
pub fn inclusive_tax(base_minor: i64, rate_bps: i64) -> Result<i64, PaymentError> {
    if !(0..=10_000).contains(&rate_bps) {
        return Err(PaymentError::Config(
            "tax rate must be 0..=10000 basis points".into(),
        ));
    }
    let denominator = 10_000i128 + i128::from(rate_bps);
    let value = round_div(i128::from(base_minor) * i128::from(rate_bps), denominator)?;
    i64::try_from(value)
        .map_err(|_| PaymentError::Config("tax amount leaves the i64 domain".into()))
}

/// Exact minor-unit formatting: `1234` USD (exponent 2) -> `12.34`;
/// `5` USD -> `0.05`; `1234` JPY (exponent 0) -> `1234`.
pub fn format_minor(amount_minor: i64, currency: &str, exponent: u32) -> String {
    debug_assert!(valid_currency(currency));
    debug_assert!(exponent <= MAX_CURRENCY_EXPONENT);
    if exponent == 0 {
        return format!("{amount_minor} {currency}");
    }
    let sign = if amount_minor < 0 { "-" } else { "" };
    let absolute = i128::from(amount_minor).unsigned_abs();
    let factor = 10u128.pow(exponent);
    let major = absolute / factor;
    let minor = absolute % factor;
    format!(
        "{sign}{major}.{minor:0width$} {currency}",
        width = exponent as usize
    )
}

/// The conventional exponent for a currency when the catalogue omits one.
pub fn default_exponent(currency: &str) -> u32 {
    match currency {
        "JPY" | "KRW" | "VND" | "CLP" | "ISK" => 0,
        "BHD" | "KWD" | "OMR" | "TND" => 3,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_formatting_never_uses_float() {
        assert_eq!(format_minor(1234, "USD", 2), "12.34 USD");
        assert_eq!(format_minor(5, "USD", 2), "0.05 USD");
        assert_eq!(format_minor(-1234, "EUR", 2), "-12.34 EUR");
        assert_eq!(format_minor(1234, "JPY", 0), "1234 JPY");
        assert_eq!(format_minor(1234, "BHD", 3), "1.234 BHD");
        assert_eq!(format_minor(0, "USD", 2), "0.00 USD");
        assert!(valid_currency("USD"));
        for bad in ["usd", "US", "USDD", "U$D", ""] {
            assert!(!valid_currency(bad), "{bad}");
        }
    }

    #[test]
    fn checked_arithmetic_refuses_saturation() {
        assert_eq!(checked_add(2, 3).unwrap(), 5);
        assert_eq!(checked_sub(2, 3).unwrap(), -1);
        assert!(checked_add(MAX_MONEY_MINOR, 1).is_err());
        assert!(checked_sub(-MAX_MONEY_MINOR, 1).is_err());
        assert!(checked_mul(MAX_MONEY_MINOR, 2).is_err());
        assert!(checked_mul(1_000_000, 1_000_000).is_ok());
    }

    #[test]
    fn proration_and_tax_are_exact_and_half_away_from_zero() {
        assert_eq!(prorate(1000, 15, 30).unwrap(), 500);
        assert_eq!(prorate(1000, 1, 3).unwrap(), 333);
        assert_eq!(prorate(-1000, 1, 3).unwrap(), -333);
        assert_eq!(exclusive_tax(10_000, 825).unwrap(), 825);
        assert_eq!(inclusive_tax(10_825, 825).unwrap(), 825);
        assert_eq!(convert(100, 10_000).unwrap(), 100);
        assert_eq!(convert(100, 5_000).unwrap(), 50);
        assert!(exclusive_tax(1, 10_001).is_err());
        assert!(prorate(1, 1, 0).is_err());
    }
}
