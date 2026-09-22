//! Part-number normalization and product identity.
//!
//! # Normalization rules
//!
//! [`PartNumberNormalizer`] produces a canonical form plus a structured,
//! **preserved** suffix list. It never strips a suffix from the identity.
//!
//! 1. Full-width ASCII (`U+FF01..=U+FF5E`) and the ideographic space
//!    (`U+3000`) are folded to ASCII; everything else must be ASCII.
//! 2. ASCII whitespace is removed and letters are uppercased:
//!    `STM32F407VGT6`, `STM32 F407 VGT6` and `stm32f407vgt6` all canonicalize
//!    to `STM32F407VGT6`.
//! 3. Other separators (`-`, `_`, `/`, `+`, `.`) are significant: `AB-123`
//!    never equals `AB123`.
//! 4. Trailing tokens joined by a separator are classified and peeled, from
//!    the right, into [`SuffixKind`]s: packaging labels (`TR`, `TR1`,
//!    `T&R`, `TRAY`, `TUBE`, `BULK`, `CT`, `DR`, `FP`, `REEL`), tolerance
//!    (`1%`, `0.1%`, and the passive codes `F G J K M Z L`), voltage
//!    (`25V`, `3V3`, `100V`), temperature grade (`85C`, `125C`, and the
//!    semiconductor codes `I Q A C`).
//! 5. A small set of **glued** packaging tokens is recognized at the end
//!    (`T&R`, `TNR`, `TR`, `TR1`, `TR2`, `TR3`, `TRAY`, `TUBE`, `BULK`,
//!    `REEL`) and a glued percentage (`1%`). Glued voltage/temperature
//!    tokens are deliberately **not** recognized: a wrong split of a base
//!    part number is worse than an unrecognized suffix.
//! 6. Anything not recognized stays in the base. The bias is
//!    false-negative: `TPS5430DDAR` keeps its `AR`, so it never aliases a
//!    different purchasable item.
//!
//! `STM32F407VGT6TR` therefore has base `STM32F407VGT6` and a
//! [`PackagingType::TapeAndReel`] suffix: it is *distinct* from
//! `STM32F407VGT6` (different purchasable item) but related through
//! [`NormalizedPartNumber::base_eq`] and
//! [`NormalizedPartNumber::differs_only_by_packaging`].

use serde::{Deserialize, Serialize};

use crate::packaging::PackagingType;
use crate::text::{is_bidi_control, CanonicalUrl, Text};

/// Hard bound on a raw part number (bytes).
pub const MAX_PART_NUMBER_BYTES: usize = 256;
/// Maximum number of recognized suffix tokens.
pub const MAX_SUFFIX_TOKENS: usize = 4;
/// A glued packaging token is only recognized when this much base remains.
const MIN_GLUED_BASE_BYTES: usize = 3;

/// A rejected part number.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartNumberError {
    /// Empty after normalization.
    #[error("part number is empty")]
    Empty,
    /// Longer than [`MAX_PART_NUMBER_BYTES`].
    #[error("part number exceeds {max} bytes (got {actual})")]
    TooLong { max: usize, actual: usize },
    /// A non-ASCII character outside the full-width ASCII range.
    #[error("part number contains a non-ASCII character at byte {at}")]
    NonAscii { at: usize },
    /// A control character.
    #[error("part number contains a control character at byte {at}")]
    ControlChar { at: usize },
    /// No ASCII letter or digit at all.
    #[error("part number contains no alphanumeric character")]
    NoAlphanumeric,
}

/// The classified kind of a preserved suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "packaging")]
pub enum SuffixKind {
    /// A packaging suffix; the payload is the packaging it implies.
    Packaging(PackagingType),
    /// A tolerance suffix.
    Tolerance,
    /// A voltage rating suffix.
    Voltage,
    /// A temperature grade suffix.
    TemperatureGrade,
    /// A recognized-but-unclassified suffix.
    Other,
}

impl SuffixKind {
    /// A short stable label used in match signals.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Packaging(_) => "packaging",
            Self::Tolerance => "tolerance",
            Self::Voltage => "voltage",
            Self::TemperatureGrade => "temperature_grade",
            Self::Other => "other",
        }
    }
}

/// One preserved suffix token.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuffixToken {
    /// The classified kind.
    pub kind: SuffixKind,
    /// The exact normalized token text.
    pub text: String,
}

/// A normalized part number: canonical form, base, and preserved suffixes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedPartNumber {
    canonical: String,
    base: String,
    suffixes: Vec<SuffixToken>,
}

