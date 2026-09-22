//! Strategy extraction for 1688: browser-network JSON first, then embedded
//! application state, then structural DOM, then rendered text.
//!
//! Each strategy produces a [`Draft`]: bounded field observations plus the
//! variant matrix it found, or a typed
//! [`StrategyOutcome::NotApplicable`] / [`StrategyOutcome::SchemaMismatch`].
//! Nothing here reconciles: the connector does that in one place
//! (`crate::contract::extract::reconcile`), so the documented authority
//! ordering is applied identically to every field.

use serde_json::Value;

use faktor_commerce::money::{Currency, Money};
use faktor_commerce::offer::{ObservationOrigin, StockState, VariantAttribute};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{Text, VariantId};
use faktor_commerce::{SourceError, SourceId};

use super::dictionary;
use crate::contract::dom;
use crate::contract::extract::{
    parse_variant_spec, Field, FieldObservation, FieldValue, Fingerprint, SchemaDrift, Strategy,
    StrategyOutcome, VariantEntry,
};
use crate::normalize;

/// At most this many embedded-state bytes are scanned.
pub const MAX_EMBEDDED_STATE_BYTES: usize = 512 * 1024;
/// At most this many SKUs are admitted from one strategy.
const MAX_SKUS: usize = crate::contract::extract::MAX_VARIANTS;

/// One strategy's extraction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The strategy.
    pub strategy: Strategy,
    /// The outcome.
    pub outcome: StrategyOutcome,
    /// The field observations it produced.
    pub observations: Vec<FieldObservation>,
    /// The variant matrix it found.
    pub variants: Vec<VariantEntry>,
    /// Offer-level price tiers (`min_quantity`, unit price).
    pub tiers: Vec<(NonZeroQuantity, Money)>,
    /// The structural fingerprint of the payload (when it has one).
    pub fingerprint: Option<Fingerprint>,
}

impl Draft {
    fn not_applicable(strategy: Strategy) -> Self {
        Self {
            strategy,
            outcome: StrategyOutcome::NotApplicable,
            observations: Vec::new(),
            variants: Vec::new(),
            tiers: Vec::new(),
            fingerprint: None,
        }
    }

    fn schema(
        strategy: Strategy,
        expected: Option<Fingerprint>,
        observed: Fingerprint,
        missing: Vec<String>,
    ) -> Self {
        let drift = match expected {
            Some(expected) if expected != observed => SchemaDrift::new(expected, observed, missing),
            _ => SchemaDrift::new(observed, observed, missing),
        };
        Self {
            strategy,
            outcome: StrategyOutcome::SchemaMismatch(drift),
            observations: Vec::new(),
            variants: Vec::new(),
            tiers: Vec::new(),
            fingerprint: Some(observed),
        }
    }

    fn success(strategy: Strategy, fingerprint: Option<Fingerprint>) -> Self {
        Self {
            strategy,
            outcome: StrategyOutcome::Success,
            observations: Vec::new(),
            variants: Vec::new(),
            tiers: Vec::new(),
            fingerprint,
        }
    }

    fn push(&mut self, source: &SourceId, field: Field, value: FieldValue, now_ms: u64) {
        self.observations.push(FieldObservation::new(
            source.clone(),
            field,
            value,
            self.strategy,
            now_ms,
        ));
    }

    fn push_text(&mut self, source: &SourceId, field: Field, raw: &str, now_ms: u64) {
        if let Ok(text) = Text::<512>::new(raw.trim()) {
            if !text.as_str().is_empty() {
                self.push(source, field, FieldValue::Text(text), now_ms);
            }
        }
    }

    fn push_money(&mut self, source: &SourceId, field: Field, money: Money, now_ms: u64) {
        self.push(source, field, FieldValue::Money(money), now_ms);
    }

    fn push_quantity(&mut self, source: &SourceId, field: Field, value: u64, now_ms: u64) {
        if let Ok(Some(quantity)) = normalize::nonzero_quantity(value) {
            self.push(source, field, FieldValue::Quantity(quantity), now_ms);
        }
    }

