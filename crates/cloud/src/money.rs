//! Native-protocol wire representation of monetary micro-unit amounts.
//!
//! The runtime holds money as `u64` micro-units (microUSD). On the wire a
//! monetary field is a **decimal string**, never a JSON number: a JavaScript
//! `number` is integer-exact only to 2^53-1, while credit amounts are served
//! up to `i64::MAX` and the domain itself is `u64`.
//!
//! The rule is the FIELD NAME: any key carrying the money token — snake_case
//! `*_micro` (`granted_micro`, `max_managed_spend_micro_per_period`) or
//! camelCase `*Micro` (`spentCostMicro`) — is a quoted decimal string.
//! Every other number (ids, sequences, cursors, counts, tokens, latencies,
//! timestamps) stays a JSON number.
//!
//! Deserialization accepts BOTH the decimal string and a legacy JSON integer
//! for compatibility with pre-encoding clients; malformed, negative or
//! overflowing values are typed errors.

use std::collections::BTreeMap;

use serde::de::{self, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serializer};

/// Whether a field/JSON key carries the monetary micro-unit token:
/// snake_case `*_micro` or camelCase `*Micro`/`*micro`.
pub fn is_money_field(name: &str) -> bool {
    name.contains("_micro") || name.ends_with("Micro") || name.ends_with("micro")
}

/// Serialize one money amount as its decimal string.
pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Serialize one optional money amount (`None` = JSON null).
pub fn serialize_opt<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serializer.serialize_str(&value.to_string()),
        None => serializer.serialize_none(),
    }
}

/// Deserialize one money amount from a decimal string or a legacy JSON
/// integer.
pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    Wire::deserialize(deserializer).map(|wire| wire.0)
}

/// Optional-money forms for `#[serde(with = "…::money::option")]`.
pub mod option {
    use super::{Deserialize, Deserializer, Serializer, Wire};

    pub fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        super::serialize_opt(value, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<Wire>::deserialize(deserializer).map(|wire| wire.map(|wire| wire.0))
    }
}

/// Serialize a string-keyed amount map whose money-named entries are decimal
/// strings while every other entry stays a JSON number (the plan `limits`
/// map mixes token counts and monetary limits).
pub fn serialize_money_map<S>(map: &BTreeMap<String, u64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut out = serializer.serialize_map(Some(map.len()))?;
    for (key, value) in map {
        if is_money_field(key) {
            out.serialize_entry(key, &value.to_string())?;
        } else {
            out.serialize_entry(key, value)?;
        }
    }
    out.end()
}

/// The wire JSON value of one money amount: the decimal string.
pub fn json(value: u64) -> serde_json::Value {
    serde_json::Value::String(value.to_string())
}

/// The wire JSON value of one optional money amount (`None` = JSON null).
pub fn json_opt(value: Option<u64>) -> serde_json::Value {
    value.map_or(serde_json::Value::Null, json)
}

/// Read a money amount back from a wire JSON value (decimal string or legacy
/// integer). Non-money shapes are `None`.
pub fn from_json(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(raw) => parse_decimal(raw).ok(),
        serde_json::Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

/// Recursively rewrite every money-named key of a JSON value to its decimal
/// string form. Used at the native boundary for response values that embed
/// opaque durable JSON (routing decisions, tournament rows) whose source
/// types are shared with non-protocol wire shapes; non-money keys are never
/// touched.
pub fn stringify_money_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_money_field(key) {
                    if let serde_json::Value::Number(number) = entry {
                        if let Some(raw) = number.as_u64() {
                            *entry = serde_json::Value::String(raw.to_string());
                            continue;
                        }
                    }
                }
                stringify_money_fields(entry);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                stringify_money_fields(item);
            }
        }
        _ => {}
    }
}

/// The decimal-string parser: ASCII digits only (no sign, whitespace,
/// separators or exponent), bounded by the `u64` range.
fn parse_decimal(raw: &str) -> Result<u64, ()> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    raw.parse().map_err(|_| ())
}

