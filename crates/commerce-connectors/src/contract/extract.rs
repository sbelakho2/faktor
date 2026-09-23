//! Multi-extractor reconciliation, per-field extraction health, structural
//! fingerprints and schema-drift classification (`docs/acquire.md` §8, §13).
//!
//! Every browser/marketplace acquisition runs the same deterministic
//! pipeline, with no model in the loop:
//!
//! 1. each **strategy** (browser-network JSON, embedded application state,
//!    structural DOM, rendered text) either produces field observations or
//!    reports [`StrategyOutcome::NotApplicable`] /
//!    [`StrategyOutcome::SchemaMismatch`];
//! 2. observations of the same semantic field are **reconciled** with a
//!    documented authority ordering — a conflict is recorded and the
//!    strongest observation wins; conflicting prices are **never averaged**;
//! 3. structural **fingerprints** classify `SchemaDrift` instead of letting a
//!    changed payload be silently misparsed;
//! 4. extraction **health** is tracked per field and strategy, so the next
//!    attempt prefers a strategy that has actually worked (self-optimization
//!    without an LLM).
//!
//! Variant matrices are first-class: an ambiguous variant mapping is
//! [`VariantSelection::Ambiguous`], and the cheapest variant is only ever a
//! discovery hint — never a quote.

use std::collections::BTreeMap;

use faktor_commerce::money::Money;
use faktor_commerce::offer::{ObservationOrigin, StockState, VariantAttribute};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{Text, VariantId};
use faktor_commerce::{SourceError, SourceId};

/// At most this many observations are reconciled for one field.
pub const MAX_OBSERVATIONS_PER_FIELD: usize = 8;
/// At most this many variants are admitted from one source.
pub const MAX_VARIANTS: usize = 256;
/// At most this many attributes per variant.
pub const MAX_VARIANT_ATTRIBUTES: usize = 32;

/// One extraction strategy, in documented authority order (strongest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Strategy {
    /// Browser-network JSON (XHR/fetch capture).
    NetworkJson,
    /// Embedded application state (`__INIT_DATA__`-style script blocks).
    EmbeddedState,
    /// Structural DOM (landmark elements, data attributes).
    StructuralDom,
    /// Rendered text (text anchors, last resort).
    RenderedText,
}

impl Strategy {
    /// Every strategy, strongest first.
    pub const ORDER: [Strategy; 4] = [
        Self::NetworkJson,
        Self::EmbeddedState,
        Self::StructuralDom,
        Self::RenderedText,
    ];

    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetworkJson => "network_json",
            Self::EmbeddedState => "embedded_state",
            Self::StructuralDom => "structural_dom",
            Self::RenderedText => "rendered_text",
        }
    }

    /// The acquisition mechanism this strategy is (spec §6).
    pub const fn mechanism(self) -> &'static str {
        match self {
            Self::NetworkJson => "browser_network",
            Self::EmbeddedState => "embedded_state",
            Self::StructuralDom | Self::RenderedText => "dom",
        }
    }

    /// The observation origin reported for values from this strategy.
    pub const fn origin(self) -> ObservationOrigin {
        match self {
            Self::NetworkJson => ObservationOrigin::BrowserNetwork,
            Self::EmbeddedState => ObservationOrigin::EmbeddedState,
            Self::StructuralDom => ObservationOrigin::Dom,
            Self::RenderedText => ObservationOrigin::RenderedText,
        }
    }

    /// The documented authority rank (higher is stronger; never averaged).
    pub const fn authority_rank(self) -> u8 {
        match self {
            Self::NetworkJson => 4,
            Self::EmbeddedState => 3,
            Self::StructuralDom => 2,
            Self::RenderedText => 1,
        }
    }
}

/// What one strategy produced for one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrategyOutcome {
    /// The strategy extracted at least one field.
    Success,
    /// The page/payload does not carry this strategy's shape at all.
    NotApplicable,
    /// The payload's shape changed: typed drift, never a silent misparse.
    SchemaMismatch(SchemaDrift),
    /// The strategy produced a value that disagrees with a stronger one.
    Conflict,
}

impl StrategyOutcome {
    /// The stable label.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotApplicable => "not_applicable",
            Self::SchemaMismatch(_) => "schema_mismatch",
            Self::Conflict => "conflict",
        }
    }
}