    /// True when at least one observation or variant was produced.
    fn has_data(&self) -> bool {
        !self.observations.is_empty() || !self.variants.is_empty()
    }
}

/// Extract from a browser-network JSON payload.
pub fn network(body: &str, source: &SourceId, now_ms: u64, expected: Option<Fingerprint>) -> Draft {
    let strategy = Strategy::NetworkJson;
    if body.trim().is_empty() || body.len() > crate::contract::capture::MAX_CAPTURE_BYTES {
        return Draft::schema(
            strategy,
            expected,
            Fingerprint::of_bytes(body.as_bytes()),
            vec!["<body>".to_string()],
        );
    }
    let value: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => {
            return Draft::schema(
                strategy,
                expected,
                Fingerprint::of_bytes(body.as_bytes()),
                vec!["<json>".to_string()],
            )
        }
    };
    extract_value(&value, source, now_ms, expected)
}

/// Extract a draft per search-result item (bounded), falling back to one
/// draft for a single-offer payload.
pub fn network_items(
    body: &str,
    source: &SourceId,
    now_ms: u64,
    expected: Option<Fingerprint>,
) -> Vec<Draft> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return vec![network(body, source, now_ms, expected)];
    };
    let items = value
        .get("data")
        .and_then(|data| data.get("items"))
        .and_then(Value::as_array);
    match items {
        Some(items) if !items.is_empty() => items
            .iter()
            .take(MAX_SKUS)
            .map(|item| extract_value(item, source, now_ms, expected))
            .collect(),
        _ => vec![extract_value(&value, source, now_ms, expected)],
    }
}

/// Extract from an embedded application-state script block.
pub fn embedded_state(
    html: &str,
    source: &SourceId,
    now_ms: u64,
    expected: Option<Fingerprint>,
) -> Draft {
    let strategy = Strategy::EmbeddedState;
    let Some(block) = find_embedded_json(html) else {
        return Draft::not_applicable(strategy);
    };
    let value: Value = match serde_json::from_str(block) {
        Ok(value) => value,
        Err(_) => {
            return Draft::schema(
                strategy,
                expected,
                Fingerprint::of_bytes(block.as_bytes()),
                vec!["<state-json>".to_string()],
            )
        }
    };
    // The embedded shape is the same documented offer shape; its authority
    // origin differs (the state block), which `Draft::push` records.
    let mut draft = extract_value(&value, source, now_ms, expected);
    draft.strategy = strategy;
    draft.fingerprint = Some(Fingerprint::of_bytes(block.as_bytes()));
    for observation in &mut draft.observations {
        observation.strategy = strategy;
        observation.origin = ObservationOrigin::EmbeddedState;
    }
    draft
}

