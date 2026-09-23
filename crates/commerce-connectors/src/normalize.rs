//! Shared normalization primitives: hostile JSON → domain types, exactly.
//!
//! Every connector normalizes into the canonical `faktor-commerce` model
//! through this module, so the hostile-input rules are stated once:
//!
//! * **Exact money, no floats.** Price fields are captured as
//!   [`serde_json::value::RawValue`] (the raw JSON token), never as `f64`.
//!   [`money_from_raw`] accepts a JSON number token or a JSON string such as
//!   `"$1.23"` / `"1.23 USD"` and parses it with integer arithmetic. A
//!   token with more than six fractional digits, an exponent, an ambiguous
//!   decimal separator or a currency mismatch is a typed
//!   [`NormalizeError`] — a price is never rounded, guessed or misparsed.
//! * **Quantities are integers.** [`parse_u64_raw`] rejects `1.5`, `1e3`,
//!   negative values, fullwidth digits and oversized numbers instead of
//!   truncating them.
//! * **Identity text is strict, display text is sanitized.**
//!   [`strict_text`] is used for MPN / source part number / manufacturer
//!   (a bidi-overridden part number is a typed error, never a sanitized
//!   spoof); [`display_text`] is used for titles/descriptions, where
//!   control characters are replaced and the value is truncated on a char
//!   boundary.
//! * **Packaging is never guessed.** [`packaging_from_label`] maps only the
//!   documented labels; anything else is `None`.

use faktor_commerce::money::{Currency, Money};
use faktor_commerce::packaging::PackagingType;
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{Text, TextError};
use faktor_commerce::{LeadTime, OfferError, SourceError, StockState};
use serde_json::value::RawValue;

/// Milliseconds since the Unix epoch, saturating at `u64::MAX`.
pub fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A rejected normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NormalizeError {
    /// The response carried no usable product identity.
    #[error("response carries no product identity")]
    MissingIdentity,
    /// A price token is not an exact, bounded decimal amount.
    #[error("price token is not an exact decimal amount")]
    InvalidMoney,
    /// A price is negative.
    #[error("price is negative")]
    NegativePrice,
    /// Two prices in one offer use different currencies.
    #[error("offer mixes currencies")]
    CurrencyConflict,
    /// A quantity token is not a bounded positive integer.
    #[error("quantity token is not a bounded integer")]
    InvalidQuantity,
    /// Price tiers repeat or overlap.
    #[error("price tiers are invalid")]
    InvalidTiers,
    /// Identity text is empty, over-long or carries control characters.
    #[error("identity text is invalid")]
    IdentityText,
    /// A URL is not a valid canonical URL.
    #[error("url is invalid")]
    InvalidUrl,
    /// A required display field could not be produced.
    #[error("display text is invalid")]
    DisplayText,
    /// The source returned a schema the connector cannot normalize.
    #[error("response schema is not recognized")]
    Schema,
    /// The source stated something the domain cannot represent.
    #[error("response states an unsupported value")]
    Unsupported,
}

impl NormalizeError {
    /// Map to the typed acquisition error the runtime sees.
    pub const fn to_source_error(self) -> SourceError {
        match self {
            Self::MissingIdentity => SourceError::ProductNotFound,
            Self::CurrencyConflict | Self::InvalidTiers => SourceError::ExtractionConflict,
            Self::InvalidMoney
            | Self::NegativePrice
            | Self::InvalidQuantity
            | Self::IdentityText
            | Self::InvalidUrl
            | Self::DisplayText
            | Self::Schema
            | Self::Unsupported => SourceError::ExtractionIncomplete,
        }
    }
}

impl From<NormalizeError> for SourceError {
    fn from(error: NormalizeError) -> Self {
        error.to_source_error()
    }
}

/// Currency symbols accepted as money affixes.
const CURRENCY_SYMBOLS: &[char] = &['$', '¥', '€', '£', '₩', '₹', '¢'];

/// Multi-character documented symbols (`US$ 1.23`).
const MULTI_CURRENCY_SYMBOLS: &[&str] = &["US$", "CN¥", "HK$", "NT$", "S$", "JP¥"];

/// Parse one raw JSON token as exact money in `currency`.
pub fn money_from_raw(raw: &RawValue, currency: Currency) -> Result<Money, NormalizeError> {
    let token = raw.get().trim();
    if token.is_empty() || token.len() > 128 {
        return Err(NormalizeError::InvalidMoney);
    }
    let text = if token.starts_with('"') {
        serde_json::from_str::<String>(token).map_err(|_| NormalizeError::InvalidMoney)?
    } else {
        token.to_string()
    };
    parse_money_text(currency, &text)
}

/// Parse a decimal money string with optional currency affixes. When the
/// text declares a currency through a symbol or ISO code, that declaration
/// must equal `currency`: `"¥1.20"` is never parsed as USD. A conflict is a
/// typed [`NormalizeError::CurrencyConflict`], never a silent reinterpretation.
pub fn parse_money_text(currency: Currency, raw: &str) -> Result<Money, NormalizeError> {
    let (declared, body) = money_declared_currency(raw)?;
    if let Some(declared) = declared {
        if declared != currency {
            return Err(NormalizeError::CurrencyConflict);
        }
    }
    money_from_body(currency, &body)
}