/// A stable structural fingerprint: JSON key signature, DOM landmark
/// signature or script-state schema digest, FNV-1a over a canonical form.
///
/// Fingerprints are diagnostic, not security: they exist so a changed
/// payload is classified as drift instead of being parsed as if it were the
/// old schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(u64);

impl Fingerprint {
    /// Fingerprint arbitrary canonical bytes.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(hash)
    }

    /// Fingerprint the sorted key signature of a JSON value (bounded).
    pub fn of_json(value: &serde_json::Value) -> Self {
        let mut keys: Vec<String> = Vec::new();
        collect_json_keys(value, "", 0, &mut keys);
        keys.sort();
        Self::of_bytes(keys.join("\n").as_bytes())
    }

    /// Fingerprint the DOM landmark signature (bounded).
    pub fn of_dom(html: &str) -> Self {
        let mut landmarks: Vec<String> = Vec::new();
        for node in crate::contract::dom::scan(html) {
            landmarks.push(node.landmark());
        }
        Self::of_bytes(landmarks.join("\n").as_bytes())
    }

    /// The stable hexadecimal rendering.
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

fn collect_json_keys(value: &serde_json::Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
    if depth > crate::normalize::MAX_JSON_NESTING {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if out.len() >= 512 {
                    return;
                }
                let path = format!("{prefix}/{key}");
                out.push(path.clone());
                collect_json_keys(child, &path, depth + 1, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items.iter().take(4) {
                collect_json_keys(child, prefix, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// A typed schema change: the payload no longer carries the structure the
/// extractor was written against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDrift {
    /// The fingerprint of the last schema the extractor accepted.
    pub expected: Fingerprint,
    /// The fingerprint observed now.
    pub observed: Fingerprint,
    /// The required keys/landmarks that are missing.
    pub missing: Vec<String>,
}

impl SchemaDrift {
    /// Classify one drift.
    pub fn new(expected: Fingerprint, observed: Fingerprint, mut missing: Vec<String>) -> Self {
        missing.sort();
        missing.dedup();
        Self {
            expected,
            observed,
            missing,
        }
    }

    /// A one-line bounded rendering (safe for logs; no payload content).
    pub fn render(&self) -> String {
        format!(
            "schema_drift expected={} observed={} missing={}",
            self.expected.to_hex(),
            self.observed.to_hex(),
            self.missing.join(",")
        )
    }

    /// Drift is never parsed through: it surfaces as an incomplete
    /// extraction, never as a value.
    pub const fn to_source_error(&self) -> SourceError {
        SourceError::ExtractionIncomplete
    }
}

/// A semantic field the connectors extract (site dictionaries map to these).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    /// The source's offer/listing id.
    OfferId,
    /// The listing title.
    Title,
    /// A single unit price.
    Price,
    /// The low end of a headline price range.
    PriceRangeLow,
    /// The high end of a headline price range.
    PriceRangeHigh,
    /// The offer currency.
    Currency,
    /// The minimum order quantity.
    MinimumOrder,
    /// Stock (text or quantity).
    Stock,
    /// The manufacturer/factory name.
    Manufacturer,
    /// The model number (型号).
    Model,
    /// The specification text (规格).
    Specification,
    /// The supplier name.
    Supplier,
    /// A price tier (quantity + unit price).
    TierPrice,
    /// A variant attribute value.
    Variant,
    /// The page states inquiry/RFQ-only pricing.
    InquiryOnly,
    /// The order multiple.
    OrderMultiple,
    /// Lead time text.
    LeadTime,
}

impl Field {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfferId => "offer_id",
            Self::Title => "title",
            Self::Price => "price",
            Self::PriceRangeLow => "price_range_low",
            Self::PriceRangeHigh => "price_range_high",
            Self::Currency => "currency",
            Self::MinimumOrder => "moq",
            Self::Stock => "stock",
            Self::Manufacturer => "manufacturer",
            Self::Model => "model",
            Self::Specification => "specification",
            Self::Supplier => "supplier",
            Self::TierPrice => "tier_price",
            Self::Variant => "variant",
            Self::InquiryOnly => "inquiry_only",
            Self::OrderMultiple => "order_multiple",
            Self::LeadTime => "lead_time",
        }
    }

    /// Fields whose conflict must never be averaged or resolved by guesswork.
    pub const fn is_money_critical(self) -> bool {
        matches!(
            self,
            Self::Price | Self::PriceRangeLow | Self::PriceRangeHigh | Self::TierPrice
        )
    }
}

/// One extracted field value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// An exact amount.
    Money(Money),
    /// A positive quantity.
    Quantity(NonZeroQuantity),
    /// Free text (untrusted data).
    Text(Text<512>),
}