/// Extract from structural DOM markup.
pub fn structural_dom(
    html: &str,
    source: &SourceId,
    now_ms: u64,
    expected: Option<Fingerprint>,
) -> Draft {
    let strategy = Strategy::StructuralDom;
    let nodes = dom::scan(html);
    let fingerprint = Fingerprint::of_dom(html);
    let container = nodes.iter().find(|node| {
        node.data("offer-id").is_some()
            || node.has_class("offer-detail")
            || node.has_class("offer-container")
    });
    let Some(container) = container else {
        return Draft::not_applicable(strategy);
    };
    let mut missing: Vec<String> = Vec::new();
    if dom::by_tag(&nodes, "h1").is_none() && dom::by_class(&nodes, "offer-title").is_none() {
        missing.push("h1.offer-title".to_string());
    }
    let has_price = dom::text_by_class(&nodes, "price-value").is_some()
        || dom::by_data(&nodes, "price").is_some()
        || dom::by_data(&nodes, "price-low").is_some();
    if !has_price {
        missing.push(".price-value".to_string());
    }
    if !missing.is_empty() {
        return Draft::schema(strategy, expected, fingerprint, missing);
    }
    let mut draft = Draft::success(strategy, Some(fingerprint));
    if let Some(offer_id) = container.data("offer-id") {
        draft.push_text(source, Field::OfferId, offer_id, now_ms);
    }
    if let Some(title) = dom::by_tag(&nodes, "h1").map(|node| node.text.as_str()) {
        draft.push_text(source, Field::Title, title, now_ms);
    } else if let Some(title) = dom::text_by_class(&nodes, "offer-title") {
        draft.push_text(source, Field::Title, title, now_ms);
    }
    if let Some(currency) = container.data("currency") {
        draft.push_text(source, Field::Currency, currency, now_ms);
    }
    if let Some(low) = container.data("price-low") {
        if let Some(money) = dictionary::money_from_text(low) {
            draft.push_money(source, Field::PriceRangeLow, money, now_ms);
        }
    }
    if let Some(high) = container.data("price-high") {
        if let Some(money) = dictionary::money_from_text(high) {
            draft.push_money(source, Field::PriceRangeHigh, money, now_ms);
        }
    }
    let price_text = dom::text_by_class(&nodes, "price-value")
        .or_else(|| dom::by_data(&nodes, "price").and_then(|node| node.data("price")));
    if let Some(price_text) = price_text {
        if dictionary::is_inquiry_text(price_text) {
            draft.push_text(source, Field::InquiryOnly, "inquiry", now_ms);
        } else if let Some(money) = dictionary::money_from_text(price_text) {
            draft.push_money(source, Field::Price, money, now_ms);
        }
    }
    for node in &nodes {
        let Some(field) = dictionary::field_for_label(&node.text)
            .or_else(|| node.data("field").and_then(dictionary::field_for_label))
        else {
            continue;
        };
        let value = node
            .data("value")
            .map(str::to_string)
            .unwrap_or_else(|| labelled_value(&nodes, node).unwrap_or_default());
        apply_labelled(&mut draft, source, field, &value, now_ms);
    }
    draft.variants = variants_from_dom(&nodes, source, now_ms);
    if !draft.has_data() {
        return Draft::not_applicable(strategy);
    }
    draft
}

/// Extract from rendered text (last resort).
pub fn rendered_text(
    text: &str,
    source: &SourceId,
    now_ms: u64,
    _expected: Option<Fingerprint>,
) -> Draft {
    let strategy = Strategy::RenderedText;
    let mut draft = Draft::success(strategy, None);
    let mut first_line = true;
    for line in text.lines().take(512) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if first_line {
            first_line = false;
            let looks_like_title = trimmed.len() <= 200
                && !dictionary::is_inquiry_text(trimmed)
                && !dictionary::FIELD_LABELS
                    .iter()
                    .any(|(label, _)| trimmed.starts_with(label));
            if looks_like_title {
                draft.push_text(source, Field::Title, trimmed, now_ms);
            }
        }
        if dictionary::is_inquiry_text(trimmed) {
            draft.push_text(source, Field::InquiryOnly, "inquiry", now_ms);
            continue;
        }
        for (label, field) in dictionary::FIELD_LABELS {
            let Some(rest) = trimmed.strip_prefix(label) else {
                continue;
            };
            let rest = rest.trim_start_matches([':', '：', ' ', '\t']);
            if rest.is_empty() {
                continue;
            }
            apply_labelled(&mut draft, source, *field, rest, now_ms);
            break;
        }
    }
    if !draft.has_data() {
        return Draft::not_applicable(strategy);
    }
    draft
}

