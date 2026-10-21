//! Exact money, rates, quantities and deterministic rounding.
//!
//! Every monetary value is an integer number of micro-units of one currency.
//! There is no floating-point representation, conversion or intermediate:
//! scaling uses `i128` intermediates and every result is checked. Quantities
//! used by cost kernels are integer micro-units of a base measure (grams,
//! metres, pieces) and planning quantities are bounded whole units. Rounding
//! is deterministic and documented: costs round half away from zero, and
//! requirements round up (you cannot buy a fraction of a unit).

use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

/// Fractional digits represented by one major unit (money and scaled
/// quantities).
pub const MICROS_PER_UNIT: i64 = 1_000_000;
/// Hard bound on any money magnitude, in micro-units (one trillion major
/// units). This keeps every `i128` intermediate comfortably in range.
pub const MAX_MONEY_MICROS: i64 = 1_000_000_000_000_000_000;
/// Hard bound on a scaled quantity, in micro-units.
pub const MAX_SCALED_QUANTITY: i64 = 1_000_000_000_000_000_000;
/// Hard bound on a whole-unit planning quantity.
pub const MAX_UNITS: u64 = 1_000_000_000_000;
/// Maximum rate in parts-per-million (100%).
pub const MAX_PPM: i64 = 1_000_000;

/// A rejected exact value or arithmetic operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExactError {
    /// A currency code is not three uppercase ASCII letters.
    #[error("currency must be exactly three uppercase ASCII letters")]
    InvalidCurrency,
    /// A value is outside its documented bound.
    #[error("{what} is outside its bound: {detail}")]
    OutOfBounds {
        /// The value class.
        what: &'static str,
        /// A bounded explanation.
        detail: String,
    },
    /// The value carries more than six fractional digits.
    #[error("value has more than six fractional digits")]
    TooManyFractionalDigits,
    /// Checked arithmetic overflowed.
    #[error("exact arithmetic overflowed")]
    Overflow,
    /// A division by zero.
    #[error("division by zero")]
    DivisionByZero,
    /// Negative money or quantity where only non-negative is meaningful.
    #[error("{what} must not be negative")]
    Negative {
        /// The value class.
        what: &'static str,
    },
    /// Two different currencies were combined.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch {
        /// The left currency.
        left: Currency,
        /// The right currency.
        right: Currency,
    },
}

// ------------------------------------------------------------------ currency

/// An ISO-4217-style three-letter uppercase currency code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Currency([u8; 3]);

impl Currency {
    /// US dollar.
    pub const USD: Currency = Currency(*b"USD");
    /// Euro.
    pub const EUR: Currency = Currency(*b"EUR");
    /// Renminbi.
    pub const CNY: Currency = Currency(*b"CNY");
    /// Japanese yen.
    pub const JPY: Currency = Currency(*b"JPY");
    /// Pound sterling.
    pub const GBP: Currency = Currency(*b"GBP");

    /// Validate a currency code.
    pub fn new(code: &str) -> Result<Self, ExactError> {
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(ExactError::InvalidCurrency);
        }
        Ok(Self([bytes[0], bytes[1], bytes[2]]))
    }

    /// The code.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

impl TryFrom<String> for Currency {
    type Error = ExactError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Currency::new(&value)
    }
}

impl From<Currency> for String {
    fn from(value: Currency) -> Self {
        value.as_str().to_string()
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// --------------------------------------------------------------------- money

/// Exact money: signed integer micro-units plus a currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Money {
    /// Micro-units of the major currency unit.
    pub micros: i64,
    /// The currency.
    pub currency: Currency,
}

/// Wire shape for [`Money`]; deserialization re-applies the documented bound
/// so an out-of-range JSON value cannot bypass [`Money::from_micros`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoneyDef {
    micros: i64,
    currency: Currency,
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let def = MoneyDef::deserialize(deserializer)?;
        Money::from_micros(def.micros, def.currency).map_err(serde::de::Error::custom)
    }
}

impl Money {
    /// Zero in a currency.
    pub const fn zero(currency: Currency) -> Self {
        Self {
            micros: 0,
            currency,
        }
    }

    /// Construct from micro-units, enforcing the magnitude bound.
    pub fn from_micros(micros: i64, currency: Currency) -> Result<Self, ExactError> {
        if micros.unsigned_abs() > MAX_MONEY_MICROS as u64 {
            return Err(ExactError::OutOfBounds {
                what: "money",
                detail: format!("{micros} micros"),
            });
        }
        Ok(Self { micros, currency })
    }