/// Parse a numeric body (affixes already stripped) in the given currency.
pub(crate) fn money_from_body(currency: Currency, body: &str) -> Result<Money, NormalizeError> {
    if body.contains(',') {
        // Grouped thousands: `1,234.56` (or `1,234`). A European decimal
        // comma is ambiguous and refused rather than guessed.
        if !is_grouped_integer_form(body) {
            return Err(NormalizeError::InvalidMoney);
        }
    }
    let cleaned: String = body.chars().filter(|c| *c != ',').collect();
    if cleaned.matches('.').count() > 1 {
        return Err(NormalizeError::InvalidMoney);
    }
    let money = Money::parse(currency, &cleaned).map_err(|_| NormalizeError::InvalidMoney)?;
    if money.is_negative() {
        return Err(NormalizeError::NegativePrice);
    }
    Ok(money)
}

/// Parse a money text that carries its own declaration: returns the declared
/// currency (`None` for a bare amount) and the numeric body. Every affix is
/// mapped through the documented symbol/code table; two different declared
/// currencies in one token are a typed conflict, and an affix that maps to
/// nothing is a typed refusal (it is never stripped as noise).
pub(crate) fn money_declared_currency(
    raw: &str,
) -> Result<(Option<Currency>, String), NormalizeError> {
    let mut body = raw.trim();
    if body.is_empty() {
        return Err(NormalizeError::InvalidMoney);
    }
    let mut symbols = 0u8;
    let mut codes = 0u8;
    let mut declared: Option<Currency> = None;
    // Leading affixes: whitespace, at most one currency symbol and at most
    // one uppercase 3-letter code. A stray word is refused, never skipped.
    loop {
        let trimmed = body.trim_start();
        if trimmed.is_empty() {
            return Err(NormalizeError::InvalidMoney);
        }
        let first = trimmed.chars().next().unwrap_or(' ');
        if CURRENCY_SYMBOLS.contains(&first) {
            symbols += 1;
            if symbols > 1 {
                return Err(NormalizeError::InvalidMoney);
            }
            let currency =
                currency_from_symbol(&first.to_string()).ok_or(NormalizeError::InvalidMoney)?;
            declare(&mut declared, currency)?;
            body = &trimmed[first.len_utf8()..];
            continue;
        }
        if let Some(symbol) = MULTI_CURRENCY_SYMBOLS
            .iter()
            .find(|symbol| trimmed.starts_with(**symbol))
        {
            symbols += 1;
            if symbols > 1 {
                return Err(NormalizeError::InvalidMoney);
            }
            let currency = currency_from_symbol(symbol).ok_or(NormalizeError::InvalidMoney)?;
            declare(&mut declared, currency)?;
            body = &trimmed[symbol.len()..];
            continue;
        }
        if let Some((skip, currency)) = uppercase_code_prefix(trimmed) {
            codes += 1;
            // A prefix and a suffix code may both be present (`$1.23 USD`);
            // `declare` refuses them when they disagree.
            if codes > 2 {
                return Err(NormalizeError::InvalidMoney);
            }
            declare(&mut declared, currency)?;
            body = &trimmed[skip..];
            continue;
        }
        body = trimmed;
        break;
    }
    // Trailing affixes, symmetric.
    loop {
        let trimmed = body.trim_end();
        if trimmed.is_empty() {
            return Err(NormalizeError::InvalidMoney);
        }
        let last = trimmed.chars().last().unwrap_or(' ');
        if let Some(symbol) = MULTI_CURRENCY_SYMBOLS
            .iter()
            .find(|symbol| trimmed.ends_with(**symbol))
        {
            symbols += 1;
            if symbols > 1 {
                return Err(NormalizeError::InvalidMoney);
            }
            let currency = currency_from_symbol(symbol).ok_or(NormalizeError::InvalidMoney)?;
            declare(&mut declared, currency)?;
            body = &trimmed[..trimmed.len() - symbol.len()];
            continue;
        }
        if CURRENCY_SYMBOLS.contains(&last) {
            symbols += 1;
            if symbols > 1 {
                return Err(NormalizeError::InvalidMoney);
            }
            let currency =
                currency_from_symbol(&last.to_string()).ok_or(NormalizeError::InvalidMoney)?;
            declare(&mut declared, currency)?;
            body = &trimmed[..trimmed.len() - last.len_utf8()];
            continue;
        }
        if let Some((cut, currency)) = uppercase_code_suffix(trimmed) {
            codes += 1;
            if codes > 2 {
                return Err(NormalizeError::InvalidMoney);
            }
            declare(&mut declared, currency)?;
            body = &trimmed[..trimmed.len() - cut];
            continue;
        }
        body = trimmed;
        break;
    }
    let body = body.trim();
    if body.is_empty() {
        return Err(NormalizeError::InvalidMoney);
    }
    Ok((declared, body.to_string()))
}

/// Record one declared currency, refusing a token that declares two.
fn declare(declared: &mut Option<Currency>, currency: Currency) -> Result<(), NormalizeError> {
    match declared {
        Some(existing) if *existing != currency => Err(NormalizeError::CurrencyConflict),
        Some(_) => Ok(()),
        None => {
            *declared = Some(currency);
            Ok(())
        }
    }
}