fn apply_labelled(draft: &mut Draft, source: &SourceId, field: Field, raw: &str, now_ms: u64) {
    if raw.trim().is_empty() {
        return;
    }
    if dictionary::is_inquiry_text(raw) {
        draft.push_text(source, Field::InquiryOnly, "inquiry", now_ms);
        return;
    }
    match field {
        Field::Price | Field::TierPrice => {
            if let Some((low, high)) = dictionary::money_range_from_text(raw) {
                draft.push_money(source, Field::PriceRangeLow, low, now_ms);
                draft.push_money(source, Field::PriceRangeHigh, high, now_ms);
            } else if let Some(money) = dictionary::money_from_text(raw) {
                draft.push_money(source, field, money, now_ms);
            }
        }
        Field::MinimumOrder | Field::OrderMultiple => {
            if let Some(quantity) = dictionary::quantity_from_text(raw) {
                draft.push(source, field, FieldValue::Quantity(quantity), now_ms);
            }
        }
        Field::Stock => {
            let state = dictionary::stock_from_text(raw);
            match state {
                StockState::InStock { quantity } => {
                    draft.push(source, field, FieldValue::Quantity(quantity), now_ms);
                }
                StockState::OutOfStock => {
                    draft.push_text(source, field, "缺货", now_ms);
                }
                _ => {}
            }
        }
        other => draft.push_text(source, other, raw, now_ms),
    }
}

/// The value following a `<dt>label</dt><dd>value</dd>` pair.
fn labelled_value(nodes: &[dom::DomNode], label: &dom::DomNode) -> Option<String> {
    let index = nodes.iter().position(|node| node == label)?;
    nodes
        .iter()
        .skip(index + 1)
        .take(2)
        .find(|node| node.tag == "dd" && !node.text.is_empty())
        .map(|node| node.text.clone())
}

fn variants_from_dom(nodes: &[dom::DomNode], source: &SourceId, now_ms: u64) -> Vec<VariantEntry> {
    let mut variants: Vec<VariantEntry> = Vec::new();
    for node in nodes {
        let Some(sku) = node.data("sku") else {
            continue;
        };
        if variants.len() >= MAX_SKUS {
            break;
        }
        let attributes = attributes_from_spec(sku);
        if attributes.is_empty() {
            continue;
        }
        let price = node.data("price").and_then(dictionary::money_from_text);
        let stock = node
            .data("stock")
            .map(dictionary::stock_from_text)
            .unwrap_or(StockState::Unknown);
        let moq = node.data("moq").and_then(dictionary::quantity_from_text);
        if let Ok(entry) = VariantEntry::new(attributes, price, moq, stock) {
            variants.push(entry);
        }
    }
    let _ = (source, now_ms);
    variants
}

fn attributes_from_spec(spec: &str) -> Vec<VariantAttribute> {
    parse_variant_spec(spec)
        .into_iter()
        .filter_map(|(name, value)| {
            let name = Text::<64>::new(&name).ok()?;
            let value = Text::<256>::new(&value).ok()?;
            Some(VariantAttribute { name, value })
        })
        .collect()
}

/// The documented offer object inside an arbitrary envelope.
fn offer_object(value: &Value) -> Option<&Value> {
    if let Some(data) = value.get("data") {
        if let Some(items) = data.get("items").and_then(Value::as_array) {
            return items.first();
        }
        if data.get("offer").is_some() {
            return data.get("offer");
        }
        if data.get("offerId").is_some() || data.get("subject").is_some() {
            return Some(data);
        }
    }
    if value.get("offer").is_some() {
        return value.get("offer");
    }
    if value.get("offerId").is_some() || value.get("subject").is_some() {
        return Some(value);
    }
    None
}