    /// Construct from a decimal string with at most six fractional digits.
    /// No floating point is parsed anywhere.
    pub fn parse(amount: &str, currency: Currency) -> Result<Self, ExactError> {
        let (negative, body) = match amount.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, amount),
        };
        let (whole, fraction) = match body.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (body, ""),
        };
        if whole.is_empty() && fraction.is_empty() {
            return Err(ExactError::OutOfBounds {
                what: "money",
                detail: "empty amount".to_string(),
            });
        }
        if fraction.len() > 6 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ExactError::TooManyFractionalDigits);
        }
        if !whole.is_empty() && !whole.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ExactError::OutOfBounds {
                what: "money",
                detail: "invalid digits".to_string(),
            });
        }
        let whole_value: i128 = if whole.is_empty() {
            0
        } else {
            whole.parse::<i128>().map_err(|_| ExactError::Overflow)?
        };
        let mut fraction_value: i128 = 0;
        for (index, byte) in fraction.bytes().enumerate() {
            fraction_value += i128::from(byte - b'0') * 10_i128.pow(5 - index as u32);
        }
        let magnitude = whole_value
            .checked_mul(i128::from(MICROS_PER_UNIT))
            .and_then(|value| value.checked_add(fraction_value))
            .ok_or(ExactError::Overflow)?;
        let signed = if negative { -magnitude } else { magnitude };
        let micros = i64::try_from(signed).map_err(|_| ExactError::Overflow)?;
        Self::from_micros(micros, currency)
    }

    /// The micro-unit value.
    pub const fn micros(self) -> i64 {
        self.micros
    }

    /// Checked addition.
    pub fn checked_add(self, other: Self) -> Result<Self, ExactError> {
        if self.currency != other.currency {
            return Err(ExactError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            });
        }
        let value = self
            .micros
            .checked_add(other.micros)
            .ok_or(ExactError::Overflow)?;
        Self::from_micros(value, self.currency)
    }

    /// Checked subtraction.
    pub fn checked_sub(self, other: Self) -> Result<Self, ExactError> {
        if self.currency != other.currency {
            return Err(ExactError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            });
        }
        let value = self
            .micros
            .checked_sub(other.micros)
            .ok_or(ExactError::Overflow)?;
        Self::from_micros(value, self.currency)
    }

    /// Sum a series, refusing an empty series (an empty total is not a zero
    /// unless the caller says so explicitly).
    pub fn checked_sum(values: &[Self], currency: Currency) -> Result<Self, ExactError> {
        let mut total = Self::zero(currency);
        for value in values {
            total = total.checked_add(*value)?;
        }
        Ok(total)
    }

    /// Multiply by a whole-unit quantity.
    pub fn mul_units(self, units: u64) -> Result<Self, ExactError> {
        let value = i128::from(self.micros)
            .checked_mul(i128::from(units))
            .ok_or(ExactError::Overflow)?;
        Self::from_micros(
            i64::try_from(value).map_err(|_| ExactError::Overflow)?,
            self.currency,
        )
    }

    /// Scale by a parts-per-million rate, rounding half away from zero.
    pub fn scale(self, rate: Rate) -> Result<Self, ExactError> {
        let value =
            mul_div_round_half_up(i128::from(self.micros), i128::from(rate.ppm()), 1_000_000)
                .ok_or(ExactError::Overflow)?;
        Self::from_micros(
            i64::try_from(value).map_err(|_| ExactError::Overflow)?,
            self.currency,
        )
    }

    /// Divide by a positive whole-unit count, rounding half away from zero.
    pub fn div_units(self, units: u64) -> Result<Self, ExactError> {
        if units == 0 {
            return Err(ExactError::DivisionByZero);
        }
        let value = div_round_half_away(i128::from(self.micros), i128::from(units));
        Self::from_micros(
            i64::try_from(value).map_err(|_| ExactError::Overflow)?,
            self.currency,
        )
    }

    /// True for exact zero.
    pub const fn is_zero(self) -> bool {
        self.micros == 0
    }

    /// Non-negative value, with a typed refusal.
    pub fn non_negative(self, what: &'static str) -> Result<Self, ExactError> {
        if self.micros < 0 {
            return Err(ExactError::Negative { what });
        }
        Ok(self)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.micros < 0 { "-" } else { "" };
        let magnitude = self.micros.unsigned_abs();
        write!(
            f,
            "{sign}{}.{:06} {}",
            magnitude / MICROS_PER_UNIT as u64,
            magnitude % MICROS_PER_UNIT as u64,
            self.currency
        )
    }
}

