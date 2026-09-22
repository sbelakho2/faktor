//! Exact money.
//!
//! [`Money`] is an integer count of micro-units plus an ISO-4217-style
//! currency. There is no floating-point representation anywhere in this
//! module: every operation is checked integer arithmetic over `i64`
//! micro-units with `i128` intermediates, and every failure is a typed
//! [`MoneyError`].
//!
//! # Wire format
//!
//! Canonical JSON is an object with a decimal-string amount:
//!
//! ```json
//! { "currency": "CNY", "amount": "0.003700" }
//! ```
//!
//! - `amount` is always a string, always in major units, always exactly six
//!   fractional digits when serialized; a JSON number is rejected.
//! - Deserialization additionally accepts the legacy micro-unit form
//!   `{ "currency": "CNY", "micros": "3700" }` (or an integer `micros`),
//!   because earlier Faktor wire surfaces encoded micro-units. The two
//!   forms are never mixed: `amount` is major units, `micros` is micro
//!   units, and a document carrying both is rejected.
//! - `"0.003700"` (amount) and `"3700"` (micros) are the same value; a
//!   `micros` field is never interpreted as major units.

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;

use crate::quantity::Quantity;
use crate::text::deserialize_validated;

/// Number of fractional digits in the canonical decimal string.
pub const FRACTIONAL_DIGITS: u32 = 6;
/// Micro-units per major unit.
pub const MICROS_PER_UNIT: i64 = 1_000_000;
/// Hard bound on a decimal money string.
pub const MAX_DECIMAL_STRING_BYTES: usize = 32;
/// Maximum basis points (100%).
pub const MAX_BASIS_POINTS: u16 = 10_000;

/// A rejected currency code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CurrencyError {
    /// Not exactly three ASCII letters.
    #[error("currency must be exactly three ASCII letters")]
    InvalidCode,
}

/// An ISO-4217-style three-letter currency code (uppercase ASCII).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Currency([u8; 3]);

impl Currency {
    /// Renminbi.
    pub const CNY: Currency = Currency(*b"CNY");
    /// US dollar.
    pub const USD: Currency = Currency(*b"USD");
    /// Euro.
    pub const EUR: Currency = Currency(*b"EUR");
    /// Hong Kong dollar.
    pub const HKD: Currency = Currency(*b"HKD");
    /// New Taiwan dollar.
    pub const TWD: Currency = Currency(*b"TWD");
    /// Japanese yen.
    pub const JPY: Currency = Currency(*b"JPY");
    /// Pound sterling.
    pub const GBP: Currency = Currency(*b"GBP");
    /// Singapore dollar.
    pub const SGD: Currency = Currency(*b"SGD");

    /// Validate a three-letter uppercase ASCII code.
    pub fn new(code: &str) -> Result<Self, CurrencyError> {
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(|b| b.is_ascii_uppercase()) {
            return Err(CurrencyError::InvalidCode);
        }
        Ok(Self([bytes[0], bytes[1], bytes[2]]))
    }

    /// The code as a string slice.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Currency {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer, |raw| {
            Currency::new(raw).map_err(|e| e.to_string())
        })
    }
}

/// A rejected money value or money operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    /// The decimal string is not `-?digits[.digits]`.
    #[error("invalid decimal money string {excerpt:?}")]
    InvalidDecimalString {
        /// A bounded excerpt of the rejected input.
        excerpt: String,
    },
    /// More than six fractional digits cannot be represented exactly.
    #[error("money has {actual} fractional digits; at most {max} are representable")]
    TooManyFractionalDigits { max: u32, actual: usize },
    /// The value does not fit `i64` micro-units.
    #[error("money value {excerpt:?} does not fit i64 micro-units")]
    OutOfRange {
        /// A bounded excerpt of the rejected input.
        excerpt: String,
    },
    /// Two different currencies were combined.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch { left: Currency, right: Currency },
    /// Checked integer arithmetic overflowed.
    #[error("money arithmetic overflowed i64 micro-units")]
    Overflow,
    /// Basis points outside `0..=10000`.
    #[error("basis points {actual} exceed the maximum {max}")]
    BasisPointsOutOfRange { max: u16, actual: u16 },
}