fn extract_value(
    value: &Value,
    source: &SourceId,
    now_ms: u64,
    expected: Option<Fingerprint>,
) -> Draft {
    let strategy = Strategy::NetworkJson;
    let Some(offer) = offer_object(value) else {
        return Draft::not_applicable(strategy);
    };
    let fingerprint = Fingerprint::of_json(offer);
    let inquiry = offer
        .get("priceVisibility")
        .and_then(json_text)
        .is_some_and(|raw| raw.eq_ignore_ascii_case("inquiry") || raw.eq_ignore_ascii_case("rfq"))
        || offer
            .get("priceText")
            .and_then(json_text)
            .is_some_and(|raw| dictionary::is_inquiry_text(&raw));
    let mut missing: Vec<String> = Vec::new();
    if offer.get("offerId").is_none() {
        missing.push("data/offerId".to_string());
    }
    if offer.get("subject").is_none() {
        missing.push("data/subject".to_string());
    }
    let has_price = offer.get("price").is_some()
        || offer.get("priceRange").is_some()
        || offer.get("skuMap").is_some()
        || offer.get("tierPrices").is_some()
        || inquiry;
    if !has_price {
        missing.push("data/price".to_string());
    }
    if !missing.is_empty() {
        if let Some(expected) = expected {
            if expected != fingerprint {
                return Draft::schema(strategy, Some(expected), fingerprint, missing);
            }
        }
        // The payload has a known envelope but is missing fields the
        // extractor requires; that is still a typed drift, never a partial
        // parse of whatever happens to be there.
        return Draft::schema(strategy, expected, fingerprint, missing);
    }
    let mut draft = Draft::success(strategy, Some(fingerprint));
    if let Some(offer_id) = offer.get("offerId").and_then(json_text) {
        draft.push_text(source, Field::OfferId, &offer_id, now_ms);
    }
    if let Some(subject) = offer.get("subject").and_then(json_text) {
        draft.push_text(source, Field::Title, &subject, now_ms);
    }
    if inquiry {
        draft.push_text(source, Field::InquiryOnly, "inquiry", now_ms);
    } else {
        if let Some(currency) = offer.get("currency").and_then(json_text) {
            draft.push_text(source, Field::Currency, &currency, now_ms);
            if !currency.eq_ignore_ascii_case("cny") && !currency.eq_ignore_ascii_case("rmb") {
                // Non-CNY amounts on 1688 are not silently converted.
                draft.outcome = StrategyOutcome::NotApplicable;
                draft.observations.clear();
                draft.variants.clear();
                return draft;
            }
        }
        if let Some(range) = offer.get("priceRange") {
            if let Some(low) = range.get("low").and_then(json_money) {
                draft.push_money(source, Field::PriceRangeLow, low, now_ms);
            }
            if let Some(high) = range.get("high").and_then(json_money) {
                draft.push_money(source, Field::PriceRangeHigh, high, now_ms);
            }
        }
        if let Some(money) = offer.get("price").and_then(json_money) {
            draft.push_money(source, Field::Price, money, now_ms);
        }
        for tier in offer
            .get("tierPrices")
            .and_then(Value::as_array)
            .map(|tiers| tiers.iter().take(16))
            .into_iter()
            .flatten()
        {
            if let (Some(quantity), Some(money)) = (
                tier.get("minQuantity").and_then(json_u64),
                tier.get("price").and_then(json_money),
            ) {
                if let Ok(Some(quantity)) = normalize::nonzero_quantity(quantity) {
                    draft.tiers.push((quantity, money));
                }
            }
        }
        draft.variants = variants_from_sku_map(offer, dictionary::money_from_text);
        if !draft.variants.is_empty() {
            // The variant matrix is the authoritative price source; the
            // headline range stays a hint (never a quote).
            draft.push_text(source, Field::Variant, "matrix", now_ms);
        }
    }
    if let Some(moq) = offer.get("moq").and_then(json_u64) {
        draft.push_quantity(source, Field::MinimumOrder, moq, now_ms);
    }
    if let Some(stock) = offer.get("stock").and_then(json_u64) {
        draft.push_quantity(source, Field::Stock, stock, now_ms);
    }
    if let Some(model) = offer.get("model").and_then(json_text) {
        draft.push_text(source, Field::Model, &model, now_ms);
    }
    if let Some(spec) = offer.get("spec").and_then(json_text) {
        draft.push_text(source, Field::Specification, &spec, now_ms);
    }
    if let Some(seller) = offer.get("seller") {
        if let Some(name) = seller.get("name").and_then(json_text) {
            draft.push_text(source, Field::Supplier, &name, now_ms);
            draft.push_text(source, Field::Manufacturer, &name, now_ms);
        }
    }
    if let Some(factory) = offer.get("factory").and_then(json_text) {
        draft.push_text(source, Field::Manufacturer, &factory, now_ms);
    }
    if let Some(lead_time) = offer.get("leadTime") {
        let min = lead_time.get("minDays").and_then(json_u64);
        let max = lead_time.get("maxDays").and_then(json_u64);
        if let Some(min) = min {
            let max = max.unwrap_or(min);
            draft.push_text(source, Field::LeadTime, &format!("{min}-{max}天"), now_ms);
        }
    }
    if let Some(multiple) = offer.get("orderMultiple").and_then(json_u64) {
        draft.push_quantity(source, Field::OrderMultiple, multiple, now_ms);
    }
    draft
}