impl NormalizedPartNumber {
    /// The full canonical string (the identity of the purchasable item).
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// The canonical string without recognized trailing suffixes.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The preserved suffixes, left to right.
    pub fn suffixes(&self) -> &[SuffixToken] {
        &self.suffixes
    }

    /// True when the two canonical forms are identical (same purchasable
    /// item).
    pub fn exact_eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }

    /// True when the two base forms are identical (same die, possibly a
    /// different suffix).
    pub fn base_eq(&self, other: &Self) -> bool {
        self.base == other.base
    }

    /// True when the two part numbers differ only in packaging suffixes.
    pub fn differs_only_by_packaging(&self, other: &Self) -> bool {
        self.base == other.base
            && self.canonical != other.canonical
            && self
                .suffixes
                .iter()
                .all(|s| matches!(s.kind, SuffixKind::Packaging(_)))
            && other
                .suffixes
                .iter()
                .all(|s| matches!(s.kind, SuffixKind::Packaging(_)))
    }

    /// The packaging implied by the (last) packaging suffix.
    pub fn packaging(&self) -> Option<PackagingType> {
        self.suffixes.iter().rev().find_map(|s| match s.kind {
            SuffixKind::Packaging(packaging) => Some(packaging),
            _ => None,
        })
    }

    /// The suffix kinds that differ between two normalized numbers.
    ///
    /// Order is deterministic: kinds that appear only on `self` first (in
    /// suffix order), then kinds that appear only on `other`.
    pub fn differing_suffix_kinds(&self, other: &Self) -> Vec<SuffixKind> {
        let mut kinds: Vec<SuffixKind> = Vec::new();
        for token in &self.suffixes {
            if !other.suffixes.contains(token) && !kinds.contains(&token.kind) {
                kinds.push(token.kind);
            }
        }
        for token in &other.suffixes {
            if !self.suffixes.contains(token) && !kinds.contains(&token.kind) {
                kinds.push(token.kind);
            }
        }
        kinds
    }
}

/// The deterministic part-number normalizer (see module docs for rules).
#[derive(Debug, Clone, Copy, Default)]
pub struct PartNumberNormalizer;

impl PartNumberNormalizer {
    /// A normalizer (stateless).
    pub const fn new() -> Self {
        Self
    }

    /// Normalize a raw part number.
    pub fn normalize(&self, raw: &str) -> Result<NormalizedPartNumber, PartNumberError> {
        if raw.len() > MAX_PART_NUMBER_BYTES {
            return Err(PartNumberError::TooLong {
                max: MAX_PART_NUMBER_BYTES,
                actual: raw.len(),
            });
        }
        let mut canonical = String::with_capacity(raw.len());
        for (at, ch) in raw.char_indices() {
            let mapped = fold_width(ch);
            if !mapped.is_ascii() {
                if mapped.is_control() || is_bidi_control(mapped) {
                    return Err(PartNumberError::ControlChar { at });
                }
                return Err(PartNumberError::NonAscii { at });
            }
            if mapped.is_ascii_whitespace() {
                continue;
            }
            if mapped.is_ascii_control() {
                return Err(PartNumberError::ControlChar { at });
            }
            canonical.push(mapped.to_ascii_uppercase());
        }
        if canonical.is_empty() {
            return Err(PartNumberError::Empty);
        }
        if !canonical.bytes().any(|b| b.is_ascii_alphanumeric()) {
            return Err(PartNumberError::NoAlphanumeric);
        }

        let mut working = canonical.as_str();
        let mut suffixes: Vec<SuffixToken> = Vec::new();
        while suffixes.len() < MAX_SUFFIX_TOKENS {
            let Some(position) = working.rfind(['-', '_', '/', '+']) else {
                break;
            };
            if position == 0 {
                break;
            }
            let tail = &working[position + 1..];
            let Some(kind) = classify_separated_token(tail) else {
                break;
            };
            suffixes.push(SuffixToken {
                kind,
                text: tail.to_string(),
            });
            working = working[..position].trim_end_matches(['-', '_', '/', '+']);
        }
        while suffixes.len() < MAX_SUFFIX_TOKENS {
            let Some((token, kind)) = glued_packaging_token(working) else {
                break;
            };
            let base_len = working.len() - token.len();
            if base_len < MIN_GLUED_BASE_BYTES {
                break;
            }
            suffixes.push(SuffixToken {
                kind,
                text: working[base_len..].to_string(),
            });
            working = &working[..base_len];
        }
        if suffixes.len() < MAX_SUFFIX_TOKENS {
            if let Some(token) = glued_percentage_token(working) {
                let base_len = working.len() - token.len();
                if base_len >= 1 {
                    suffixes.push(SuffixToken {
                        kind: SuffixKind::Tolerance,
                        text: token.to_string(),
                    });
                    working = &working[..base_len];
                }
            }
        }
        suffixes.reverse();
        let base = if working.is_empty() {
            canonical.clone()
        } else {
            working.to_string()
        };
        Ok(NormalizedPartNumber {
            canonical,
            base,
            suffixes,
        })
    }
}