struct Wire(u64);

impl<'de> Deserialize<'de> for Wire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(WireVisitor)
    }
}

struct WireVisitor;

impl Visitor<'_> for WireVisitor {
    type Value = Wire;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a decimal string or JSON integer within the u64 range")
    }

    fn visit_str<E>(self, raw: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        parse_decimal(raw)
            .map(Wire)
            .map_err(|()| E::invalid_value(de::Unexpected::Str(raw), &"a decimal u64 string"))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Wire(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        u64::try_from(value).map(Wire).map_err(|_| {
            E::invalid_value(
                de::Unexpected::Signed(value),
                &"a non-negative decimal amount",
            )
        })
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Err(E::invalid_type(
            de::Unexpected::Float(value),
            &"a decimal u64 string",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::{CreditBalance, EntitlementSnapshot, UsageEvent, UsageUnit};
    use crate::ids::{BillingAccountId, OrganizationId, UsageEventId};

    const BOUNDARIES: [u64; 4] = [0, (1 << 53) - 1, 1 << 53, i64::MAX as u64];

    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Probe {
        #[serde(with = "crate::money")]
        amount_micro: u64,
        #[serde(default, with = "crate::money::option")]
        cap_micro: Option<u64>,
        count: u64,
    }

    fn assert_money_strings(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, entry) in map {
                    if is_money_field(key) {
                        let raw = entry.as_str().unwrap_or_else(|| {
                            panic!("money field {key:?} is not a string: {entry}")
                        });
                        assert!(raw.bytes().all(|b| b.is_ascii_digit()), "{key:?}={raw}");
                        raw.parse::<u64>().unwrap();
                    }
                    assert_money_strings(entry);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_money_strings(item);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn money_fields_serialize_as_quoted_decimal_strings_at_every_boundary() {
        for amount in BOUNDARIES {
            let probe = Probe {
                amount_micro: amount,
                cap_micro: Some(amount),
                count: 7,
            };
            let value = serde_json::to_value(&probe).unwrap();
            assert_eq!(value["amount_micro"], serde_json::json!(amount.to_string()));
            assert_eq!(value["cap_micro"], serde_json::json!(amount.to_string()));
            assert_eq!(value["count"], serde_json::json!(7), "counts stay numbers");
            assert!(value["amount_micro"].is_string());
            assert!(value["cap_micro"].is_string());
            assert_money_strings(&value);
            // Round-trip is exact in both representations.
            let back: Probe = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(back, probe);
            let legacy = serde_json::json!({
                "amount_micro": amount,
                "cap_micro": amount,
                "count": 7,
            });
            let back: Probe = serde_json::from_value(legacy).unwrap();
            assert_eq!(back, probe, "legacy numbers still decode");
        }
        // u64::MAX is representable on the wire (credit amounts are bounded
        // to i64::MAX by the domain, but the encoding itself is u64-wide).
        let max = Probe {
            amount_micro: u64::MAX,
            cap_micro: None,
            count: 0,
        };
        let value = serde_json::to_value(&max).unwrap();
        assert_eq!(
            value["amount_micro"],
            serde_json::json!(u64::MAX.to_string())
        );
        assert_eq!(value["cap_micro"], serde_json::Value::Null);
        assert_eq!(serde_json::from_value::<Probe>(value).unwrap(), max);
        let absent = serde_json::json!({"amount_micro": "1", "count": 0});
        assert_eq!(
            serde_json::from_value::<Probe>(absent).unwrap().cap_micro,
            None
        );
        let null = serde_json::json!({"amount_micro": "1", "cap_micro": null, "count": 0});
        assert_eq!(
            serde_json::from_value::<Probe>(null).unwrap().cap_micro,
            None
        );
    }

    #[test]
    fn malformed_negative_and_overflowing_money_is_refused_typed() {
        for bad in [
            serde_json::json!({"amount_micro": "abc"}),
            serde_json::json!({"amount_micro": ""}),
            serde_json::json!({"amount_micro": "-1"}),
            serde_json::json!({"amount_micro": "+1"}),
            serde_json::json!({"amount_micro": "1.5"}),
            serde_json::json!({"amount_micro": " 1"}),
            serde_json::json!({"amount_micro": "1 "}),
            serde_json::json!({"amount_micro": "1e3"}),
            serde_json::json!({"amount_micro": "18446744073709551616"}),
            serde_json::json!({"amount_micro": -1}),
            serde_json::json!({"amount_micro": 1.5}),
            serde_json::json!({"amount_micro": 1e3}),
            serde_json::json!({"amount_micro": true}),
            serde_json::json!({"amount_micro": null}),
            serde_json::json!({"amount_micro": []}),
        ] {
            let err = serde_json::from_value::<Probe>(bad.clone())
                .expect_err(&format!("must refuse {bad}"));
            assert!(
                err.to_string().contains("decimal"),
                "the refusal names the expected decimal form: {err}"
            );
        }
    }

    #[test]
    fn the_money_token_rule_never_touches_non_money_numbers() {
        for money in [
            "granted_micro",
            "consumed_micro",
            "refunded_micro",
            "held_micro",
            "provider_cost_micro",
            "managed_cost_micro",
            "byok_cost_micro",
            "managed_spend_micro",
            "byok_spend_micro",
            "amount_micro",
            "max_managed_spend_micro_per_period",
            "min_credit_balance_micro",
            "predictedMicro",
            "spentMicro",
            "providerReportedMicro",
            "settledCostMicro",
            "maxCostMicro",
            "spentCostMicro",
            "openReservedMicro",
            "uncertainReservedMicro",
        ] {
            assert!(is_money_field(money), "{money} carries the money token");
        }
        for other in [
            "id",
            "event_seq",
            "seq",
            "cursor",
            "input_tokens",
            "output_tokens",
            "pending_consumes",
            "events",
            "corrected_events",
            "count",
            "open_reservations",
            "settled_count",
            "estimated_latency_ms",
            "occurred_at_ms",
            "now_ms",
            "microscope",
        ] {
            assert!(!is_money_field(other), "{other} is not money");
        }
    }

    #[test]
    fn credit_balance_and_entitlement_snapshot_money_is_stringified() {
        let organization = OrganizationId::try_new("org_money").unwrap();
        for amount in BOUNDARIES {
            let balance = CreditBalance {
                granted_micro: amount,
                consumed_micro: amount,
                refunded_micro: amount,
                held_micro: amount,
                pending_consumes: 2,
            };
            let value = serde_json::to_value(balance).unwrap();
            assert_money_strings(&value);
            assert_eq!(value["pending_consumes"], serde_json::json!(2));

            let snapshot = EntitlementSnapshot {
                organization_id: organization.clone(),
                billing_account_id: None,
                plan_id: Some("pro".into()),
                plan_found: true,
                subscription_status: None,
                subscription_expires_ms: None,
                subscription_active: false,
                features: Default::default(),
                limits: [
                    ("max_active_tasks".to_string(), 3u64),
                    ("max_managed_spend_micro_per_period".to_string(), amount),
                    ("min_credit_balance_micro".to_string(), amount),
                ]
                .into_iter()
                .collect(),
                credits: balance,
                managed_spend_micro: amount,
                byok_spend_micro: amount,
                total_tokens: 5,
                in_flight: Vec::new(),
                now_ms: 1,
            };
            let value = serde_json::to_value(&snapshot).unwrap();
            assert_money_strings(&value);
            assert_eq!(
                value["limits"]["max_active_tasks"],
                serde_json::json!(3),
                "non-money limits stay numbers"
            );
            assert_eq!(
                value["limits"]["max_managed_spend_micro_per_period"],
                serde_json::json!(amount.to_string())
            );
            assert_eq!(value["total_tokens"], serde_json::json!(5));
        }
    }

    #[test]
    fn usage_event_money_round_trips_as_string_and_legacy_number() {
        let organization = OrganizationId::try_new("org_usage").unwrap();
        for amount in BOUNDARIES {
            let event = UsageEvent {
                id: UsageEventId::try_new("uev_1").unwrap(),
                organization_id: organization.clone(),
                billing_account_id: BillingAccountId::try_new("acct_1").unwrap(),
                task_id: 42,
                run_id: "run-1".into(),
                attempt_id: "attempt-1".into(),
                provider: "managed-provider".into(),
                model: "m".into(),
                unit: UsageUnit::ProviderCostMicro,
                quantity: 1,
                provider_cost_micro: amount,
                source_operation: "settle".into(),
                occurred_at_ms: 1,
                reconciliation_state: crate::billing::ReconciliationState::Pending,
                correction_of: None,
                category: crate::billing::SpendCategory::Managed,
                source_key: "reservation:1:provider_cost_micro".into(),
            };
            let value = serde_json::to_value(&event).unwrap();
            assert_money_strings(&value);
            assert_eq!(
                value["provider_cost_micro"],
                serde_json::json!(amount.to_string())
            );
            assert_eq!(value["quantity"], serde_json::json!(1));
            let back: UsageEvent = serde_json::from_value(value).unwrap();
            assert_eq!(back, event, "round-trip exact");
            let mut legacy = serde_json::to_value(&event).unwrap();
            legacy["provider_cost_micro"] = serde_json::json!(amount);
            let back: UsageEvent = serde_json::from_value(legacy).unwrap();
            assert_eq!(back, event, "legacy numeric payloads still decode");
        }
    }

    #[test]
    fn stringify_money_fields_walks_nested_values_and_spares_other_numbers() {
        let mut value = serde_json::json!({
            "provider_cost_micro": 9007199254740993u64,
            "input_tokens": 7,
            "nested": {
                "estimated_cost_micro": 5,
                "estimated_latency_ms": 9,
                "items": [
                    {"predictedMicro": 11, "count": 2},
                    {"spentCostMicro": "3", "count": 4},
                ],
            },
        });
        stringify_money_fields(&mut value);
        assert_money_strings(&value);
        assert_eq!(
            value["provider_cost_micro"],
            serde_json::json!("9007199254740993")
        );
        assert_eq!(
            value["nested"]["estimated_cost_micro"],
            serde_json::json!("5")
        );
        assert_eq!(
            value["nested"]["items"][0]["predictedMicro"],
            serde_json::json!("11")
        );
        assert_eq!(
            value["nested"]["items"][1]["spentCostMicro"],
            serde_json::json!("3")
        );
        assert_eq!(value["input_tokens"], serde_json::json!(7));
        assert_eq!(
            value["nested"]["estimated_latency_ms"],
            serde_json::json!(9)
        );
        assert_eq!(value["nested"]["items"][0]["count"], serde_json::json!(2));
    }

    #[test]
    fn json_helpers_emit_and_read_the_wire_forms() {
        assert_eq!(json(0), serde_json::json!("0"));
        assert_eq!(
            json(i64::MAX as u64),
            serde_json::json!(i64::MAX.to_string())
        );
        assert_eq!(json_opt(None), serde_json::Value::Null);
        assert_eq!(json_opt(Some(2)), serde_json::json!("2"));
        assert_eq!(from_json(&serde_json::json!("7")), Some(7));
        assert_eq!(from_json(&serde_json::json!(7)), Some(7));
        assert_eq!(from_json(&serde_json::json!(null)), None);
        assert_eq!(from_json(&serde_json::json!("-7")), None);
        assert_eq!(from_json(&serde_json::json!(1.5)), None);
    }
}
