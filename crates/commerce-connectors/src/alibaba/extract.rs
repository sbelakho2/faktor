//! Strategy extraction for Alibaba.com — separate from the 1688 extractor.
//!
//! Alibaba's buyer-visible payloads use their own envelopes, field names and
//! vocabulary (`result.products[]`, `data.product`, `skuOptions`,
//! `priceTiers`, supplier profiles). Mixing the two sites' parsers would be
//! exactly the sort of hidden coupling the spec forbids, so this module is
//! deliberately independent: only the reconciliation vocabulary
//! (`crate::contract::extract`) is shared.

use serde_json::Value;

use faktor_commerce::money::Currency;
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
    /// Offer-level price tiers (`ladder`, unit price).
    pub tiers: Vec<(NonZeroQuantity, faktor_commerce::Money)>,
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

    fn push_money(
        &mut self,
        source: &SourceId,
        field: Field,
        money: faktor_commerce::Money,
        now_ms: u64,
    ) {
        self.push(source, field, FieldValue::Money(money), now_ms);
    }

    fn push_quantity(&mut self, source: &SourceId, field: Field, value: u64, now_ms: u64) {
        if let Ok(Some(quantity)) = normalize::nonzero_quantity(value) {
            self.push(source, field, FieldValue::Quantity(quantity), now_ms);
        }
    }

    fn has_data(&self) -> bool {
        !self.observations.is_empty() || !self.variants.is_empty() || !self.tiers.is_empty()
    }
}

/// Parse one exact amount from buyer-visible text (`US $1.20`, `$1.20`,
/// `¥18.20`, `CNY 100`). The currency is taken from the token's own symbol or
/// ISO code — a bare amount declares nothing and yields no observation, and
/// an affix that contradicts another affix is refused. It is never silently
/// read as USD.
pub fn money_from_text(raw: &str) -> Option<faktor_commerce::Money> {
    // Buyer-visible text often separates the code from the symbol
    // (`US $1.20`); normalize that documented spelling, never guess.
    let trimmed = raw.trim().replace("US $", "US$").replace("CN ¥", "CN¥");
    let (declared, body) = normalize::money_declared_currency(&trimmed).ok()?;
    let currency = declared?;
    normalize::money_from_body(currency, &body).ok()
}

/// Extract from a browser-network JSON payload (detail envelope).
pub fn network(body: &str, source: &SourceId, now_ms: u64, expected: Option<Fingerprint>) -> Draft {
    let strategy = Strategy::NetworkJson;
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

/// Extract a draft per search-result item (bounded).
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
        .get("result")
        .and_then(|result| result.get("products"))
        .or_else(|| value.get("data").and_then(|data| data.get("products")))
        .and_then(Value::as_array);
    match items {
        Some(items) if !items.is_empty() => items
            .iter()
            .take(crate::contract::extract::MAX_VARIANTS)
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
        node.data("product-id").is_some()
            || node.has_class("product-detail")
            || node.has_class("product-container")
    });
    let Some(container) = container else {
        return Draft::not_applicable(strategy);
    };
    let mut missing: Vec<String> = Vec::new();
    if dom::by_tag(&nodes, "h1").is_none() && dom::by_class(&nodes, "product-title").is_none() {
        missing.push("h1.product-title".to_string());
    }
    if dom::text_by_class(&nodes, "price-value").is_none() && container.data("price").is_none() {
        missing.push(".price-value".to_string());
    }
    if !missing.is_empty() {
        return Draft::schema(strategy, expected, fingerprint, missing);
    }
    let mut draft = Draft::success(strategy, Some(fingerprint));
    if let Some(product_id) = container.data("product-id") {
        draft.push_text(source, Field::OfferId, product_id, now_ms);
    }
    if let Some(title) = dom::by_tag(&nodes, "h1").map(|node| node.text.as_str()) {
        draft.push_text(source, Field::Title, title, now_ms);
    } else if let Some(title) = dom::text_by_class(&nodes, "product-title") {
        draft.push_text(source, Field::Title, title, now_ms);
    }
    if let Some(price) = dom::text_by_class(&nodes, "price-value") {
        if dictionary::is_inquiry_text(price) {
            draft.push_text(source, Field::InquiryOnly, "inquiry", now_ms);
        } else if let Some(money) = money_from_text(price) {
            draft.push_money(source, Field::Price, money, now_ms);
        }
    }
    if let Some(money) = container.data("price").and_then(money_from_text) {
        draft.push_money(source, Field::Price, money, now_ms);
    }
    if let Some(currency) = container.data("currency") {
        draft.push_text(source, Field::Currency, currency, now_ms);
    }
    for node in &nodes {
        if node.has_class("tier-row") {
            if let (Some(ladder), Some(price)) = (
                node.data("ladder").and_then(normalize::parse_leading_u64),
                node.data("price").and_then(money_from_text),
            ) {
                if let Ok(Some(quantity)) = normalize::nonzero_quantity(ladder) {
                    draft.tiers.push((quantity, price));
                }
            }
        }
        if node.has_class("company-name") && !node.text.is_empty() {
            draft.push_text(source, Field::Supplier, &node.text, now_ms);
        }
        if node.has_class("supplier-country") && !node.text.is_empty() {
            draft.push_text(source, Field::Supplier, &node.text, now_ms);
        }
        let Some(field) = dictionary::field_for_label(&node.text) else {
            continue;
        };
        let value = node
            .data("value")
            .map(str::to_string)
            .unwrap_or_else(|| labelled_value(&nodes, node).unwrap_or_default());
        apply_labelled(&mut draft, source, field, &value, now_ms);
    }
    draft.variants = variants_from_dom(&nodes);
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
            if trimmed.len() <= 200
                && !dictionary::is_inquiry_text(trimmed)
                && !dictionary::FIELD_LABELS
                    .iter()
                    .any(|(label, _)| trimmed.starts_with(label))
            {
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
        Field::Price => {
            if let Some(money) = money_from_text(raw) {
                draft.push_money(source, Field::Price, money, now_ms);
            }
        }
        Field::MinimumOrder | Field::OrderMultiple => {
            if let Some(quantity) = normalize::parse_leading_u64(raw)
                .and_then(|value| normalize::nonzero_quantity(value).ok().flatten())
            {
                draft.push(source, field, FieldValue::Quantity(quantity), now_ms);
            }
        }
        Field::Stock => {
            let digits: String = raw
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| *c != ',')
                .collect();
            if let Ok(value) = digits.parse::<u64>() {
                draft.push_quantity(source, field, value, now_ms);
            }
        }
        other => draft.push_text(source, other, raw, now_ms),
    }
}