impl FieldValue {
    /// The canonical comparison key (conflict detection is equality of
    /// canonical keys; amounts are never combined).
    pub fn canonical_key(&self) -> String {
        match self {
            Self::Money(money) => format!("money:{}:{}", money.currency.as_str(), money.micros),
            Self::Quantity(quantity) => format!("qty:{}", quantity.get()),
            Self::Text(text) => format!("text:{}", text.as_str().trim().to_lowercase()),
        }
    }

    /// The amount, when this is money.
    pub const fn as_money(&self) -> Option<Money> {
        match self {
            Self::Money(money) => Some(*money),
            _ => None,
        }
    }

    /// The quantity, when this is a quantity.
    pub const fn as_quantity(&self) -> Option<NonZeroQuantity> {
        match self {
            Self::Quantity(quantity) => Some(*quantity),
            _ => None,
        }
    }

    /// The text, when this is text.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text.as_str()),
            _ => None,
        }
    }
}

/// One observation of one field by one strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldObservation {
    /// The source that produced it.
    pub source: SourceId,
    /// The semantic field.
    pub field: Field,
    /// The observed value.
    pub value: FieldValue,
    /// The strategy that produced it.
    pub strategy: Strategy,
    /// The observation origin.
    pub origin: ObservationOrigin,
    /// When it was observed.
    pub observed_at_ms: u64,
}

impl FieldObservation {
    /// Construct one observation.
    pub fn new(
        source: SourceId,
        field: Field,
        value: FieldValue,
        strategy: Strategy,
        observed_at_ms: u64,
    ) -> Self {
        Self {
            source,
            field,
            value,
            strategy,
            origin: strategy.origin(),
            observed_at_ms,
        }
    }
}

/// A reconciled field (`docs/acquire.md` §8): a value with authority and
/// corroboration, a recorded conflict, or missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedField {
    /// All observations agree.
    Value {
        /// The agreed value.
        value: FieldValue,
        /// The strongest strategy that produced it.
        authority: Strategy,
        /// Every other agreeing strategy (deduplicated, strongest first).
        corroborated_by: Vec<Strategy>,
    },
    /// Observations disagree. The conflict is recorded; `selected` is the
    /// documented authority ordering (never an average, never a guess).
    Conflict {
        /// Every observation, strongest strategy first.
        observations: Vec<FieldObservation>,
        /// The strongest strategy.
        authority: Strategy,
        /// The value the authority ordering selects.
        selected: FieldValue,
    },
    /// No strategy produced this field.
    Missing,
}

impl ResolvedField {
    /// The selected value, when there is one.
    pub fn value(&self) -> Option<&FieldValue> {
        match self {
            Self::Value { value, .. } => Some(value),
            Self::Conflict { selected, .. } => Some(selected),
            Self::Missing => None,
        }
    }

    /// True when observations disagreed.
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// The recorded observations.
    pub fn observations(&self) -> &[FieldObservation] {
        match self {
            Self::Value { .. } | Self::Missing => &[],
            Self::Conflict { observations, .. } => observations,
        }
    }
}