/// Convenience wrapper around [`PartNumberNormalizer::normalize`].
pub fn normalize_part_number(raw: &str) -> Result<NormalizedPartNumber, PartNumberError> {
    PartNumberNormalizer::new().normalize(raw)
}

fn fold_width(ch: char) -> char {
    match ch {
        '\u{3000}' => ' ',
        '\u{ff01}'..='\u{ff5e}' => char::from_u32(ch as u32 - 0xfee0).unwrap_or(ch),
        other => other,
    }
}

fn glued_packaging_token(value: &str) -> Option<(&'static str, SuffixKind)> {
    const GLUED: &[(&str, PackagingType)] = &[
        ("TRAY", PackagingType::Tray),
        ("TUBE", PackagingType::Tube),
        ("BULK", PackagingType::Bulk),
        ("REEL", PackagingType::FullReel),
        ("T&R", PackagingType::TapeAndReel),
        ("TNR", PackagingType::TapeAndReel),
        ("TR1", PackagingType::TapeAndReel),
        ("TR2", PackagingType::TapeAndReel),
        ("TR3", PackagingType::TapeAndReel),
        ("TR", PackagingType::TapeAndReel),
    ];
    GLUED
        .iter()
        .find(|(token, _)| value.ends_with(token))
        .map(|(token, packaging)| (*token, SuffixKind::Packaging(*packaging)))
}

fn glued_percentage_token(value: &str) -> Option<&str> {
    let stripped = value.strip_suffix('%')?;
    let start = stripped
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|index| index + 1)
        .unwrap_or(0);
    let token = &value[start..];
    if token.len() > 1 && is_decimal(&token[..token.len() - 1]) {
        Some(token)
    } else {
        None
    }
}

fn classify_separated_token(token: &str) -> Option<SuffixKind> {
    match token {
        "TR" | "TR1" | "TR2" | "TR3" | "T&R" | "TNR" | "TAPE" | "REEL" => {
            Some(SuffixKind::Packaging(PackagingType::TapeAndReel))
        }
        "TRAY" => Some(SuffixKind::Packaging(PackagingType::Tray)),
        "TUBE" => Some(SuffixKind::Packaging(PackagingType::Tube)),
        "BULK" | "BAG" => Some(SuffixKind::Packaging(PackagingType::Bulk)),
        "CT" | "CT1" => Some(SuffixKind::Packaging(PackagingType::CutTape)),
        "DR" | "DIGIREEL" => Some(SuffixKind::Packaging(PackagingType::DigiReel)),
        "FP" | "FULLREEL" | "PACK" => Some(SuffixKind::Packaging(PackagingType::FactoryPack)),
        "F" | "G" | "J" | "K" | "M" | "Z" | "L" => Some(SuffixKind::Tolerance),
        "I" | "Q" | "A" | "C" => Some(SuffixKind::TemperatureGrade),
        _ => {
            if token.ends_with('%') && is_decimal(&token[..token.len() - 1]) {
                Some(SuffixKind::Tolerance)
            } else if is_voltage_token(token) {
                Some(SuffixKind::Voltage)
            } else if is_temperature_token(token) {
                Some(SuffixKind::TemperatureGrade)
            } else {
                None
            }
        }
    }
}

fn is_decimal(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut digits = 0usize;
    let mut dots = 0usize;
    for byte in value.bytes() {
        if byte.is_ascii_digit() {
            digits += 1;
        } else if byte == b'.' {
            dots += 1;
            if dots > 1 {
                return false;
            }
        } else {
            return false;
        }
    }
    digits > 0
}

fn is_voltage_token(token: &str) -> bool {
    let Some(position) = token.find('V') else {
        return false;
    };
    let (value, rest) = token.split_at(position);
    let rest = &rest[1..];
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && (rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()))
}