fn labelled_value(nodes: &[dom::DomNode], label: &dom::DomNode) -> Option<String> {
    let index = nodes.iter().position(|node| node == label)?;
    nodes
        .iter()
        .skip(index + 1)
        .take(2)
        .find(|node| matches!(node.tag.as_str(), "dd" | "span") && !node.text.is_empty())
        .map(|node| node.text.clone())
}

fn variants_from_dom(nodes: &[dom::DomNode]) -> Vec<VariantEntry> {
    let mut variants: Vec<VariantEntry> = Vec::new();
    for node in nodes {
        let Some(sku) = node.data("sku") else {
            continue;
        };
        if variants.len() >= crate::contract::extract::MAX_VARIANTS {
            break;
        }
        let attributes = attributes_from_spec(sku);
        if attributes.is_empty() {
            continue;
        }
        let price = node.data("price").and_then(money_from_text);
        let stock = node
            .data("stock")
            .and_then(normalize::parse_leading_u64)
            .and_then(|value| normalize::nonzero_quantity(value).ok().flatten())
            .map(|quantity| StockState::InStock { quantity })
            .unwrap_or(StockState::Unknown);
        let moq = node
            .data("moq")
            .and_then(normalize::parse_leading_u64)
            .and_then(|value| normalize::nonzero_quantity(value).ok().flatten());
        if let Ok(entry) = VariantEntry::new(attributes, price, moq, stock) {
            variants.push(entry);
        }
    }
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

/// The product object of a buyer-visible payload.
fn product_object(value: &Value) -> Option<&Value> {
    for wrapper in [
        value,
        value.get("data").unwrap_or(value),
        value.get("result").unwrap_or(value),
    ] {
        if let Some(product) = wrapper.get("product") {
            return Some(product);
        }
        if let Some(items) = wrapper.get("products").and_then(Value::as_array) {
            return items.first();
        }
        if wrapper.get("productId").is_some() || wrapper.get("title").is_some() {
            return Some(wrapper);
        }
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
    let Some(product) = product_object(value) else {
        // A payload that no longer matches the product shape is a typed
        // drift when a schema was previously accepted; an unrelated
        // first-party XHR body stays NotApplicable.
        let observed = Fingerprint::of_json(value);
        if let Some(expected) = expected {
            if expected != observed {
                return Draft::schema(
                    strategy,
                    Some(expected),
                    observed,
                    vec!["data/product".to_string()],
                );
            }
        }
        return Draft::not_applicable(strategy);
    };
    let fingerprint = Fingerprint::of_json(product);
    let inquiry = product.get("contactSupplier").and_then(Value::as_bool) == Some(true)
        || product
            .get("priceVisibility")
            .and_then(text_field)
            .is_some_and(|raw| {
                raw.eq_ignore_ascii_case("inquiry") || raw.eq_ignore_ascii_case("rfq")
            })
        || product
            .get("priceText")
            .and_then(text_field)
            .is_some_and(|raw| dictionary::is_inquiry_text(&raw));
    let mut missing: Vec<String> = Vec::new();
    if product.get("productId").is_none() {
        missing.push("data/product/productId".to_string());
    }
    if product.get("title").is_none() && product.get("subject").is_none() {
        missing.push("data/product/title".to_string());
    }
    let has_price = product.get("price").is_some()
        || product.get("priceRange").is_some()
        || product.get("priceTiers").is_some()
        || product.get("skuOptions").is_some()
        || inquiry;
    if !has_price {
        missing.push("data/product/price".to_string());
    }
    if !missing.is_empty() {
        return Draft::schema(strategy, expected, fingerprint, missing);
    }
    let mut draft = Draft::success(strategy, Some(fingerprint));
    if let Some(product_id) = product.get("productId").and_then(text_field) {
        draft.push_text(source, Field::OfferId, &product_id, now_ms);
    }
    let title = product
        .get("title")
        .or_else(|| product.get("subject"))
        .and_then(text_field);
    if let Some(title) = title {
        draft.push_text(source, Field::Title, &title, now_ms);
    }
    // The payload-level currency is the only non-guessing source for bare
    // JSON numbers; it is never defaulted to USD.
    let declared = product
        .get("currency")
        .and_then(text_field)
        .and_then(|raw| normalize::currency_from_symbol(&raw));
    if inquiry {
        draft.push_text(source, Field::InquiryOnly, "inquiry", now_ms);
    } else {
        if let Some(currency) = product.get("currency").and_then(text_field) {
            draft.push_text(source, Field::Currency, &currency, now_ms);
        }
        if let Some(range) = product.get("priceRange") {
            let range_currency = range
                .get("currency")
                .and_then(text_field)
                .and_then(|raw| normalize::currency_from_symbol(&raw))
                .or(declared);
            if let Some(low) = range
                .get("min")
                .or_else(|| range.get("low"))
                .and_then(|value| json_money(value, range_currency))
            {
                draft.push_money(source, Field::PriceRangeLow, low, now_ms);
            }
            if let Some(high) = range
                .get("max")
                .or_else(|| range.get("high"))
                .and_then(|value| json_money(value, range_currency))
            {
                draft.push_money(source, Field::PriceRangeHigh, high, now_ms);
            }
        }
        if let Some(money) = product
            .get("price")
            .and_then(|value| json_money(value, declared))
        {
            draft.push_money(source, Field::Price, money, now_ms);
        }
        if let Some(tiers) = product.get("priceTiers").and_then(Value::as_array) {
            for tier in tiers.iter().take(16) {
                if let (Some(ladder), Some(price)) = (
                    tier.get("ladder")
                        .or_else(|| tier.get("minQuantity"))
                        .and_then(Value::as_u64),
                    tier.get("price")
                        .and_then(|value| json_money(value, declared)),
                ) {
                    if let Ok(Some(quantity)) = normalize::nonzero_quantity(ladder) {
                        draft.tiers.push((quantity, price));
                    }
                }
            }
        }
        // Variants are the authoritative price source when they carry prices.
        draft.variants = variants_from_sku_options(product, declared);
    }
    if let Some(moq) = product
        .get("moq")
        .or_else(|| product.get("minOrder"))
        .and_then(Value::as_u64)
    {
        draft.push_quantity(source, Field::MinimumOrder, moq, now_ms);
    }
    if let Some(multiple) = product.get("orderMultiple").and_then(Value::as_u64) {
        draft.push_quantity(source, Field::OrderMultiple, multiple, now_ms);
    }
    if let Some(stock) = product.get("stock").and_then(Value::as_u64) {
        draft.push_quantity(source, Field::Stock, stock, now_ms);
    }
    if let Some(model) = product.get("model").and_then(text_field) {
        draft.push_text(source, Field::Model, &model, now_ms);
    }
    if let Some(spec) = product.get("spec").and_then(text_field) {
        draft.push_text(source, Field::Specification, &spec, now_ms);
    }
    if let Some(supplier) = product.get("supplier") {
        if let Some(name) = supplier
            .get("companyName")
            .or_else(|| supplier.get("name"))
            .and_then(text_field)
        {
            draft.push_text(source, Field::Supplier, &name, now_ms);
            draft.push_text(source, Field::Manufacturer, &name, now_ms);
        }
        if let Some(country) = supplier.get("country").and_then(text_field) {
            draft.push_text(source, Field::Supplier, &country, now_ms);
        }
        if let Some(verified) = supplier.get("verified").and_then(Value::as_bool) {
            if verified {
                draft.push_text(source, Field::Supplier, "verified", now_ms);
            }
        }
    }
    if let Some(lead_time) = product.get("leadTime") {
        let min = lead_time.get("minDays").and_then(Value::as_u64);
        let max = lead_time.get("maxDays").and_then(Value::as_u64);
        if let Some(min) = min {
            let max = max.unwrap_or(min);
            draft.push_text(
                source,
                Field::LeadTime,
                &format!("{min}-{max} days"),
                now_ms,
            );
        }
    }
    draft
}

fn variants_from_sku_options(product: &Value, declared: Option<Currency>) -> Vec<VariantEntry> {
    let Some(options) = product.get("skuOptions").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut variants: Vec<VariantEntry> = Vec::new();
    for option in options.iter().take(crate::contract::extract::MAX_VARIANTS) {
        let mut attributes: Vec<VariantAttribute> = Vec::new();
        if let Some(list) = option.get("attributes").and_then(Value::as_array) {
            for attribute in list
                .iter()
                .take(crate::contract::extract::MAX_VARIANT_ATTRIBUTES)
            {
                let (Some(name), Some(value)) = (
                    attribute.get("name").and_then(text_field),
                    attribute.get("value").and_then(text_field),
                ) else {
                    continue;
                };
                if let (Ok(name), Ok(value)) = (Text::<64>::new(&name), Text::<256>::new(&value)) {
                    attributes.push(VariantAttribute { name, value });
                }
            }
        }
        if attributes.is_empty() {
            continue;
        }
        let price = option
            .get("price")
            .and_then(|value| json_money(value, declared));
        let moq = option
            .get("moq")
            .and_then(Value::as_u64)
            .and_then(|value| normalize::nonzero_quantity(value).ok().flatten());
        let stock = option
            .get("stock")
            .and_then(Value::as_u64)
            .and_then(|value| normalize::nonzero_quantity(value).ok().flatten())
            .map(|quantity| StockState::InStock { quantity })
            .unwrap_or(StockState::Unknown);
        if let Ok(entry) = VariantEntry::new(attributes, price, moq, stock) {
            variants.push(entry);
        }
    }
    variants
}

fn text_field(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) if !raw.trim().is_empty() && raw.len() <= 512 => {
            Some(raw.trim().to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Parse one JSON money token deterministically:
///
/// * a string with its own symbol/code decides the currency; a conflicting
///   payload-level declaration yields no observation,
/// * a string without an affix uses the payload-level currency,
/// * a bare JSON number has no affix and requires the payload-level
///   currency; without it there is no observation (never a USD guess).
fn json_money(value: &Value, declared: Option<Currency>) -> Option<faktor_commerce::Money> {
    match value {
        Value::String(raw) => {
            let (affix, body) = normalize::money_declared_currency(raw).ok()?;
            let currency = match (affix, declared) {
                (Some(affix), Some(declared)) if affix != declared => return None,
                (Some(affix), _) => affix,
                (None, Some(declared)) => declared,
                (None, None) => return None,
            };
            normalize::money_from_body(currency, &body).ok()
        }
        Value::Number(number) => {
            let currency = declared?;
            normalize::money_from_body(currency, &number.to_string()).ok()
        }
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
        let tail = &html[position
            ..html
                .len()
                .min(position + crate::contract::capture::MAX_CAPTURE_BYTES)];
        let Some(start) = tail.find('{') else {
            continue;
        };
        if let Some(block) = balanced_object(tail, start) {
            return Some(block);
        }
    }
    None
}

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

/// A canonical variant id for tests and callers that know an attribute spec.
pub fn variant_id_for(spec: &str) -> Result<VariantId, SourceError> {
    let attributes = attributes_from_spec(spec);
    crate::contract::extract::canonical_variant_id(&attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alibaba::normalize::assemble;
    use faktor_commerce::money::Money;
    use faktor_commerce::text::CanonicalUrl;
    use serde_json::json;

    #[test]
    fn buyer_text_currency_comes_from_the_affix_never_from_a_usd_guess() {
        let cases = [
            ("$18.20", Currency::USD, 18_200_000i64),
            ("US $1.20", Currency::USD, 1_200_000),
            ("¥18.20", Currency::CNY, 18_200_000),
            ("CN ¥18.20", Currency::CNY, 18_200_000),
            ("CNY 100", Currency::CNY, 100_000_000),
            ("€5", Currency::EUR, 5_000_000),
            ("5 EUR", Currency::EUR, 5_000_000),
            ("£2.50", Currency::GBP, 2_500_000),
        ];
        for (raw, currency, micros) in cases {
            let money = money_from_text(raw).unwrap_or_else(|| panic!("{raw} parses"));
            assert_eq!(money.currency, currency, "{raw}");
            assert_eq!(money.micros, micros, "{raw}");
        }
        // A bare amount declares nothing: no observation (never USD).
        for raw in ["18.20", "1,234.56", "\u{a5}100 USD", "$1.23 CNY", "₩100"] {
            assert!(money_from_text(raw).is_none(), "{raw} must not be a price");
        }
    }

    #[test]
    fn json_money_requires_agreement_between_affix_and_payload_currency() {
        let usd = |value: serde_json::Value, declared| json_money(&value, declared);
        // A bare number uses the payload's declared currency.
        assert_eq!(
            usd(json!(18.2), Some(Currency::CNY))
                .expect("declared")
                .currency,
            Currency::CNY
        );
        // Without a declared currency, a bare number is never guessed.
        assert!(usd(json!(18.2), None).is_none());
        // An affix decides; agreement with the payload is required.
        assert_eq!(
            usd(json!("¥18.20"), Some(Currency::CNY))
                .expect("affix")
                .micros,
            18_200_000
        );
        assert_eq!(
            usd(json!("€5"), None).expect("affix alone").currency,
            Currency::EUR
        );
        // Disagreement yields no observation (the assembly layer then refuses
        // an uncovered price as ExtractionIncomplete, never a USD conversion).
        assert!(usd(json!("¥18.20"), Some(Currency::USD)).is_none());
        assert!(usd(json!("$18.20"), Some(Currency::CNY)).is_none());
        // A token declaring two currencies is ambiguous.
        assert!(usd(json!("USD 1.23 EUR"), Some(Currency::USD)).is_none());
    }

    #[test]
    fn assembly_infers_the_currency_from_prices_and_refuses_conflicts() {
        let source = SourceId::new("alibaba").expect("source");
        let rig = crate::testsupport::Rig::new();
        let ctx = rig.ctx();
        let url = CanonicalUrl::parse("https://www.alibaba.com/product-detail/_1600123456789.html")
            .expect("url");

        let draft_with = |field_price: Option<Money>, declared: Option<&str>| {
            let mut draft = Draft::success(Strategy::NetworkJson, None);
            draft.push_text(&source, Field::Title, "USB Cable", 1);
            if let Some(declared) = declared {
                draft.push_text(&source, Field::Currency, declared, 1);
            }
            if let Some(price) = field_price {
                draft.push_money(&source, Field::Price, price, 1);
            }
            draft
        };

        // No declared currency: the observed price's currency decides.
        let offer = assemble(
            &source,
            &ctx,
            &url,
            &[draft_with(
                Some(Money::from_micros(Currency::CNY, 18_200_000)),
                None,
            )],
            1,
        )
        .expect("currency inferred from the price");
        assert_eq!(offer.offer.currency, Currency::CNY);

        // A declared currency that disagrees with the price refuses typed.
        let error = assemble(
            &source,
            &ctx,
            &url,
            &[draft_with(
                Some(Money::from_micros(Currency::USD, 5_000_000)),
                Some("CNY"),
            )],
            1,
        )
        .expect_err("declared/price conflict must refuse");
        assert_eq!(error, SourceError::ExtractionConflict);

        // An inquiry with no monetary observation keeps the documented
        // buyer-surface default (the currency is unused there).
        let mut inquiry = Draft::success(Strategy::NetworkJson, None);
        inquiry.push_text(&source, Field::Title, "USB Cable", 1);
        inquiry.push_text(&source, Field::InquiryOnly, "inquiry", 1);
        let offer = assemble(&source, &ctx, &url, &[inquiry], 1).expect("inquiry offer");
        assert_eq!(offer.offer.currency, crate::alibaba::DEFAULT_CURRENCY);
    }
}