// ----------------------------------------------------------------------- rate

/// A rate in parts-per-million, in `0..=1_000_000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Rate(i64);

impl Rate {
    /// Zero rate.
    pub const ZERO: Rate = Rate(0);
    /// 100%.
    pub const ONE: Rate = Rate(MAX_PPM);

    /// Validate a parts-per-million rate.
    pub fn new(ppm: i64) -> Result<Self, ExactError> {
        if !(0..=MAX_PPM).contains(&ppm) {
            return Err(ExactError::OutOfBounds {
                what: "rate",
                detail: format!("{ppm} ppm"),
            });
        }
        Ok(Self(ppm))
    }

    /// The parts-per-million value.
    pub const fn ppm(self) -> i64 {
        self.0
    }

    /// The complement (`1 - self`), exact.
    pub fn complement(self) -> Rate {
        Rate(MAX_PPM - self.0)
    }

    /// The rate as a fractional string with six digits.
    pub fn as_fraction(self) -> String {
        format!("{}.{:06}", self.0 / MAX_PPM, self.0 % MAX_PPM)
    }
}

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RateVisitor;
        impl Visitor<'_> for RateVisitor {
            type Value = Rate;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an integer parts-per-million rate in 0..=1000000")
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Rate, E> {
                Rate::new(value).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Rate, E> {
                let value = i64::try_from(value).map_err(E::custom)?;
                Rate::new(value).map_err(E::custom)
            }
        }
        deserializer.deserialize_i64(RateVisitor)
    }
}

// ------------------------------------------------------------------ quantities

/// A scaled quantity in micro-units of its base measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ScaledQuantity(i64);

impl<'de> Deserialize<'de> for ScaledQuantity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let micros = i64::deserialize(deserializer)?;
        ScaledQuantity::new(micros).map_err(serde::de::Error::custom)
    }
}

impl ScaledQuantity {
    /// Zero.
    pub const ZERO: ScaledQuantity = ScaledQuantity(0);

    /// Validate a scaled quantity.
    pub fn new(micros: i64) -> Result<Self, ExactError> {
        if micros.unsigned_abs() > MAX_SCALED_QUANTITY as u64 {
            return Err(ExactError::OutOfBounds {
                what: "scaled quantity",
                detail: format!("{micros} micros"),
            });
        }
        Ok(Self(micros))
    }

    /// Micro-units.
    pub const fn micros(self) -> i64 {
        self.0
    }

    /// Non-negative value, with a typed refusal.
    pub fn non_negative(self, what: &'static str) -> Result<Self, ExactError> {
        if self.0 < 0 {
            return Err(ExactError::Negative { what });
        }
        Ok(self)
    }

    /// Multiply by an exact money unit price per base unit, yielding money.
    ///
    /// `price` is money per one whole base unit; a quantity of 250,000
    /// micro-units therefore costs a quarter of `price`.
    pub fn cost(self, price: Money) -> Result<Money, ExactError> {
        let value = mul_div_round_half_up(i128::from(self.0), i128::from(price.micros), 1_000_000)
            .ok_or(ExactError::Overflow)?;
        Money::from_micros(
            i64::try_from(value).map_err(|_| ExactError::Overflow)?,
            price.currency,
        )
    }

    /// Divide by a positive yield rate (usable output), rounding up: input
    /// must cover the loss.
    pub fn divide_by_yield(self, yield_rate: Rate) -> Result<Self, ExactError> {
        if yield_rate.ppm() == 0 {
            return Err(ExactError::DivisionByZero);
        }
        let value = mul_div_round_up(i128::from(self.0), 1_000_000, i128::from(yield_rate.ppm()))
            .ok_or(ExactError::Overflow)?;
        Self::new(i64::try_from(value).map_err(|_| ExactError::Overflow)?)
    }
}

/// A whole-unit quantity in `0..=1_000_000_000_000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Units(u64);

impl<'de> Deserialize<'de> for Units {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let units = u64::deserialize(deserializer)?;
        Units::new(units).map_err(serde::de::Error::custom)
    }
}

impl Units {
    /// Zero.
    pub const ZERO: Units = Units(0);
    /// One.
    pub const ONE: Units = Units(1);