/// The byte length of a leading uppercase 3-letter currency code and its
/// mapped currency, when one is present.
fn uppercase_code_prefix(text: &str) -> Option<(usize, Currency)> {
    let chars: Vec<char> = text.chars().take(4).collect();
    if chars.len() < 3 || !chars[..3].iter().all(char::is_ascii_uppercase) {
        return None;
    }
    let boundary_ok = chars.len() == 3
        || chars[3] == ' '
        || chars[3].is_ascii_digit()
        || CURRENCY_SYMBOLS.contains(&chars[3]);
    if !boundary_ok {
        return None;
    }
    let code: String = chars[..3].iter().collect();
    let currency = Currency::new(&code).ok()?;
    Some((chars[..3].iter().map(|c| c.len_utf8()).sum(), currency))
}

/// The byte length of a trailing uppercase 3-letter currency code and its
/// mapped currency, when one is present.
fn uppercase_code_suffix(text: &str) -> Option<(usize, Currency)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 3 {
        return None;
    }
    let tail = &chars[chars.len() - 3..];
    if !tail.iter().all(char::is_ascii_uppercase) {
        return None;
    }
    let boundary_ok = chars.len() == 3
        || chars[chars.len() - 4] == ' '
        || chars[chars.len() - 4].is_ascii_digit()
        || CURRENCY_SYMBOLS.contains(&chars[chars.len() - 4]);
    if !boundary_ok {
        return None;
    }
    let code: String = tail.iter().collect();
    let currency = Currency::new(&code).ok()?;
    Some((tail.iter().map(|c| c.len_utf8()).sum(), currency))
}

/// The documented grouped-integer rule: at least one grouping separator, a
/// first group of 1..=3 digits that must not start with `0` (so `0,123` and
/// `01,234` are refused, never read as 123/1234), and all following groups of
/// exactly three digits. An optional fractional part is digits only.
fn is_grouped_integer_form(body: &str) -> bool {
    let digits = body.strip_prefix('-').unwrap_or(body);
    let (integer, fraction) = match digits.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (digits, None),
    };
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    let groups: Vec<&str> = integer.split(',').collect();
    if groups.len() < 2 {
        return false;
    }
    let first = groups[0];
    if first.is_empty()
        || first.len() > 3
        || first.starts_with('0')
        || !first.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    groups[1..]
        .iter()
        .all(|group| group.len() == 3 && group.bytes().all(|b| b.is_ascii_digit()))
}

/// Parse one raw JSON token as a bounded `u64`. Floats, exponents, signs,
/// whitespace, non-ASCII digits and oversized values are refused.
pub fn parse_u64_raw(raw: &RawValue) -> Result<u64, NormalizeError> {
    let token = raw.get().trim();
    if token.is_empty() || token.len() > 32 {
        return Err(NormalizeError::InvalidQuantity);
    }
    let text = if token.starts_with('"') {
        serde_json::from_str::<String>(token).map_err(|_| NormalizeError::InvalidQuantity)?
    } else {
        token.to_string()
    };
    parse_u64_text(&text)
}

/// Parse an integer text: ASCII digits only.
pub fn parse_u64_text(raw: &str) -> Result<u64, NormalizeError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 20 || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(NormalizeError::InvalidQuantity);
    }
    trimmed
        .parse::<u64>()
        .map_err(|_| NormalizeError::InvalidQuantity)
}