fn is_temperature_token(token: &str) -> bool {
    let Some(value) = token.strip_suffix('C') else {
        return false;
    };
    (1..=3).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// A product identity as reported by a source.
///
/// At least one of `manufacturer_part_number`, `source_part_number` or
/// `offer_id` must be present: an offer with no identifier at all can never
/// be matched, cached or quoted deterministically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductIdentity {
    /// Manufacturer name, when the source states one.
    pub manufacturer: Option<Text<128>>,
    /// Manufacturer part number (MPN).
    pub manufacturer_part_number: Option<Text<MAX_PART_NUMBER_BYTES>>,
    /// The source's own part number / SKU.
    pub source_part_number: Option<Text<MAX_PART_NUMBER_BYTES>>,
    /// The source's offer identifier (for example a listing id).
    pub offer_id: Option<Text<512>>,
    /// The canonical product URL, when one exists.
    pub canonical_url: Option<CanonicalUrl>,
    /// The source's category label.
    pub category: Option<Text<128>>,
}

impl ProductIdentity {
    /// An empty identity (invalid until populated and validated).
    pub fn empty() -> Self {
        Self {
            manufacturer: None,
            manufacturer_part_number: None,
            source_part_number: None,
            offer_id: None,
            canonical_url: None,
            category: None,
        }
    }

    /// True when no identifying field is present.
    pub fn is_empty(&self) -> bool {
        self.manufacturer_part_number.is_none()
            && self.source_part_number.is_none()
            && self.offer_id.is_none()
    }

    /// Validate the cross-field invariant.
    pub fn validate(&self) -> Result<(), IdentityError> {
        if self.is_empty() {
            return Err(IdentityError::NoIdentifier);
        }
        Ok(())
    }

    /// The normalized MPN, or `None` when the source did not provide one.
    ///
    /// An MPN that is present but cannot be normalized is a typed error: it
    /// is never silently treated as absent.
    pub fn normalized_mpn(&self) -> Result<Option<NormalizedPartNumber>, PartNumberError> {
        match &self.manufacturer_part_number {
            Some(mpn) => normalize_part_number(mpn.as_str()).map(Some),
            None => Ok(None),
        }
    }

    /// The normalized source part number, or `None`.
    pub fn normalized_source_part_number(
        &self,
    ) -> Result<Option<NormalizedPartNumber>, PartNumberError> {
        match &self.source_part_number {
            Some(part) => normalize_part_number(part.as_str()).map(Some),
            None => Ok(None),
        }
    }
}