fn excerpt(raw: &str) -> String {
    let mut out: String = raw.chars().take(40).collect();
    if raw.chars().count() > 40 {
        out.push('…');
    }
    out
}

/// A rejected basis-points value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("basis points {actual} exceed the maximum {max}")]
pub struct BasisPointsError {
    /// The maximum (10000).
    pub max: u16,
    /// The rejected value.
    pub actual: u16,
}

/// A validated basis-points value in `0..=10000` (10000 = 100%).
///
/// Integer-only scaling: no percentage ever passes through a floating-point
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct BasisPoints(u16);

impl BasisPoints {
    /// The maximum (100%).
    pub const MAX: BasisPoints = BasisPoints(MAX_BASIS_POINTS);
    /// Zero (0%).
    pub const ZERO: BasisPoints = BasisPoints(0);

    /// Validate a basis-points value.
    pub fn new(value: u16) -> Result<Self, BasisPointsError> {
        if value > MAX_BASIS_POINTS {
            return Err(BasisPointsError {
                max: MAX_BASIS_POINTS,
                actual: value,
            });
        }
        Ok(Self(value))
    }

    /// The raw value.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for BasisPoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for BasisPoints {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u16::deserialize(deserializer)?;
        BasisPoints::new(raw).map_err(de::Error::custom)
    }
}

/// An exact monetary amount: `micros` micro-units of `currency`.
///
/// `micros` is the canonical integer representation (`¥0.0037` is
/// `3700` micro-CNY). Cross-currency ordering is deliberately absent:
/// [`PartialOrd`] returns `None` for different currencies instead of
/// inventing a conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Money {
    /// The currency of `micros`.
    pub currency: Currency,
    /// The amount in micro-units (1 major unit = 1_000_000 micro-units).
    pub micros: i64,
}

impl Money {
    /// Construct from micro-units.
    pub const fn from_micros(currency: Currency, micros: i64) -> Self {
        Self { currency, micros }
    }

    /// Zero in `currency`.
    pub const fn zero(currency: Currency) -> Self {
        Self {
            currency,
            micros: 0,
        }
    }

    /// Construct from whole major units.
    pub fn from_units(currency: Currency, units: i64) -> Result<Self, MoneyError> {
        let micros = (units as i128) * (MICROS_PER_UNIT as i128);
        let micros = i64::try_from(micros).map_err(|_| MoneyError::Overflow)?;
        Ok(Self { currency, micros })
    }

    /// The canonical decimal string, always six fractional digits.
    pub fn to_decimal_string(self) -> String {
        let value = self.micros as i128;
        let sign = if value < 0 { "-" } else { "" };
        let magnitude = value.abs();
        let units = magnitude / (MICROS_PER_UNIT as i128);
        let frac = magnitude % (MICROS_PER_UNIT as i128);
        format!("{sign}{units}.{frac:06}")
    }