/// The leading run of ASCII digits, when there is one.
pub fn parse_leading_u64(raw: &str) -> Option<u64> {
    let digits: String = raw
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() || digits.len() > 20 {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Parse a raw quantity into a bounded non-zero quantity (`0` → `None`).
pub fn parse_nonzero_quantity_raw(
    raw: &RawValue,
) -> Result<Option<NonZeroQuantity>, NormalizeError> {
    let value = parse_u64_raw(raw)?;
    nonzero_quantity(value)
}

/// Build a bounded non-zero quantity (`0` → `None`).
pub fn nonzero_quantity(value: u64) -> Result<Option<NonZeroQuantity>, NormalizeError> {
    if value == 0 {
        return Ok(None);
    }
    NonZeroQuantity::new(value)
        .map(Some)
        .map_err(|_| NormalizeError::InvalidQuantity)
}

/// Strict identity text: control/bidi characters are a typed error, never a
/// sanitized value.
pub fn strict_text<const MAX: usize>(raw: &str) -> Result<Text<MAX>, NormalizeError> {
    Text::<MAX>::new(raw).map_err(|error| match error {
        TextError::Empty => NormalizeError::MissingIdentity,
        _ => NormalizeError::IdentityText,
    })
}

/// Sanitize display text: control and bidi characters become spaces, the
/// value is trimmed and truncated on a char boundary, and an unusable value
/// falls back to `fallback` (which callers pass already validated).
pub fn display_text<const MAX: usize>(
    raw: &str,
    fallback: &str,
) -> Result<Text<MAX>, NormalizeError> {
    let cleaned = clean_display(raw);
    if let Ok(text) = Text::<MAX>::new(&cleaned) {
        return Ok(text);
    }
    let truncated = truncate_chars(&cleaned, MAX);
    if let Ok(text) = Text::<MAX>::new(&truncated) {
        return Ok(text);
    }
    Text::<MAX>::new(fallback).map_err(|_| NormalizeError::DisplayText)
}

/// Replace control and bidi-control characters with spaces and collapse
/// whitespace runs.
pub fn clean_display(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(4096));
    let mut last_space = false;
    for ch in raw.chars() {
        let bad = ch.is_control() || faktor_commerce::text::is_bidi_control(ch);
        let ch = if bad { ' ' } else { ch };
        if ch == ' ' || ch == '\t' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// Truncate to at most `max_bytes`, never splitting a character.
pub fn truncate_chars(raw: &str, max_bytes: usize) -> String {
    if raw.len() <= max_bytes {
        return raw.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_string()
}

/// A bounded, control-free excerpt for diagnostics. The caller still runs
/// it through the secret guard.
pub fn sanitize_excerpt(raw: &str, max_chars: usize) -> String {
    let mut out: String = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(max_chars)
        .collect();
    if raw.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Parse a currency label (`"USD"`, `"usd "`, `"CNY"`).
pub fn currency_from_label(raw: &str) -> Option<Currency> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 8 {
        return None;
    }
    Currency::new(&trimmed.to_ascii_uppercase()).ok()
}

/// Parse a documented lead-time label (`"8 Weeks"`, `"6-8 weeks"`,
/// `"56 days"`). Without a unit there is no guess: `None`.
pub fn parse_lead_time_label(raw: &str) -> Option<LeadTime> {
    let lowered = raw.to_ascii_lowercase();
    let multiplier = if lowered.contains("week") || lowered.contains("wks") {
        7u16
    } else if lowered.contains("month") {
        30
    } else if lowered.contains("year") {
        365
    } else if lowered.contains("day") {
        1
    } else {
        return None;
    };
    let numbers: Vec<u32> = lowered
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect();
    let first = *numbers.first()?;
    let second = numbers.get(1).copied().unwrap_or(first);
    let (low, high) = if first <= second {
        (first, second)
    } else {
        (second, first)
    };
    let min_days = u16::try_from(low.saturating_mul(u32::from(multiplier))).ok()?;
    let max_days = u16::try_from(high.saturating_mul(u32::from(multiplier))).ok()?;
    if max_days > 3650 {
        return None;
    }
    LeadTime::new(min_days, max_days).ok()
}

/// Map a documented packaging label onto the first-class packaging kind.
/// Anything unrecognized is `None` — never silently mapped.
pub fn packaging_from_label(raw: &str) -> Option<PackagingType> {
    let lowered = raw.to_ascii_lowercase();
    let compact: String = lowered.chars().filter(|c| !c.is_whitespace()).collect();
    let has = |needle: &str| compact.contains(needle);
    if has("digi-reel") || has("digireel") {
        return Some(PackagingType::DigiReel);
    }
    if has("fullreel") {
        return Some(PackagingType::FullReel);
    }
    if has("tape&reel") || has("tapeandreel") || has("(tr)") || has("t&r") || has("reel") {
        return Some(PackagingType::TapeAndReel);
    }
    if has("cuttape") || has("(ct)") {
        return Some(PackagingType::CutTape);
    }
    if has("tray") {
        return Some(PackagingType::Tray);
    }
    if has("tube") || has("stick") {
        return Some(PackagingType::Tube);
    }
    if has("bulk") || has("bag") {
        return Some(PackagingType::Bulk);
    }
    if has("factorypack") || has("case") {
        return Some(PackagingType::FactoryPack);
    }
    None
}

/// The documented MPN-shape heuristic used to choose exact part-number
/// search over keyword search:
///
/// * 3..=64 bytes, no whitespace;
/// * at least one ASCII letter and one ASCII digit;
/// * only `[A-Za-z0-9._/#-]`;
/// * never a URL.
pub fn looks_like_mpn(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.len() < 3 || trimmed.len() > 64 || trimmed.contains("://") {
        return false;
    }
    if trimmed.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let mut has_alpha = false;
    let mut has_digit = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphabetic() {
            has_alpha = true;
        } else if ch.is_ascii_digit() {
            has_digit = true;
        } else if !matches!(ch, '.' | '_' | '/' | '#' | '-') {
            return false;
        }
    }
    has_alpha && has_digit
}

/// Validate a normalized price-break list against the offer currency.
pub fn validate_tiers(
    breaks: &[faktor_commerce::PriceBreak],
    currency: Currency,
) -> Result<(), NormalizeError> {
    faktor_commerce::validate_price_breaks(breaks, currency).map_err(|error| match error {
        OfferError::TooManyPriceBreaks { .. } => NormalizeError::InvalidTiers,
        OfferError::CurrencyMismatch { .. } => NormalizeError::CurrencyConflict,
        _ => NormalizeError::InvalidTiers,
    })
}

/// Map a source lifecycle label onto the domain lifecycle. Unrecognized
/// labels are [`LifecycleStatus::Unknown`], never guessed.
pub fn lifecycle_from_label(raw: &str) -> faktor_commerce::LifecycleStatus {
    let lowered = raw.trim().to_ascii_lowercase();
    if lowered.contains("not recommended") || lowered.contains("nrnd") {
        return faktor_commerce::LifecycleStatus::NotRecommendedForNewDesign;
    }
    if lowered.contains("obsolete") || lowered.contains("end of life") || lowered == "eol" {
        return faktor_commerce::LifecycleStatus::Obsolete;
    }
    if lowered.contains("active") || lowered.contains("production") {
        return faktor_commerce::LifecycleStatus::Active;
    }
    faktor_commerce::LifecycleStatus::Unknown
}

/// The price-break cap the connectors honor per documented API contract.
pub const MAX_DOCUMENTED_PRICE_BREAKS: usize = 4;

/// The connector-side JSON nesting ceiling. Every site parser refuses a
/// deeper document before handing it to `serde_json`, so hostile nesting can
/// never reach an implementation-defined recursion limit (let alone a stack).
pub const MAX_JSON_NESTING: usize = 64;

/// A string-aware scan for the maximum JSON container nesting in `body`.
/// Returns false when the document nests deeper than `max_depth` or when a
/// string is unterminated at the end of the bounded input.
pub fn json_nesting_within(body: &[u8], max_depth: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max_depth {
                    return false;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    !in_string
}

/// Map a documented currency symbol or code onto the domain currency. Never
/// guesses: an unrecognized label is `None`.
pub fn currency_from_symbol(raw: &str) -> Option<Currency> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 8 {
        return None;
    }
    match trimmed.to_ascii_uppercase().as_str() {
        "$" | "US$" | "USD" => Some(Currency::USD),
        "¥" | "￥" | "CNY" | "RMB" | "CN¥" => Some(Currency::CNY),
        "€" | "EUR" => Some(Currency::EUR),
        "£" | "GBP" => Some(Currency::GBP),
        "JPY" | "JP¥" => Some(Currency::JPY),
        "HKD" | "HK$" => Some(Currency::HKD),
        "TWD" | "NT$" => Some(Currency::TWD),
        "SGD" | "S$" => Some(Currency::SGD),
        other => Currency::new(other).ok(),
    }
}

/// Parse a strict grouped integer (`1234`, `1,234`). The number must lead
/// the text; a decimal point or exponent immediately after the integer run
/// (`1.5`, `1e3`), a sign, malformed grouping (`12,34`), or a leading-zero
/// first group (`0,123`, `01,234`) is refused rather than truncated into a
/// different quantity.
pub fn parse_grouped_u64(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let first = trimmed.as_bytes().first().copied()?;
    if !(first.is_ascii_digit() || first == b',') {
        return None;
    }
    let mut candidate = String::new();
    let mut consumed = 0usize;
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() || ch == ',' {
            candidate.push(ch);
            consumed += ch.len_utf8();
        } else {
            break;
        }
    }
    if let Some(next) = trimmed[consumed..].chars().next() {
        if matches!(next, '.' | 'e' | 'E') {
            return None;
        }
    }
    if candidate.is_empty() || candidate.len() > 24 {
        return None;
    }
    let groups: Vec<&str> = candidate.split(',').collect();
    let valid = match groups.as_slice() {
        [] => false,
        [all] => all.bytes().all(|b| b.is_ascii_digit()),
        [first, rest @ ..] => {
            !first.is_empty()
                && first.len() <= 3
                && !first.starts_with('0')
                && first.bytes().all(|b| b.is_ascii_digit())
                && rest
                    .iter()
                    .all(|group| group.len() == 3 && group.bytes().all(|b| b.is_ascii_digit()))
        }
    };
    if !valid {
        return None;
    }
    candidate.replace(',', "").parse::<u64>().ok()
}

/// Parse a stock/availability label into a typed stock state. Only
/// documented forms are recognized; anything else is [`StockState::Unknown`]
/// — availability is never fabricated.
pub fn parse_stock_label(raw: &str) -> StockState {
    let cleaned = clean_display(raw);
    let lowered = cleaned.to_ascii_lowercase();
    if lowered.is_empty() {
        return StockState::Unknown;
    }
    let quantity = parse_grouped_u64(&lowered);
    let backorder = lowered.contains("backorder")
        || lowered.contains("back order")
        || lowered.contains("on order");
    if backorder {
        return match quantity.and_then(|value| nonzero_quantity(value).ok().flatten()) {
            Some(quantity) => StockState::Backorder {
                quantity: Some(quantity),
                lead_time: None,
            },
            None => StockState::Backorder {
                quantity: None,
                lead_time: None,
            },
        };
    }
    if lowered.contains("out of stock")
        || lowered.contains("not in stock")
        || lowered.contains("no stock")
        || lowered == "n/a"
        || lowered == "na"
    {
        return StockState::OutOfStock;
    }
    if lowered.contains("in stock") || cleaned.chars().all(|c| c.is_ascii_digit() || c == ',') {
        return match quantity {
            Some(0) => StockState::OutOfStock,
            Some(value) => match nonzero_quantity(value) {
                Ok(Some(quantity)) => StockState::InStock { quantity },
                _ => StockState::Unknown,
            },
            None => StockState::Unknown,
        };
    }
    StockState::Unknown
}

/// Map a domain offer refusal onto the normalization error the connector
/// reports (a typed acquisition error, never a panic or a silent skip).
pub fn map_offer_error(error: OfferError) -> NormalizeError {
    match error {
        OfferError::Identity(_) => NormalizeError::MissingIdentity,
        OfferError::NegativePrice { .. } => NormalizeError::NegativePrice,
        OfferError::CurrencyMismatch { .. } => NormalizeError::CurrencyConflict,
        OfferError::InvalidTierRange { .. }
        | OfferError::DuplicateTier { .. }
        | OfferError::OverlappingTiers { .. }
        | OfferError::TooManyPriceBreaks { .. }
        | OfferError::InvalidLeadTime { .. } => NormalizeError::InvalidTiers,
        OfferError::AccountScopeRequired | OfferError::AccountScopeUnexpected => {
            NormalizeError::Unsupported
        }
        _ => NormalizeError::Schema,
    }
}

/// The provenance every connector observation carries.
pub fn provenance(
    source: &faktor_commerce::SourceId,
    ctx: &crate::context::AcquireCtx,
    origin: faktor_commerce::offer::ObservationOrigin,
) -> faktor_commerce::offer::OfferProvenance {
    faktor_commerce::offer::OfferProvenance {
        origin,
        source: source.clone(),
        extractor_version: None,
        connector_version: Text::<64>::new(CONNECTOR_VERSION).ok(),
        normalization_version: Text::<64>::new(NORMALIZATION_VERSION).ok(),
        content_digest: None,
        account_scope: ctx.account_scope().cloned(),
        locale: ctx.locale().cloned(),
        market: ctx.market().cloned(),
        source_confidence_bp: None,
    }
}

/// The connector version reported in provenance.
pub const CONNECTOR_VERSION: &str = "faktor-commerce-connectors/1";
/// The normalization version reported in provenance.
pub const NORMALIZATION_VERSION: &str = "normalize/1";

/// Convert one normalized offer into the discovery shape. A discovery price
/// is only ever a hint; `documented_max_staleness_ms` records the source's
/// documented catalog staleness when it has one.
pub fn discovery_from_offer(
    offer: &faktor_commerce::CommercialOffer,
    documented_max_staleness_ms: Option<u64>,
) -> crate::contract::Discovery {
    crate::contract::Discovery {
        source: offer.source.clone(),
        identity: offer.identity.clone(),
        title: offer.title.clone(),
        url: offer.identity.canonical_url.clone(),
        price_hint: offer.cheapest_unit_price(),
        price_visibility: offer.price_visibility,
        stock_hint: match offer.stock {
            faktor_commerce::StockState::Unknown => None,
            state => Some(state),
        },
        freshness: faktor_commerce::Freshness::Live,
        origin: offer.provenance.origin,
        documented_max_staleness_ms,
        observed_at_ms: offer.observed_at_ms,
    }
}

/// Extract the text of a raw JSON token: the unquoted content for a string
/// token, the raw token otherwise. Empty and oversized tokens are refused.
pub fn raw_token_text(raw: &RawValue) -> Result<String, NormalizeError> {
    let token = raw.get().trim();
    if token.is_empty() || token.len() > 512 {
        return Err(NormalizeError::Schema);
    }
    let text = if token.starts_with('"') {
        serde_json::from_str::<String>(token).map_err(|_| NormalizeError::Schema)?
    } else {
        token.to_string()
    };
    if text.trim().is_empty() {
        return Err(NormalizeError::Schema);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> Box<RawValue> {
        serde_json::from_str::<Box<RawValue>>(text).expect("raw value")
    }

    #[test]
    fn money_accepts_plain_and_affixed_decimals_exactly() {
        let cases = [
            ("1.23", Currency::USD, 1_230_000i64),
            ("\"1.23\"", Currency::USD, 1_230_000),
            ("\"$1.23\"", Currency::USD, 1_230_000),
            ("\"US$ 1.23\"", Currency::USD, 1_230_000),
            ("\"1.23 USD\"", Currency::USD, 1_230_000),
            ("\"USD 1.23\"", Currency::USD, 1_230_000),
            // The affix decides the currency: ¥ is never read as USD.
            ("\"¥0.0037\"", Currency::CNY, 3_700),
            ("\"CNY 100\"", Currency::CNY, 100_000_000),
            ("\"€5\"", Currency::EUR, 5_000_000),
            ("\"£2.50\"", Currency::GBP, 2_500_000),
            ("\"1,234.56\"", Currency::USD, 1_234_560_000),
            ("\"1,234\"", Currency::USD, 1_234_000_000),
            ("0", Currency::USD, 0),
            ("\"0.000001\"", Currency::USD, 1),
        ];
        for (token, currency, micros) in cases {
            let money = money_from_raw(&raw(token), currency).expect(token);
            assert_eq!(money.micros, micros, "token {token}");
            assert_eq!(money.currency, currency, "token {token}");
        }
    }

    #[test]
    fn money_refuses_ambiguous_inexact_or_mismatched_tokens() {
        for (token, currency) in [
            ("\"1.2345678\"", Currency::USD), // more than six fractional digits
            ("\"1,23\"", Currency::USD),      // European decimal comma: ambiguous
            ("\"1.2.3\"", Currency::USD),     // two decimal points
            ("\"1e3\"", Currency::USD),       // exponent
            ("\"1.23%\"", Currency::USD),     // percent is not a price
            ("\"abc1.23\"", Currency::USD),   // stray word prefix
            ("\"1.23abc\"", Currency::USD),   // stray word suffix
            ("\"\"", Currency::USD),          // empty
            ("\"   \"", Currency::USD),       // whitespace
            ("\"$ 1.23 $\"", Currency::USD),  // two currency symbols
            ("\"-1.23\"", Currency::USD),     // negative
            ("null", Currency::USD),          // null is not a number token
            ("true", Currency::USD),          // wrong type
            ("\"9223372036854775808.000000\"", Currency::USD), // out of i64 range
            // A declared currency that disagrees with the caller's is a
            // typed conflict, never a silent reinterpretation.
            ("\"¥18.20\"", Currency::USD),
            ("\"€5\"", Currency::USD),
            ("\"CNY 100\"", Currency::USD),
            ("\"$18.20\"", Currency::CNY),
            // A token declaring two different currencies is ambiguous.
            ("\"USD 1.23 EUR\"", Currency::USD),
            ("\"$1.23 CNY\"", Currency::USD),
            // Unmapped symbols are refused, never stripped as noise.
            ("\"₩100\"", Currency::USD),
        ] {
            let parsed = money_from_raw(&raw(token), currency);
            assert!(parsed.is_err(), "token {token} must be refused");
        }
    }

    #[test]
    fn declared_currency_extraction_is_deterministic() {
        assert_eq!(
            money_declared_currency("¥ 18.20").unwrap(),
            (Some(Currency::CNY), "18.20".to_string())
        );
        assert_eq!(
            money_declared_currency("18.20 CNY").unwrap(),
            (Some(Currency::CNY), "18.20".to_string())
        );
        assert_eq!(
            money_declared_currency("€5").unwrap(),
            (Some(Currency::EUR), "5".to_string())
        );
        assert_eq!(
            money_declared_currency("$ 5").unwrap(),
            (Some(Currency::USD), "5".to_string())
        );
        assert_eq!(
            money_declared_currency(" 1.20 ").unwrap(),
            (None, "1.20".to_string())
        );
        assert_eq!(
            money_declared_currency("CNY 1.20 USD"),
            Err(NormalizeError::CurrencyConflict)
        );
        assert_eq!(
            money_declared_currency("USD EUR"),
            Err(NormalizeError::CurrencyConflict)
        );
    }

    #[test]
    fn quantities_refuse_floats_exponents_and_fullwidth_digits() {
        assert_eq!(parse_u64_raw(&raw("42")).unwrap(), 42);
        assert_eq!(parse_u64_raw(&raw("\"42\"")).unwrap(), 42);
        for token in [
            "1.5",
            "\"1.5\"",
            "1e3",
            "\"-1\"",
            "\"１２３\"",
            "\"4 2\"",
            "\"\"",
            "true",
        ] {
            assert!(
                parse_u64_raw(&raw(token)).is_err(),
                "token {token} must be refused"
            );
        }
        assert_eq!(parse_leading_u64("12345 In Stock"), Some(12345));
        assert_eq!(parse_leading_u64("In Stock"), None);
    }

    #[test]
    fn nonzero_quantity_rejects_zero_and_out_of_range() {
        assert_eq!(nonzero_quantity(0).unwrap(), None);
        assert_eq!(nonzero_quantity(1).unwrap().map(|q| q.get()), Some(1));
        assert!(nonzero_quantity(u64::MAX).is_err());
    }

    #[test]
    fn lead_time_labels_are_parsed_conservatively() {
        assert_eq!(
            parse_lead_time_label("8 Weeks"),
            Some(LeadTime::new(56, 56).unwrap())
        );
        assert_eq!(
            parse_lead_time_label("6-8 weeks"),
            Some(LeadTime::new(42, 56).unwrap())
        );
        assert_eq!(
            parse_lead_time_label("56 days"),
            Some(LeadTime::new(56, 56).unwrap())
        );
        assert_eq!(parse_lead_time_label("In Stock"), None);
        assert_eq!(parse_lead_time_label("8"), None);
        assert_eq!(parse_lead_time_label("9999 weeks"), None);
    }

    #[test]
    fn packaging_labels_map_only_documented_forms() {
        assert_eq!(
            packaging_from_label("Tape & Reel (TR)"),
            Some(PackagingType::TapeAndReel)
        );
        assert_eq!(
            packaging_from_label("Digi-Reel®"),
            Some(PackagingType::DigiReel)
        );
        assert_eq!(
            packaging_from_label("Cut Tape (CT)"),
            Some(PackagingType::CutTape)
        );
        assert_eq!(packaging_from_label("Tray"), Some(PackagingType::Tray));
        assert_eq!(packaging_from_label("mystery pack"), None);
        // A reel label must not be misread as cut tape.
        assert_eq!(
            packaging_from_label("Full Reel"),
            Some(PackagingType::FullReel)
        );
    }

    #[test]
    fn mpn_shape_heuristic_is_documented_and_stable() {
        for good in ["TPS5430DDAR", "STM32F407VGT6", "RC0603FR-0710KL"] {
            assert!(looks_like_mpn(good), "{good}");
        }
        for bad in [
            "10k resistor",
            "12345",
            "ABC",
            "https://example.com/part",
            "a b c 1",
            "Ω-123",
        ] {
            assert!(!looks_like_mpn(bad), "{bad}");
        }
    }

    #[test]
    fn identity_text_is_strict_but_display_text_is_sanitized() {
        // A bidi override in an MPN is a typed error, never a sanitized
        // spoof.
        let spoof = "TPS5430\u{202e}DDAR";
        assert!(strict_text::<256>(spoof).is_err());
        // Display text is cleaned and truncated, never rejected for a
        // control character.
        let text = display_text::<16>("  a\u{0}b  ", "fallback").unwrap();
        assert_eq!(text.as_str(), "a b");
        let long = "x".repeat(100);
        let text = display_text::<16>(&long, "fallback").unwrap();
        assert_eq!(text.as_str().len(), 16);
        let empty = display_text::<16>("   ", "fallback").unwrap();
        assert_eq!(empty.as_str(), "fallback");
        // Multi-byte truncation never splits a character.
        let multibyte = "é".repeat(20);
        let text = display_text::<16>(&multibyte, "fallback").unwrap();
        assert!(text.as_str().len() <= 16);
    }

    #[test]
    fn excerpts_are_bounded_and_control_free() {
        let excerpt = sanitize_excerpt("a\u{0}b\nc", 3);
        assert_eq!(excerpt, "a b…");
    }

    #[test]
    fn currency_symbols_map_only_documented_forms() {
        assert_eq!(currency_from_symbol("$"), Some(Currency::USD));
        assert_eq!(currency_from_symbol("USD"), Some(Currency::USD));
        assert_eq!(currency_from_symbol("¥"), Some(Currency::CNY));
        assert_eq!(currency_from_symbol("EUR"), Some(Currency::EUR));
        assert_eq!(currency_from_symbol("dollars"), None);
        assert_eq!(currency_from_symbol(""), None);
    }

    #[test]
    fn grouped_integers_are_strict() {
        assert_eq!(parse_grouped_u64("1234"), Some(1234));
        assert_eq!(parse_grouped_u64("1,234"), Some(1234));
        assert_eq!(parse_grouped_u64("1,234 In Stock"), Some(1234));
        assert_eq!(parse_grouped_u64("12,34"), None);
        // A leading-zero first group is refused: `0,123` is not 123 units.
        assert_eq!(parse_grouped_u64("0,123"), None);
        assert_eq!(parse_grouped_u64("0,123 In Stock"), None);
        assert_eq!(parse_grouped_u64("00,123"), None);
        assert_eq!(parse_grouped_u64("01,234"), None);
        assert_eq!(parse_grouped_u64(",123"), None);
        assert_eq!(parse_grouped_u64("1.5"), None);
        assert_eq!(parse_grouped_u64("1e3"), None);
        assert_eq!(parse_grouped_u64("-1"), None);
        assert_eq!(parse_grouped_u64(""), None);

        // The same documented rule applies to grouped money.
        assert_eq!(
            parse_money_text(Currency::USD, "1,234.56").unwrap().micros,
            1_234_560_000
        );
        for bad in ["0,123", ",123", "12,34", "00,123", "0,123.45"] {
            assert!(
                parse_money_text(Currency::USD, bad).is_err(),
                "{bad:?} must never be read as a larger amount"
            );
        }
    }

    #[test]
    fn stock_labels_never_fabricate_availability() {
        assert_eq!(
            parse_stock_label("1,234 In Stock"),
            StockState::InStock {
                quantity: NonZeroQuantity::new(1234).unwrap()
            }
        );
        assert_eq!(parse_stock_label("0 In Stock"), StockState::OutOfStock);
        assert_eq!(parse_stock_label("Out of Stock"), StockState::OutOfStock);
        assert_eq!(parse_stock_label("N/A"), StockState::OutOfStock);
        assert!(matches!(
            parse_stock_label("500 On Order"),
            StockState::Backorder {
                quantity: Some(_),
                ..
            }
        ));
        assert_eq!(parse_stock_label("plenty"), StockState::Unknown);
        assert_eq!(parse_stock_label("1e9 in stock"), StockState::Unknown);
        assert_eq!(parse_stock_label(""), StockState::Unknown);
    }

    #[test]
    fn raw_token_text_extracts_strings_and_numbers() {
        assert_eq!(raw_token_text(&raw("\" 8 Weeks \"")).unwrap(), " 8 Weeks ");
        assert_eq!(raw_token_text(&raw("1234")).unwrap(), "1234");
        assert!(raw_token_text(&raw("\"\"")).is_err());
    }

    #[test]
    fn nesting_scanner_is_string_aware_and_bounded() {
        assert!(json_nesting_within(br#"{"a":[1,{"b":2}]}"#, 4));
        assert!(!json_nesting_within(br#"{"a":[1,{"b":2}]}"#, 2));
        // Brackets inside strings do not count...
        assert!(json_nesting_within(br#"{"a":"[[[[[[[["}"#, 2));
        // ...and an escaped quote does not end the string.
        assert!(json_nesting_within(br#"{"a":"x\"[[[[[[[["}"#, 2));
        assert!(!json_nesting_within(br#"{"a":"unterminated"#, 4));
        assert!(json_nesting_within(b"", 1));
    }
}
