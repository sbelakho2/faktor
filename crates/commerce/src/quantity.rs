//! Bounded purchase quantities.
//!
//! Quantities are integers. The hard bound ([`MAX_ORDER_QUANTITY`]) is the
//! same bound the `source_market` tool schema uses, so a quantity that
//! cannot be requested cannot be represented here either.

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;

/// The largest representable purchase quantity (one billion units).
pub const MAX_ORDER_QUANTITY: u64 = 1_000_000_000;

/// A rejected quantity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuantityError {
    /// Above [`MAX_ORDER_QUANTITY`].
    #[error("quantity {actual} exceeds the maximum {max}")]
    TooLarge { max: u64, actual: u64 },
    /// A negative value.
    #[error("quantity must not be negative")]
    Negative,
    /// Zero where a strictly positive quantity is required.
    #[error("quantity must be at least 1")]
    Zero,
    /// A non-integer JSON value.
    #[error("quantity must be an integer, never a floating-point number")]
    NotAnInteger,
}

fn quantity_from_u128(value: u128) -> Result<u64, QuantityError> {
    if value > MAX_ORDER_QUANTITY as u128 {
        return Err(QuantityError::TooLarge {
            max: MAX_ORDER_QUANTITY,
            actual: u64::MAX,
        });
    }
    Ok(value as u64)
}

fn quantity_from_i128(value: i128) -> Result<u64, QuantityError> {
    if value < 0 {
        return Err(QuantityError::Negative);
    }
    quantity_from_u128(value as u128)
}

/// A quantity in `0..=MAX_ORDER_QUANTITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Quantity(u64);

impl Quantity {
    /// Zero.
    pub const ZERO: Quantity = Quantity(0);
    /// One.
    pub const ONE: Quantity = Quantity(1);

    /// Validate a quantity.
    pub fn new(value: u64) -> Result<Self, QuantityError> {
        if value > MAX_ORDER_QUANTITY {
            return Err(QuantityError::TooLarge {
                max: MAX_ORDER_QUANTITY,
                actual: value,
            });
        }
        Ok(Self(value))
    }

    /// The raw value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// True for zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Round up to the next multiple of `step` (exact at multiples).
    ///
    /// Returns `None` when the result would exceed
    /// [`MAX_ORDER_QUANTITY`].
    pub fn checked_next_multiple(self, step: NonZeroQuantity) -> Option<Self> {
        let step = step.get();
        let remainder = self.0 % step;
        let base = self.0 - remainder;
        let rounded = if remainder == 0 {
            base
        } else {
            base.checked_add(step)?
        };
        if rounded > MAX_ORDER_QUANTITY {
            return None;
        }
        Some(Self(rounded))
    }

    /// Checked multiplication of two quantities.
    pub fn checked_mul(self, other: Self) -> Option<Self> {
        let product = self.0.checked_mul(other.0)?;
        if product > MAX_ORDER_QUANTITY {
            return None;
        }
        Some(Self(product))
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Quantity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Quantity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct QuantityVisitor;

        impl<'de> Visitor<'de> for QuantityVisitor {
            type Value = Quantity;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "an integer quantity in 0..={MAX_ORDER_QUANTITY}")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Quantity::new(value).map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                if value < 0 {
                    return Err(E::custom(QuantityError::Negative));
                }
                Quantity::new(value as u64).map_err(E::custom)
            }

            fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
                quantity_from_u128(value)
                    .and_then(Quantity::new)
                    .map_err(E::custom)
            }

            fn visit_i128<E: de::Error>(self, value: i128) -> Result<Self::Value, E> {
                quantity_from_i128(value)
                    .and_then(Quantity::new)
                    .map_err(E::custom)
            }

            fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
                Err(E::custom(QuantityError::NotAnInteger))
            }
        }

        deserializer.deserialize_any(QuantityVisitor)
    }
}

/// A quantity in `1..=MAX_ORDER_QUANTITY` (MOQ, order multiple, pack size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NonZeroQuantity(u64);

impl NonZeroQuantity {
    /// One.
    pub const ONE: NonZeroQuantity = NonZeroQuantity(1);

    /// Validate a strictly positive quantity.
    pub fn new(value: u64) -> Result<Self, QuantityError> {
        if value == 0 {
            return Err(QuantityError::Zero);
        }
        Quantity::new(value)?;
        Ok(Self(value))
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// As a plain [`Quantity`].
    pub const fn as_quantity(self) -> Quantity {
        Quantity(self.0)
    }
}

impl fmt::Display for NonZeroQuantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for NonZeroQuantity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for NonZeroQuantity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NonZeroVisitor;

        impl<'de> Visitor<'de> for NonZeroVisitor {
            type Value = NonZeroQuantity;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "an integer quantity in 1..={MAX_ORDER_QUANTITY}")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                NonZeroQuantity::new(value).map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                if value < 0 {
                    return Err(E::custom(QuantityError::Negative));
                }
                NonZeroQuantity::new(value as u64).map_err(E::custom)
            }

            fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
                quantity_from_u128(value)
                    .and_then(NonZeroQuantity::new)
                    .map_err(E::custom)
            }

            fn visit_i128<E: de::Error>(self, value: i128) -> Result<Self::Value, E> {
                quantity_from_i128(value)
                    .and_then(NonZeroQuantity::new)
                    .map_err(E::custom)
            }

            fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
                Err(E::custom(QuantityError::NotAnInteger))
            }
        }

        deserializer.deserialize_any(NonZeroVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantity_bounds_are_enforced() {
        assert_eq!(Quantity::new(0).expect("valid").as_u64(), 0);
        assert_eq!(
            Quantity::new(MAX_ORDER_QUANTITY).expect("valid").as_u64(),
            MAX_ORDER_QUANTITY
        );
        assert!(matches!(
            Quantity::new(MAX_ORDER_QUANTITY + 1),
            Err(QuantityError::TooLarge { .. })
        ));
        assert!(matches!(NonZeroQuantity::new(0), Err(QuantityError::Zero)));
        assert_eq!(NonZeroQuantity::new(1).expect("valid").get(), 1);
    }

    #[test]
    fn next_multiple_rounds_up_and_is_exact_at_multiples() {
        let step = NonZeroQuantity::new(100).expect("valid");
        for (input, expected) in [(0u64, 0u64), (1, 100), (99, 100), (100, 100), (101, 200)] {
            assert_eq!(
                Quantity::new(input)
                    .expect("valid")
                    .checked_next_multiple(step)
                    .expect("fits")
                    .as_u64(),
                expected,
                "input {input}"
            );
        }
        let huge = Quantity::new(MAX_ORDER_QUANTITY).expect("valid");
        assert!(huge
            .checked_next_multiple(NonZeroQuantity::new(MAX_ORDER_QUANTITY - 1).expect("valid"))
            .is_none());
    }

    #[test]
    fn serde_rejects_floats_strings_and_out_of_range() {
        assert_eq!(
            serde_json::from_str::<Quantity>("5").expect("valid"),
            Quantity::new(5).expect("valid")
        );
        assert_eq!(
            serde_json::from_str::<NonZeroQuantity>("1000000000").expect("valid"),
            NonZeroQuantity::new(MAX_ORDER_QUANTITY).expect("valid")
        );
        for document in ["1.5", "-1", "1000000001", "\"5\"", "null", "true", "0.0"] {
            assert!(
                serde_json::from_str::<Quantity>(document).is_err(),
                "{document} must be rejected"
            );
        }
        assert!(serde_json::from_str::<NonZeroQuantity>("0").is_err());
        assert!(serde_json::from_str::<NonZeroQuantity>("1.0").is_err());
    }
}