/// Reconcile observations of one field with the documented authority
/// ordering. Conflicting amounts are **never averaged**: the conflict is
/// recorded and the strongest strategy's value is selected.
///
/// Ranking happens over **all** observations; truncation to
/// [`MAX_OBSERVATIONS_PER_FIELD`] only ever drops the tail *after* ranking,
/// so a stronger later observation can never be silently discarded. If any
/// dropped observation disagrees with the selected value, the field is
/// recorded as a [`ResolvedField::Conflict`] rather than a clean value.
pub fn reconcile(observations: &[FieldObservation]) -> ResolvedField {
    if observations.is_empty() {
        return ResolvedField::Missing;
    }
    let mut ordered: Vec<FieldObservation> = observations.to_vec();
    ordered.sort_by(|left, right| {
        right
            .strategy
            .authority_rank()
            .cmp(&left.strategy.authority_rank())
            .then_with(|| left.field.as_str().cmp(right.field.as_str()))
    });
    let truncated = ordered.len() > MAX_OBSERVATIONS_PER_FIELD;
    let dropped: Vec<FieldObservation> = if truncated {
        ordered.split_off(MAX_OBSERVATIONS_PER_FIELD)
    } else {
        Vec::new()
    };
    let authority = ordered[0].strategy;
    let first_key = ordered[0].value.canonical_key();
    let dropped_disagrees = dropped
        .iter()
        .any(|observation| observation.value.canonical_key() != first_key);
    if !dropped_disagrees
        && ordered
            .iter()
            .all(|observation| observation.value.canonical_key() == first_key)
    {
        let mut corroborated_by: Vec<Strategy> = Vec::new();
        for observation in ordered.iter().skip(1) {
            if !corroborated_by.contains(&observation.strategy) {
                corroborated_by.push(observation.strategy);
            }
        }
        return ResolvedField::Value {
            value: ordered[0].value.clone(),
            authority,
            corroborated_by,
        };
    }
    ResolvedField::Conflict {
        selected: ordered[0].value.clone(),
        authority,
        observations: ordered,
    }
}

/// Counters for one `(field, strategy)` pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrategyStats {
    /// Successful extractions.
    pub success: u64,
    /// The strategy did not apply to the payload.
    pub not_applicable: u64,
    /// The payload shape changed.
    pub schema_mismatch: u64,
    /// The strategy disagreed with a stronger one.
    pub conflict: u64,
}

/// Per-field, per-strategy extraction health (spec §8: the runtime remembers
/// which strategy works).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractionHealth {
    entries: BTreeMap<(Field, Strategy), StrategyStats>,
}

impl ExtractionHealth {
    /// An empty health record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one strategy outcome for one field.
    pub fn record(&mut self, field: Field, strategy: Strategy, outcome: &StrategyOutcome) {
        let entry = self.entries.entry((field, strategy)).or_default();
        match outcome {
            StrategyOutcome::Success => entry.success = entry.success.saturating_add(1),
            StrategyOutcome::NotApplicable => {
                entry.not_applicable = entry.not_applicable.saturating_add(1)
            }
            StrategyOutcome::SchemaMismatch(_) => {
                entry.schema_mismatch = entry.schema_mismatch.saturating_add(1)
            }
            StrategyOutcome::Conflict => entry.conflict = entry.conflict.saturating_add(1),
        }
    }

    /// The counters for one pair.
    pub fn stats(&self, field: Field, strategy: Strategy) -> StrategyStats {
        self.entries
            .get(&(field, strategy))
            .copied()
            .unwrap_or_default()
    }

    /// The preferred strategy for a field: a strategy with at least one
    /// success and no schema mismatch, strongest first; otherwise the
    /// strongest strategy with any success.
    pub fn preferred_strategy(&self, field: Field) -> Option<Strategy> {
        let clean = Strategy::ORDER.into_iter().find(|strategy| {
            let stats = self.stats(field, *strategy);
            stats.success > 0 && stats.schema_mismatch == 0
        });
        clean.or_else(|| {
            Strategy::ORDER
                .into_iter()
                .find(|strategy| self.stats(field, *strategy).success > 0)
        })
    }

    /// True when every recorded attempt for this pair failed structurally.
    pub fn is_structurally_failing(&self, field: Field, strategy: Strategy) -> bool {
        let stats = self.stats(field, strategy);
        stats.schema_mismatch > 0 && stats.success == 0
    }

    /// Every recorded pair (deterministic order).
    pub fn entries(&self) -> Vec<(Field, Strategy, StrategyStats)> {
        self.entries
            .iter()
            .map(|((field, strategy), stats)| (*field, *strategy, *stats))
            .collect()
    }
}

/// One variant (SKU) of a marketplace offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantEntry {
    /// The canonical variant id (`attr=value;attr=value`, sorted).
    pub variant_id: VariantId,
    /// The variant attributes.
    pub attributes: Vec<VariantAttribute>,
    /// The variant unit price, when stated.
    pub unit_price: Option<Money>,
    /// The variant MOQ, when stated.
    pub moq: Option<NonZeroQuantity>,
    /// The variant stock.
    pub stock: StockState,
}