/// Build the variant matrix from a `skuMap` object: key is the attribute
/// spec, value states price/MOQ/stock.
fn variants_from_sku_map(
    offer: &Value,
    money_from_text: fn(&str) -> Option<Money>,
) -> Vec<VariantEntry> {
    let Some(map) = offer.get("skuMap").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut variants: Vec<VariantEntry> = Vec::new();
    for (spec, entry) in map.iter().take(MAX_SKUS) {
        let attributes = attributes_from_spec(spec);
        if attributes.is_empty() {
            continue;
        }
        let price = entry
            .get("price")
            .and_then(json_text)
            .and_then(|raw| money_from_text(&raw));
        let moq = entry
            .get("moq")
            .and_then(json_u64)
            .and_then(|value| normalize::nonzero_quantity(value).ok().flatten());
        let stock = entry
            .get("stock")
            .and_then(json_u64)
            .and_then(|value| {
                normalize::nonzero_quantity(value)
                    .ok()
                    .flatten()
                    .map(|quantity| StockState::InStock { quantity })
            })
            .unwrap_or(StockState::Unknown);
        if let Ok(variant) = VariantEntry::new(attributes, price, moq, stock) {
            variants.push(variant);
        }
    }
    variants
}

fn json_text(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) if !raw.trim().is_empty() && raw.len() <= 512 => {
            Some(raw.trim().to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_money(value: &Value) -> Option<Money> {
    match value {
        Value::String(raw) => dictionary::money_from_text(raw),
        Value::Number(number) => {
            normalize::parse_money_text(Currency::CNY, &number.to_string()).ok()
        }
        _ => None,
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) => normalize::parse_leading_u64(raw),
        _ => None,
    }
}

/// Find the first embedded application-state JSON block, bounded.
pub fn find_embedded_json(html: &str) -> Option<&str> {
    const MARKERS: [&str; 4] = [
        "window.__INIT_DATA__",
        "__INIT_DATA__",
        "__NEXT_DATA__",
        "window.__DATA__",
    ];
    for marker in MARKERS {
        let Some(position) = html.find(marker) else {
            continue;
        };
        let tail = &html[position..html.len().min(position + MAX_EMBEDDED_STATE_BYTES)];
        let Some(start) = tail.find('{') else {
            continue;
        };
        if let Some(block) = balanced_object(tail, start) {
            return Some(block);
        }
    }
    None
}

/// A string-aware balanced `{...}` scan, bounded by the slice length.
fn balanced_object(text: &str, start: usize) -> Option<&str> {
    if text.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return text.get(start..=index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The typed error a variant request must produce when the mapping is not
/// unambiguous.
pub fn variant_error() -> SourceError {
    SourceError::VariantAmbiguous
}

/// A variant id for tests and callers that know an attribute spec.
pub fn variant_id_for(spec: &str) -> Result<VariantId, SourceError> {
    let attributes = attributes_from_spec(spec);
    crate::contract::extract::canonical_variant_id(&attributes)
}