/// A rejected identity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// No MPN, source part number or offer id.
    #[error("product identity carries no identifier")]
    NoIdentifier,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(raw: &str) -> NormalizedPartNumber {
        normalize_part_number(raw).expect("normalizable")
    }

    #[test]
    fn case_and_space_insensitive_normalization() {
        let a = normalized("STM32F407VGT6");
        let b = normalized("STM32 F407 VGT6");
        let c = normalized("stm32f407vgt6");
        let d = normalized("  Stm32\tF407\nVGT6  ");
        for other in [&b, &c, &d] {
            assert!(a.exact_eq(other), "{other:?}");
            assert_eq!(a.canonical(), "STM32F407VGT6");
            assert_eq!(a.base(), "STM32F407VGT6");
            assert!(a.suffixes().is_empty());
        }
    }

    #[test]
    fn full_width_ascii_is_folded() {
        assert_eq!(
            normalized("ＳＴＭ３２Ｆ４０７ＶＧＴ６").canonical(),
            "STM32F407VGT6"
        );
        assert_eq!(normalized("STM32\u{3000}F407").canonical(), "STM32F407");
    }

    #[test]
    fn packaging_suffix_is_distinct_but_related() {
        let bare = normalized("STM32F407VGT6");
        let reel = normalized("STM32F407VGT6TR");
        assert!(!bare.exact_eq(&reel));
        assert!(bare.base_eq(&reel));
        assert!(reel.differs_only_by_packaging(&bare));
        assert_eq!(reel.base(), "STM32F407VGT6");
        assert_eq!(reel.packaging(), Some(PackagingType::TapeAndReel));
        assert_eq!(reel.suffixes().len(), 1);
        assert_eq!(reel.suffixes()[0].text, "TR");
        assert_eq!(
            reel.differing_suffix_kinds(&bare),
            vec![SuffixKind::Packaging(PackagingType::TapeAndReel)]
        );

        let tray = normalized("STM32F407VGT6TRAY");
        assert!(bare.base_eq(&tray));
        assert!(!reel.exact_eq(&tray));
        assert!(reel.differs_only_by_packaging(&tray));
        assert_eq!(tray.packaging(), Some(PackagingType::Tray));
    }

    #[test]
    fn packaging_suffixes_are_never_stripped_from_identity() {
        let a = normalized("TPS5430DDAR");
        let b = normalized("TPS5430DDA");
        assert!(!a.exact_eq(&b));
        assert!(!a.base_eq(&b), "an unrecognized suffix must never be split");
        assert!(!a.differs_only_by_packaging(&b));
        assert_eq!(a.packaging(), None);
        assert_eq!(a.suffixes().len(), 0);
    }

    #[test]
    fn separated_suffixes_are_classified_in_order() {
        let part = normalized("ABC-25V-TR");
        assert_eq!(part.canonical(), "ABC-25V-TR");
        assert_eq!(part.base(), "ABC");
        assert_eq!(
            part.suffixes().iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec![
                SuffixKind::Voltage,
                SuffixKind::Packaging(PackagingType::TapeAndReel)
            ]
        );

        assert_eq!(
            normalized("CAP-1%").suffixes()[0].kind,
            SuffixKind::Tolerance
        );
        assert_eq!(
            normalized("CAP-0.1%").suffixes()[0].kind,
            SuffixKind::Tolerance
        );
        assert_eq!(
            normalized("CAP-1%").base(),
            "CAP",
            "glued percentages are peeled"
        );
        assert_eq!(
            normalized("MCU-85C").suffixes()[0].kind,
            SuffixKind::TemperatureGrade
        );
        assert_eq!(
            normalized("MCU-125C").suffixes()[0].kind,
            SuffixKind::TemperatureGrade
        );
        assert_eq!(
            normalized("MCU-I").suffixes()[0].kind,
            SuffixKind::TemperatureGrade
        );
        assert_eq!(
            normalized("RES-F").suffixes()[0].kind,
            SuffixKind::Tolerance
        );
        assert_eq!(
            normalized("CAP-3V3").suffixes()[0].kind,
            SuffixKind::Voltage
        );
        assert_eq!(
            normalized("CAP-100V").suffixes()[0].kind,
            SuffixKind::Voltage
        );
        assert_eq!(
            normalized("CAP-CT").suffixes()[0].kind,
            SuffixKind::Packaging(PackagingType::CutTape)
        );
        assert_eq!(
            normalized("CAP-DR").suffixes()[0].kind,
            SuffixKind::Packaging(PackagingType::DigiReel)
        );
    }

    #[test]
    fn unrecognized_tails_stay_in_the_base() {
        for raw in ["ATmega328P-AU", "ABC-100", "ABC-XX", "ABC-1234", "LM358DR"] {
            let part = normalized(raw);
            assert!(
                part.suffixes().is_empty(),
                "{raw} must keep its tail in the base"
            );
            assert_eq!(part.base(), part.canonical());
        }
    }

    #[test]
    fn separators_are_significant() {
        assert!(!normalized("AB-123").exact_eq(&normalized("AB123")));
        assert!(!normalized("AB_123").exact_eq(&normalized("AB123")));
        assert_eq!(normalized("AB-123").base(), "AB-123");
        assert_eq!(normalized("ABC--TR").base(), "ABC");
    }

    #[test]
    fn temperature_and_tolerance_are_not_packaging() {
        let i = normalized("MCU-I");
        let q = normalized("MCU-Q");
        assert!(!i.differs_only_by_packaging(&q));
        assert_eq!(
            i.differing_suffix_kinds(&q),
            vec![SuffixKind::TemperatureGrade],
            "kinds dedupe but a difference is reported"
        );
        let f = normalized("RES-F");
        let k = normalized("RES-K");
        assert_eq!(f.differing_suffix_kinds(&k), vec![SuffixKind::Tolerance]);
        assert_eq!(f.differing_suffix_kinds(&f), Vec::new());
        assert_eq!(
            f.differing_suffix_kinds(&normalized("RES")),
            vec![SuffixKind::Tolerance]
        );
    }

    #[test]
    fn hostile_part_numbers_are_typed_errors() {
        assert!(matches!(
            normalize_part_number(""),
            Err(PartNumberError::Empty)
        ));
        assert!(matches!(
            normalize_part_number("   "),
            Err(PartNumberError::Empty)
        ));
        assert!(matches!(
            normalize_part_number("---"),
            Err(PartNumberError::NoAlphanumeric)
        ));
        assert!(matches!(
            normalize_part_number("ABC\u{0}DEF"),
            Err(PartNumberError::ControlChar { .. })
        ));
        assert!(matches!(
            normalize_part_number("ABC\u{202e}DEF"),
            Err(PartNumberError::ControlChar { .. })
        ));
        assert!(matches!(
            normalize_part_number("AB\u{4e2d}CD"),
            Err(PartNumberError::NonAscii { .. })
        ));
        let long = "A".repeat(MAX_PART_NUMBER_BYTES + 1);
        assert!(matches!(
            normalize_part_number(&long),
            Err(PartNumberError::TooLong { .. })
        ));
        let huge = format!("STM32{}", "F".repeat(4 * 1024 * 1024));
        assert!(matches!(
            normalize_part_number(&huge),
            Err(PartNumberError::TooLong { .. })
        ));
    }

    #[test]
    fn normalizer_never_panics_on_adversarial_suffix_soup() {
        for raw in [
            "TR",
            "-TR",
            "TR-",
            "A-TR-TR-TR-TR-TR",
            "A-%%%%",
            "A-1.2.3%",
            "A-",
            "-",
            "A--B",
            "A//B",
            "A++B",
            "%%%",
            "1",
            "%",
            "A-999999999999999999999999999999999V",
        ] {
            let result = normalize_part_number(raw);
            assert!(result.is_ok() || result.is_err(), "no panic for {raw:?}");
        }
    }

    #[test]
    fn normalized_part_number_serde_round_trips() {
        let part = normalized("STM32F407VGT6TR");
        let json = serde_json::to_string(&part).expect("serialize");
        assert_eq!(
            serde_json::from_str::<NormalizedPartNumber>(&json).expect("round trip"),
            part
        );
        assert!(serde_json::from_str::<NormalizedPartNumber>("{\"canonical\":\"X\"}").is_err());
    }

    #[test]
    fn identity_requires_an_identifier() {
        let empty = ProductIdentity::empty();
        assert!(empty.is_empty());
        assert!(matches!(empty.validate(), Err(IdentityError::NoIdentifier)));

        let mut identity = ProductIdentity::empty();
        identity.offer_id = Some(Text::new("offer-1").expect("valid"));
        assert!(identity.validate().is_ok());
        assert!(identity.normalized_mpn().expect("no mpn").is_none());
        assert!(identity
            .normalized_source_part_number()
            .expect("no part")
            .is_none());

        identity.manufacturer_part_number = Some(Text::new("stm32f407vgt6tr").expect("valid"));
        assert_eq!(
            identity
                .normalized_mpn()
                .expect("normalizable")
                .expect("present")
                .canonical(),
            "STM32F407VGT6TR"
        );
    }

    #[test]
    fn identity_serde_is_strict_and_bounded() {
        let json = r#"{
            "manufacturer": "STMicroelectronics",
            "manufacturer_part_number": "STM32F407VGT6",
            "source_part_number": "C1234",
            "offer_id": "offer-9",
            "canonical_url": "https://www.lcsc.com/product/C1234.html",
            "category": "Microcontrollers"
        }"#;
        let identity: ProductIdentity = serde_json::from_str(json).expect("valid");
        assert_eq!(identity.validate(), Ok(()));
        assert_eq!(
            identity.manufacturer.as_ref().expect("present").as_str(),
            "STMicroelectronics"
        );
        let round = serde_json::to_string(&identity).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ProductIdentity>(&round).expect("round trip"),
            identity
        );

        assert!(serde_json::from_str::<ProductIdentity>("{}").is_ok());
        assert!(serde_json::from_str::<ProductIdentity>(r#"{"unexpected":"x"}"#).is_err());
        let long = "M".repeat(MAX_PART_NUMBER_BYTES + 1);
        assert!(serde_json::from_str::<ProductIdentity>(&format!(
            r#"{{"manufacturer_part_number":"{long}"}}"#
        ))
        .is_err());
        assert!(serde_json::from_str::<ProductIdentity>(
            r#"{"manufacturer_part_number":"AB\u202eCD"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ProductIdentity>(
            r#"{"canonical_url":"https://user:pass@evil.com/"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ProductIdentity>(
            r#"{"canonical_url":"javascript:alert(1)"}"#
        )
        .is_err());
    }
}