impl VariantEntry {
    /// Construct and validate one variant entry.
    pub fn new(
        attributes: Vec<VariantAttribute>,
        unit_price: Option<Money>,
        moq: Option<NonZeroQuantity>,
        stock: StockState,
    ) -> Result<Self, SourceError> {
        if attributes.len() > MAX_VARIANT_ATTRIBUTES {
            return Err(SourceError::ExtractionIncomplete);
        }
        let variant_id = canonical_variant_id(&attributes)?;
        Ok(Self {
            variant_id,
            attributes,
            unit_price,
            moq,
            stock,
        })
    }
}

/// The canonical variant id: attributes sorted by name, `name=value`,
/// joined with `;`. Deterministic, so the same attribute set always maps to
/// the same id.
pub fn canonical_variant_id(attributes: &[VariantAttribute]) -> Result<VariantId, SourceError> {
    let mut pairs: Vec<(String, String)> = attributes
        .iter()
        .map(|attribute| {
            (
                attribute.name.as_str().trim().to_string(),
                attribute.value.as_str().trim().to_string(),
            )
        })
        .collect();
    pairs.sort();
    let mut spec = String::new();
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            spec.push(';');
        }
        spec.push_str(name);
        spec.push('=');
        spec.push_str(value);
    }
    if spec.is_empty() {
        return Err(SourceError::ExtractionIncomplete);
    }
    VariantId::new(&spec).map_err(|_| SourceError::ExtractionIncomplete)
}

/// Parse a requested variant id (`a=b;c=d`, also `a:b`) into attribute pairs.
pub fn parse_variant_spec(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in raw.split([';', '|']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let split = part.split_once('=').or_else(|| part.split_once(':'));
        if let Some((name, value)) = split {
            let name = name.trim();
            let value = value.trim();
            if !name.is_empty() && !value.is_empty() {
                out.push((name.to_string(), value.to_string()));
            }
        }
    }
    out
}

/// The result of selecting a variant from a matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantSelection {
    /// Exactly one variant matched.
    Exact(VariantEntry),
    /// More than one variant matched: never silently pick one.
    Ambiguous(Vec<VariantEntry>),
    /// Nothing matched.
    Missing,
}

impl VariantSelection {
    /// The selected variant, when unambiguous.
    pub fn selected(&self) -> Option<&VariantEntry> {
        match self {
            Self::Exact(entry) => Some(entry),
            Self::Ambiguous(_) | Self::Missing => None,
        }
    }

    /// The typed error a quote must fail with when the selection is not
    /// unambiguous.
    pub const fn ambiguity_error(&self) -> Option<SourceError> {
        match self {
            Self::Exact(_) => None,
            Self::Ambiguous(_) | Self::Missing => Some(SourceError::VariantAmbiguous),
        }
    }
}

/// Select a variant by exact id first, then by attribute-spec match. An
/// ambiguous or missing mapping is never resolved to the cheapest variant.
pub fn select_variant(matrix: &[VariantEntry], requested_id: &str) -> VariantSelection {
    if let Some(exact) = matrix
        .iter()
        .find(|entry| entry.variant_id.as_str() == requested_id)
    {
        return VariantSelection::Exact(exact.clone());
    }
    let spec = parse_variant_spec(requested_id);
    if spec.is_empty() {
        return VariantSelection::Missing;
    }
    let matching: Vec<VariantEntry> = matrix
        .iter()
        .filter(|entry| {
            spec.iter().all(|(name, value)| {
                let wanted = VariantAttribute {
                    name: Text::<64>::new(name)
                        .unwrap_or_else(|_| Text::<64>::new("attribute").expect("literal")),
                    value: Text::<256>::new(value)
                        .unwrap_or_else(|_| Text::<256>::new("value").expect("literal")),
                };
                entry
                    .attributes
                    .iter()
                    .any(|attribute| attribute.matches(&wanted))
            })
        })
        .cloned()
        .collect();
    match matching.len() {
        0 => VariantSelection::Missing,
        1 => VariantSelection::Exact(matching[0].clone()),
        _ => VariantSelection::Ambiguous(matching),
    }
}