    /// Parse a decimal string in major units (strict, exact).
    ///
    /// Accepted: `-?[0-9]+(\.[0-9]{1,6})?`. Rejected: exponents, `+`
    /// signs, whitespace, grouping separators, a trailing bare dot, and
    /// more than six fractional digits (which cannot be represented
    /// exactly, so they are never silently rounded).
    pub fn parse(currency: Currency, raw: &str) -> Result<Self, MoneyError> {
        if raw.is_empty() || raw.len() > MAX_DECIMAL_STRING_BYTES {
            return Err(MoneyError::InvalidDecimalString {
                excerpt: excerpt(raw),
            });
        }
        let (negative, body) = match raw.strip_prefix('-') {
            Some(body) => (true, body),
            None => (false, raw),
        };
        let (integer_part, fractional_part) = match body.split_once('.') {
            Some((integer, fractional)) => (integer, Some(fractional)),
            None => (body, None),
        };
        if integer_part.is_empty()
            || !integer_part.bytes().all(|b| b.is_ascii_digit())
            || integer_part.len() > 18
        {
            return Err(MoneyError::InvalidDecimalString {
                excerpt: excerpt(raw),
            });
        }
        let fractional_digits = fractional_part.unwrap_or("");
        if let Some(fractional) = fractional_part {
            if fractional.is_empty() {
                return Err(MoneyError::InvalidDecimalString {
                    excerpt: excerpt(raw),
                });
            }
            if !fractional.bytes().all(|b| b.is_ascii_digit()) {
                return Err(MoneyError::InvalidDecimalString {
                    excerpt: excerpt(raw),
                });
            }
        }
        if fractional_digits.len() > FRACTIONAL_DIGITS as usize {
            return Err(MoneyError::TooManyFractionalDigits {
                max: FRACTIONAL_DIGITS,
                actual: fractional_digits.len(),
            });
        }
        let units: i128 = integer_part.parse().map_err(|_| MoneyError::OutOfRange {
            excerpt: excerpt(raw),
        })?;
        let mut padded = String::with_capacity(FRACTIONAL_DIGITS as usize);
        padded.push_str(fractional_digits);
        while padded.len() < FRACTIONAL_DIGITS as usize {
            padded.push('0');
        }
        let frac: i128 = padded
            .parse()
            .map_err(|_| MoneyError::InvalidDecimalString {
                excerpt: excerpt(raw),
            })?;
        let magnitude = units * (MICROS_PER_UNIT as i128) + frac;
        let signed = if negative { -magnitude } else { magnitude };
        let micros = i64::try_from(signed).map_err(|_| MoneyError::OutOfRange {
            excerpt: excerpt(raw),
        })?;
        Ok(Self { currency, micros })
    }

    /// True when the amount is exactly zero.
    pub const fn is_zero(self) -> bool {
        self.micros == 0
    }

    /// True when the amount is negative.
    pub const fn is_negative(self) -> bool {
        self.micros < 0
    }

    /// True when the amount is positive.
    pub const fn is_positive(self) -> bool {
        self.micros > 0
    }

    /// Checked negation (`i64::MIN` micro-units has no positive form).
    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        let micros = self.micros.checked_neg().ok_or(MoneyError::Overflow)?;
        Ok(Self {
            currency: self.currency,
            micros,
        })
    }

    /// Checked addition; currencies must match.
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        self.require_same_currency(other)?;
        let sum = (self.micros as i128) + (other.micros as i128);
        let micros = i64::try_from(sum).map_err(|_| MoneyError::Overflow)?;
        Ok(Self {
            currency: self.currency,
            micros,
        })
    }

    /// Checked subtraction; currencies must match.
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        self.require_same_currency(other)?;
        let difference = (self.micros as i128) - (other.micros as i128);
        let micros = i64::try_from(difference).map_err(|_| MoneyError::Overflow)?;
        Ok(Self {
            currency: self.currency,
            micros,
        })
    }

    /// Exact multiplication by a quantity, checked against `i64::MAX`
    /// micro-units.
    pub fn checked_mul_quantity(self, quantity: Quantity) -> Result<Self, MoneyError> {
        let product = (self.micros as i128) * (quantity.as_u64() as i128);
        let micros = i64::try_from(product).map_err(|_| MoneyError::Overflow)?;
        Ok(Self {
            currency: self.currency,
            micros,
        })
    }

    /// Scale by basis points with round-half-away-from-zero.
    ///
    /// Integer-only: `micros * bp / 10000`, rounded to the nearest
    /// micro-unit, half away from zero.
    pub fn checked_mul_basis_points(self, basis_points: u16) -> Result<Self, MoneyError> {
        if basis_points > MAX_BASIS_POINTS {
            return Err(MoneyError::BasisPointsOutOfRange {
                max: MAX_BASIS_POINTS,
                actual: basis_points,
            });
        }
        let numerator = (self.micros as i128) * (basis_points as i128);
        let micros = i64::try_from(div_round_half_away(numerator, 10_000))
            .map_err(|_| MoneyError::Overflow)?;
        Ok(Self {
            currency: self.currency,
            micros,
        })
    }

    fn require_same_currency(self, other: Self) -> Result<(), MoneyError> {
        if self.currency != other.currency {
            return Err(MoneyError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            });
        }
        Ok(())
    }
}