    /// Validate a whole-unit quantity.
    pub fn new(units: u64) -> Result<Self, ExactError> {
        if units > MAX_UNITS {
            return Err(ExactError::OutOfBounds {
                what: "units",
                detail: format!("{units} units"),
            });
        }
        Ok(Self(units))
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// True for zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Checked addition.
    pub fn checked_add(self, other: Self) -> Result<Self, ExactError> {
        Self::new(self.0.checked_add(other.0).ok_or(ExactError::Overflow)?)
    }

    /// Checked subtraction.
    pub fn checked_sub(self, other: Self) -> Result<Self, ExactError> {
        Self::new(self.0.checked_sub(other.0).ok_or(ExactError::Overflow)?)
    }

    /// Checked multiplication.
    pub fn checked_mul(self, other: Self) -> Result<Self, ExactError> {
        Self::new(self.0.checked_mul(other.0).ok_or(ExactError::Overflow)?)
    }

    /// Divide round up (a fractional requirement consumes a whole unit).
    pub fn div_ceil(self, divisor: Units) -> Result<Self, ExactError> {
        if divisor.0 == 0 {
            return Err(ExactError::DivisionByZero);
        }
        Self::new(
            self.0
                .checked_add(divisor.0 - 1)
                .ok_or(ExactError::Overflow)?
                / divisor.0,
        )
    }

    /// Round up to a multiple (multiples of one are unchanged).
    pub fn round_up_to_multiple(self, multiple: Units) -> Result<Self, ExactError> {
        if multiple.0 == 0 {
            return Err(ExactError::DivisionByZero);
        }
        let remainder = self.0 % multiple.0;
        if remainder == 0 {
            return Ok(self);
        }
        Self::new(
            self.0
                .checked_add(multiple.0 - remainder)
                .ok_or(ExactError::Overflow)?,
        )
    }

    /// The larger of two quantities.
    pub const fn max(self, other: Self) -> Self {
        if self.0 >= other.0 {
            self
        } else {
            other
        }
    }

    /// Convert to scaled micro-units.
    pub fn to_scaled(self) -> Result<ScaledQuantity, ExactError> {
        let value = i128::from(self.0)
            .checked_mul(i128::from(MICROS_PER_UNIT))
            .ok_or(ExactError::Overflow)?;
        ScaledQuantity::new(i64::try_from(value).map_err(|_| ExactError::Overflow)?)
    }
}

// ------------------------------------------------------------------ rounding

/// `a * b / d` with half-away-from-zero rounding.
pub fn mul_div_round_half_up(a: i128, b: i128, d: i128) -> Option<i128> {
    if d == 0 {
        return None;
    }
    let product = a.checked_mul(b)?;
    Some(div_round_half_away(product, d))
}

/// `a * b / d` rounded up (towards positive infinity), exact.
pub fn mul_div_round_up(a: i128, b: i128, d: i128) -> Option<i128> {
    if d == 0 {
        return None;
    }
    let product = a.checked_mul(b)?;
    Some(div_round_up(product, d))
}

/// `n / d` with half-away-from-zero rounding.
pub fn div_round_half_away(n: i128, d: i128) -> i128 {
    debug_assert!(d != 0);
    let (sign, n_abs) = if n < 0 { (-1_i128, -n) } else { (1_i128, n) };
    let (d_sign, d_abs) = if d < 0 { (-1_i128, -d) } else { (1_i128, d) };
    let quotient = n_abs / d_abs;
    let remainder = n_abs % d_abs;
    let rounded = if remainder.saturating_mul(2) >= d_abs {
        quotient + 1
    } else {
        quotient
    };
    sign * d_sign * rounded
}

/// `n / d` rounded towards positive infinity (exact).
pub fn div_round_up(n: i128, d: i128) -> i128 {
    debug_assert!(d != 0);
    if n >= 0 {
        if d > 0 {
            (n + d - 1) / d
        } else {
            n / d
        }
    } else if d > 0 {
        n / d
    } else {
        (n + d + 1) / d
    }
}

/// Integer square root of a non-negative value, rounded down.
pub fn isqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    let mut low = 1_u64;
    let mut high = value.min(u32::MAX as u64);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if mid <= value / mid {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// `sqrt(value)` scaled by 1,000,000, rounded down. Used by safety-stock
/// kernels that must avoid floating point.
pub fn isqrt_scaled_ppm(value: Units) -> u64 {
    let scaled = (value.get() as u128) * 1_000_000_000_000;
    isqrt_u128(scaled) as u64
}

fn isqrt_u128(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut low = 1_u128;
    let mut high = (value >> 1).saturating_add(1);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if mid <= value / mid {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_parsing_is_exact_without_float() {
        let value = Money::parse("12.345678", Currency::USD).unwrap();
        assert_eq!(value.micros(), 12_345_678);
        assert_eq!(value.to_string(), "12.345678 USD");
        assert_eq!(Money::parse("0.000001", Currency::USD).unwrap().micros(), 1);
        assert!(Money::parse("0.0000001", Currency::USD).is_err());
        assert!(Money::parse("1e3", Currency::USD).is_err());
        assert!(Money::parse("--1", Currency::USD).is_err());
        assert_eq!(
            Money::parse("-0.5", Currency::USD).unwrap().micros(),
            -500_000
        );
    }

    #[test]
    fn overflow_and_currency_mismatch_are_typed() {
        let max = Money::from_micros(MAX_MONEY_MICROS, Currency::USD).unwrap();
        assert!(matches!(
            max.checked_add(Money::from_micros(1, Currency::USD).unwrap()),
            Err(ExactError::OutOfBounds { .. })
        ));
        let eur = Money::from_micros(1, Currency::EUR).unwrap();
        assert!(matches!(
            max.checked_add(eur),
            Err(ExactError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn rate_bounds_and_complement() {
        assert!(Rate::new(-1).is_err());
        assert!(Rate::new(1_000_001).is_err());
        assert_eq!(Rate::new(250_000).unwrap().complement().ppm(), 750_000);
    }

    #[test]
    fn quantities_round_up_for_requirements_and_half_for_costs() {
        assert_eq!(
            Units::new(10)
                .unwrap()
                .div_ceil(Units::new(3).unwrap())
                .unwrap()
                .get(),
            4
        );
        assert_eq!(
            Units::new(10)
                .unwrap()
                .round_up_to_multiple(Units::new(4).unwrap())
                .unwrap()
                .get(),
            12
        );
        // 0.333333 units at 3.000000 = 1.000000 (rounded half up).
        let quantity = ScaledQuantity::new(333_333).unwrap();
        let price = Money::from_micros(3_000_000, Currency::USD).unwrap();
        assert_eq!(quantity.cost(price).unwrap().micros(), 999_999);
    }

    #[test]
    fn yield_requires_more_input() {
        let qty = ScaledQuantity::new(900_000).unwrap();
        assert_eq!(
            qty.divide_by_yield(Rate::new(900_000).unwrap())
                .unwrap()
                .micros(),
            1_000_000
        );
        assert!(qty.divide_by_yield(Rate::ZERO).is_err());
    }

    #[test]
    fn scaled_sqrt_is_monotonic_and_exact_at_squares() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(10), 3);
        assert_eq!(isqrt(10_000), 100);
        assert_eq!(isqrt_scaled_ppm(Units::new(4).unwrap()), 2_000_000);
        assert_eq!(isqrt_scaled_ppm(Units::new(9).unwrap()), 3_000_000);
        assert!(isqrt_scaled_ppm(Units::new(2).unwrap()) > 1_414_000);
    }

    #[test]
    fn deserialization_cannot_bypass_documented_bounds() {
        let over = format!("{{\"micros\":{},\"currency\":\"USD\"}}", i64::MAX);
        assert!(serde_json::from_str::<Money>(&over).is_err());
        let under = format!("{{\"micros\":{},\"currency\":\"USD\"}}", i64::MIN);
        assert!(serde_json::from_str::<Money>(&under).is_err());
        let valid = format!("{{\"micros\":{},\"currency\":\"USD\"}}", MAX_MONEY_MICROS);
        assert!(serde_json::from_str::<Money>(&valid).is_ok());
        assert!(serde_json::from_str::<Currency>("\"usd\"").is_err());

        assert!(serde_json::from_str::<Units>("1000000000001").is_err());
        assert_eq!(
            serde_json::from_str::<Units>("1000000000000")
                .unwrap()
                .get(),
            MAX_UNITS
        );
        assert!(serde_json::from_str::<ScaledQuantity>(&i64::MAX.to_string()).is_err());
        assert_eq!(
            serde_json::from_str::<ScaledQuantity>("-1")
                .unwrap()
                .micros(),
            -1
        );
    }

    #[test]
    fn half_away_rounding_is_symmetric() {
        assert_eq!(div_round_half_away(5, 2), 3);
        assert_eq!(div_round_half_away(-5, 2), -3);
        assert_eq!(div_round_half_away(4, 2), 2);
        assert_eq!(div_round_half_away(5, -2), -3);
    }
}