/// The cheapest advertised variant price. **Discovery hint only** — never a
/// quote and never a substitute for an ambiguous variant mapping.
pub fn cheapest_variant_price(matrix: &[VariantEntry]) -> Option<Money> {
    matrix
        .iter()
        .filter_map(|entry| entry.unit_price)
        .min_by_key(|money| money.micros)
}

/// Every distinct variant price, ascending (for explicit price ranges).
pub fn variant_price_range(matrix: &[VariantEntry]) -> Option<(Money, Money)> {
    let mut prices: Vec<Money> = matrix.iter().filter_map(|entry| entry.unit_price).collect();
    prices.sort_by_key(|money| money.micros);
    match (prices.first(), prices.last()) {
        (Some(low), Some(high)) => Some((*low, *high)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_commerce::money::Currency;

    fn source() -> SourceId {
        SourceId::new("1688").expect("source")
    }

    fn observation(field: Field, money: i64, strategy: Strategy) -> FieldObservation {
        FieldObservation::new(
            source(),
            field,
            FieldValue::Money(Money::from_micros(Currency::CNY, money)),
            strategy,
            1_700_000_000_000,
        )
    }

    #[test]
    fn agreeing_observations_carry_authority_and_corroboration() {
        let resolved = reconcile(&[
            observation(Field::Price, 36_000_000, Strategy::NetworkJson),
            observation(Field::Price, 36_000_000, Strategy::StructuralDom),
            observation(Field::Price, 36_000_000, Strategy::RenderedText),
        ]);
        match resolved {
            ResolvedField::Value {
                value,
                authority,
                corroborated_by,
            } => {
                assert_eq!(value.as_money().expect("money").micros, 36_000_000);
                assert_eq!(authority, Strategy::NetworkJson);
                assert_eq!(
                    corroborated_by,
                    vec![Strategy::StructuralDom, Strategy::RenderedText]
                );
            }
            other => panic!("expected value, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_prices_are_recorded_and_never_averaged() {
        // Network says ¥36.00; a stale DOM says ¥2.00. The conflict is
        // recorded and the documented ordering selects the network value —
        // the result is never ¥19.00 and never the DOM value.
        let resolved = reconcile(&[
            observation(Field::Price, 2_000_000, Strategy::StructuralDom),
            observation(Field::Price, 36_000_000, Strategy::NetworkJson),
        ]);
        assert!(resolved.is_conflict());
        match &resolved {
            ResolvedField::Conflict {
                observations,
                authority,
                selected,
            } => {
                assert_eq!(*authority, Strategy::NetworkJson);
                assert_eq!(selected.as_money().expect("money").micros, 36_000_000);
                assert_eq!(observations.len(), 2);
                assert!(observations[0].strategy == Strategy::NetworkJson);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn truncation_never_drops_a_stronger_later_observation() {
        // Nine weaker RenderedText observations, then a stronger NetworkJson
        // observation that a pre-sort `take(8)` window would silently drop.
        let mut observations: Vec<FieldObservation> = (0..=MAX_OBSERVATIONS_PER_FIELD)
            .map(|index| {
                observation(
                    Field::Price,
                    1_000_000 + index as i64,
                    Strategy::RenderedText,
                )
            })
            .collect();
        observations.push(observation(Field::Price, 36_000_000, Strategy::NetworkJson));
        let resolved = reconcile(&observations);
        assert_eq!(
            resolved
                .value()
                .and_then(FieldValue::as_money)
                .expect("money")
                .micros,
            36_000_000,
            "ranking must run before truncation"
        );
        match &resolved {
            ResolvedField::Conflict { authority, .. } => {
                assert_eq!(*authority, Strategy::NetworkJson);
            }
            other => panic!("expected a recorded conflict, got {other:?}"),
        }
    }

    #[test]
    fn dropped_agreeing_observations_do_not_fabricate_a_conflict() {
        let observations: Vec<FieldObservation> = (0..MAX_OBSERVATIONS_PER_FIELD + 4)
            .map(|_| observation(Field::Price, 36_000_000, Strategy::RenderedText))
            .collect();
        let resolved = reconcile(&observations);
        assert!(!resolved.is_conflict());
        assert_eq!(
            resolved
                .value()
                .and_then(FieldValue::as_money)
                .expect("money")
                .micros,
            36_000_000
        );
        assert_eq!(resolved.observations().len(), 0);
    }

    #[test]
    fn health_remembers_which_strategy_works() {
        let mut health = ExtractionHealth::new();
        health.record(
            Field::Price,
            Strategy::NetworkJson,
            &StrategyOutcome::Success,
        );
        health.record(
            Field::Price,
            Strategy::StructuralDom,
            &StrategyOutcome::SchemaMismatch(SchemaDrift::new(
                Fingerprint::of_bytes(b"a"),
                Fingerprint::of_bytes(b"b"),
                vec!["data-price".to_string()],
            )),
        );
        assert_eq!(
            health.preferred_strategy(Field::Price),
            Some(Strategy::NetworkJson)
        );
        assert!(health.is_structurally_failing(Field::Price, Strategy::StructuralDom));
        assert_eq!(health.stats(Field::Price, Strategy::NetworkJson).success, 1);
    }

    #[test]
    fn fingerprints_classify_drift() {
        let good: serde_json::Value =
            serde_json::from_str(r#"{"data":{"offerId":"1","price":"2.00"}}"#).expect("json");
        let drifted: serde_json::Value =
            serde_json::from_str(r#"{"result":{"id":"1","amount":2}}"#).expect("json");
        let expected = Fingerprint::of_json(&good);
        let observed = Fingerprint::of_json(&drifted);
        assert_ne!(expected, observed);
        let drift = SchemaDrift::new(expected, observed, vec!["data/price".to_string()]);
        assert_eq!(drift.to_source_error(), SourceError::ExtractionIncomplete);
        assert!(drift.render().contains("missing=data/price"));
        assert_eq!(
            Fingerprint::of_dom("<div class=\"x\"></div>"),
            Fingerprint::of_dom("<div class=\"x\"></div>")
        );
    }

    #[test]
    fn variant_selection_never_falls_back_to_the_cheapest() {
        let cny = Currency::CNY;
        let matrix = vec![
            VariantEntry::new(
                vec![
                    VariantAttribute {
                        name: Text::<64>::new("颜色").expect("name"),
                        value: Text::<256>::new("黑色").expect("value"),
                    },
                    VariantAttribute {
                        name: Text::<64>::new("长度").expect("name"),
                        value: Text::<256>::new("1m").expect("value"),
                    },
                ],
                Some(Money::from_micros(cny, 36_000_000)),
                None,
                StockState::Unknown,
            )
            .expect("variant"),
            VariantEntry::new(
                vec![VariantAttribute {
                    name: Text::<64>::new("颜色").expect("name"),
                    value: Text::<256>::new("白色").expect("value"),
                }],
                Some(Money::from_micros(cny, 2_000_000)),
                None,
                StockState::Unknown,
            )
            .expect("variant"),
        ];
        assert_eq!(
            cheapest_variant_price(&matrix).expect("cheapest").micros,
            2_000_000
        );
        let selection = select_variant(&matrix, "颜色=黑色;长度=1m");
        let selected = selection.selected().expect("exact match");
        assert_eq!(selected.unit_price.expect("price").micros, 36_000_000);
        assert_eq!(selected.variant_id.as_str(), "长度=1m;颜色=黑色");

        // An unknown variant never resolves to the cheapest one.
        let missing = select_variant(&matrix, "颜色=蓝色");
        assert_eq!(missing, VariantSelection::Missing);
        assert_eq!(
            missing.ambiguity_error(),
            Some(SourceError::VariantAmbiguous)
        );
    }

    #[test]
    fn variant_ids_are_canonical_and_bounded() {
        let attributes = vec![
            VariantAttribute {
                name: Text::<64>::new("b").expect("name"),
                value: Text::<256>::new("2").expect("value"),
            },
            VariantAttribute {
                name: Text::<64>::new("a").expect("name"),
                value: Text::<256>::new("1").expect("value"),
            },
        ];
        let id = canonical_variant_id(&attributes).expect("id");
        assert_eq!(id.as_str(), "a=1;b=2");
        assert!(canonical_variant_id(&[]).is_err());
    }
}