/// Integer division rounded to the nearest unit, half away from zero.
pub(crate) fn div_round_half_away(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        -((-numerator + denominator / 2) / denominator)
    }
}

impl PartialOrd for Money {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if self.currency != other.currency {
            return None;
        }
        Some(self.micros.cmp(&other.micros))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.to_decimal_string(), self.currency)
    }
}

impl Serialize for Money {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_struct("Money", 2)?;
        map.serialize_field("currency", &self.currency)?;
        map.serialize_field("amount", &self.to_decimal_string())?;
        map.end()
    }
}

struct MicrosValue(i64);

impl<'de> Deserialize<'de> for MicrosValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MicrosVisitor;

        impl<'de> Visitor<'de> for MicrosVisitor {
            type Value = MicrosValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an integer count of micro-units (as a string or an integer)")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(MicrosValue(value))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                i64::try_from(value)
                    .map(MicrosValue)
                    .map_err(|_| E::custom("micros does not fit i64"))
            }

            fn visit_i128<E: de::Error>(self, value: i128) -> Result<Self::Value, E> {
                i64::try_from(value)
                    .map(MicrosValue)
                    .map_err(|_| E::custom("micros does not fit i64"))
            }

            fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
                i64::try_from(value)
                    .map(MicrosValue)
                    .map_err(|_| E::custom("micros does not fit i64"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value
                    .parse::<i64>()
                    .map(MicrosValue)
                    .map_err(|_| E::custom("micros string is not an integer"))
            }

            fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
                Err(E::custom(
                    "micros must be an integer; floating-point money is never accepted",
                ))
            }
        }

        deserializer.deserialize_any(MicrosVisitor)
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MoneyVisitor;

        impl<'de> Visitor<'de> for MoneyVisitor {
            type Value = Money;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a money object such as {\"currency\":\"CNY\",\"amount\":\"0.003700\"}")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut currency: Option<Currency> = None;
                let mut amount: Option<String> = None;
                let mut micros: Option<i64> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "currency" => {
                            if currency.is_some() {
                                return Err(de::Error::duplicate_field("currency"));
                            }
                            currency = Some(map.next_value()?);
                        }
                        "amount" => {
                            if amount.is_some() {
                                return Err(de::Error::duplicate_field("amount"));
                            }
                            amount = Some(map.next_value()?);
                        }
                        "micros" => {
                            if micros.is_some() {
                                return Err(de::Error::duplicate_field("micros"));
                            }
                            micros = Some(map.next_value::<MicrosValue>()?.0);
                        }
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["currency", "amount", "micros"],
                            ));
                        }
                    }
                }
                let currency = currency.ok_or_else(|| de::Error::missing_field("currency"))?;
                match (amount, micros) {
                    (Some(amount), None) => {
                        Money::parse(currency, &amount).map_err(de::Error::custom)
                    }
                    (None, Some(micros)) => Ok(Money::from_micros(currency, micros)),
                    (Some(_), Some(_)) => Err(de::Error::custom(
                        "money carries both `amount` and `micros`; provide exactly one",
                    )),
                    (None, None) => Err(de::Error::missing_field("amount")),
                }
            }
        }

        deserializer.deserialize_map(MoneyVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantity::Quantity;

    #[test]
    fn currency_validates_and_round_trips() {
        assert_eq!(Currency::new("CNY").expect("valid").as_str(), "CNY");
        assert!(Currency::new("cny").is_err());
        assert!(Currency::new("CN").is_err());
        assert!(Currency::new("CNYY").is_err());
        assert!(Currency::new("C1Y").is_err());
        assert!(Currency::new("").is_err());
        assert_eq!(Currency::CNY.to_string(), "CNY");
        assert_eq!(
            serde_json::from_str::<Currency>("\"USD\"").expect("valid"),
            Currency::USD
        );
        assert!(serde_json::from_str::<Currency>("\"usd\"").is_err());
        assert!(serde_json::from_str::<Currency>("840").is_err());
    }

    #[test]
    fn decimal_string_round_trips_exactly() {
        let cases: &[(i64, &str)] = &[
            (0, "0.000000"),
            (1, "0.000001"),
            (3_700, "0.003700"),
            (18_200_000, "18.200000"),
            (91_000_000_000, "91000.000000"),
            (-1, "-0.000001"),
            (i64::MAX, "9223372036854.775807"),
            (i64::MIN, "-9223372036854.775808"),
        ];
        for (micros, expected) in cases {
            let money = Money::from_micros(Currency::CNY, *micros);
            assert_eq!(&money.to_decimal_string(), expected);
            assert_eq!(
                Money::parse(Currency::CNY, expected).expect("parse"),
                money,
                "round trip for {expected}"
            );
        }
    }

    #[test]
    fn spec_example_micros_are_exact() {
        let money = Money::from_micros(Currency::CNY, 3_700);
        assert_eq!(money.to_decimal_string(), "0.003700");
        assert_eq!(money.micros, 3_700);
        let y1820 = Money::parse(Currency::CNY, "18.20").expect("parse");
        assert_eq!(y1820.micros, 18_200_000);
        let line = y1820
            .checked_mul_quantity(Quantity::new(5_000).expect("qty"))
            .expect("exact");
        assert_eq!(line.to_decimal_string(), "91000.000000");
        assert_eq!(line.micros, 91_000_000_000);
    }

    #[test]
    fn parse_rejects_non_exact_and_malformed_inputs() {
        for raw in [
            "",
            " ",
            "1 ",
            "+1",
            "1.",
            ".5",
            "1e5",
            "1E5",
            "0x10",
            "1,5",
            "1_000",
            "NaN",
            "inf",
            "--1",
            "1.2.3",
            "12.3456789",
            "99999999999999999999",
            "9223372036854.775808",
            "-9223372036854.775809",
            "１２３",
        ] {
            assert!(
                Money::parse(Currency::CNY, raw).is_err(),
                "{raw:?} must be rejected"
            );
        }
        assert_eq!(
            Money::parse(Currency::CNY, "-0.000000")
                .expect("valid")
                .micros,
            0
        );
        assert_eq!(
            Money::parse(Currency::CNY, "0.1").expect("valid").micros,
            100_000
        );
        assert_eq!(
            Money::parse(Currency::CNY, "1").expect("valid").micros,
            MICROS_PER_UNIT
        );
    }

    #[test]
    fn arithmetic_is_checked_and_typed() {
        let a = Money::from_micros(Currency::CNY, 100);
        let b = Money::from_micros(Currency::USD, 100);
        assert!(matches!(
            a.checked_add(b),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
        assert!(a.partial_cmp(&b).is_none());
        assert!(a.partial_cmp(&a).is_some());

        let max = Money::from_micros(Currency::CNY, i64::MAX);
        assert!(matches!(
            max.checked_add(Money::from_micros(Currency::CNY, 1)),
            Err(MoneyError::Overflow)
        ));
        let min = Money::from_micros(Currency::CNY, i64::MIN);
        assert!(matches!(
            min.checked_sub(Money::from_micros(Currency::CNY, 1)),
            Err(MoneyError::Overflow)
        ));
        assert!(matches!(min.checked_neg(), Err(MoneyError::Overflow)));
        assert!(matches!(
            max.checked_mul_quantity(Quantity::new(2).expect("qty")),
            Err(MoneyError::Overflow)
        ));
        assert!(matches!(
            a.checked_mul_basis_points(10_001),
            Err(MoneyError::BasisPointsOutOfRange { .. })
        ));
        assert_eq!(
            Money::from_micros(Currency::CNY, 1)
                .checked_mul_basis_points(5_000)
                .expect("exact")
                .micros,
            1,
            "round half away from zero"
        );
        assert_eq!(
            Money::from_micros(Currency::CNY, -1)
                .checked_mul_basis_points(5_000)
                .expect("exact")
                .micros,
            -1
        );
        assert_eq!(
            Money::from_micros(Currency::CNY, 20_000_000)
                .checked_mul_basis_points(9_100)
                .expect("exact")
                .micros,
            18_200_000
        );
    }

    #[test]
    fn serde_round_trip_uses_decimal_strings_never_numbers() {
        let money = Money::from_micros(Currency::CNY, 3_700);
        let json = serde_json::to_value(money).expect("serialize");
        assert_eq!(json["currency"], "CNY");
        assert_eq!(json["amount"], "0.003700");
        assert!(json["amount"].is_string());
        assert_eq!(
            serde_json::from_value::<Money>(json).expect("round trip"),
            money
        );

        for micros in [0i64, 1, 3_700, i64::MAX, i64::MIN] {
            let money = Money::from_micros(Currency::CNY, micros);
            let json = serde_json::to_string(&money).expect("serialize");
            let back = serde_json::from_str::<Money>(&json).expect("round trip");
            assert_eq!(back, money);
        }
    }

    #[test]
    fn serde_accepts_legacy_micros_but_never_confuses_units() {
        let from_amount: Money =
            serde_json::from_str(r#"{"currency":"CNY","amount":"3700"}"#).expect("amount is major");
        assert_eq!(from_amount.micros, 3_700_000_000);
        let from_micros: Money =
            serde_json::from_str(r#"{"currency":"CNY","micros":"3700"}"#).expect("micros form");
        assert_eq!(from_micros.micros, 3_700);
        let from_int: Money =
            serde_json::from_str(r#"{"currency":"CNY","micros":3700}"#).expect("micros int");
        assert_eq!(from_int, from_micros);
        assert_eq!(from_amount.to_decimal_string(), "3700.000000");
        assert_eq!(from_micros.to_decimal_string(), "0.003700");
    }

    #[test]
    fn serde_rejects_hostile_money_documents() {
        for document in [
            r#"{"currency":"CNY","amount":0.1}"#,
            r#"{"currency":"CNY","amount":3700}"#,
            r#"{"currency":"CNY","micros":1.5}"#,
            r#"{"currency":"CNY","micros":"1.5"}"#,
            r#"{"currency":"CNY"}"#,
            r#"{"amount":"1.000000"}"#,
            r#"{"currency":"CNY","amount":"1.000000","micros":"1000000"}"#,
            r#"{"currency":"CNY","amount":"1.000000","extra":true}"#,
            r#"{"currency":"CNY","amount":"1.0000001"}"#,
            r#"{"currency":"cny","amount":"1.000000"}"#,
            r#"{"currency":"CNY","amount":"1e-6"}"#,
            r#""1.000000""#,
            "1.000000",
            r#"{"currency":"CNY","amount":"1.000000","amount":"2.000000"}"#,
            r#"{"currency":"CNY","micros":9223372036854775808}"#,
            r#"{"currency":"CNY","micros":"99999999999999999999"}"#,
        ] {
            assert!(
                serde_json::from_str::<Money>(document).is_err(),
                "{document} must be rejected"
            );
        }
    }

    #[test]
    fn display_is_the_canonical_decimal() {
        assert_eq!(
            Money::from_micros(Currency::CNY, 3_700).to_string(),
            "0.003700 CNY"
        );
    }
}
