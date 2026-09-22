//! The curated Alibaba.com dictionary: buyer-visible field labels, documented
//! seller-surface vocabulary, deterministic query expansion and challenge
//! markers. Curated data only — never learned at runtime.

use crate::contract::capture::ChallengeMarkers;
use crate::contract::extract::Field;

/// `(label, field)` for the structural-DOM and rendered-text strategies.
pub const FIELD_LABELS: &[(&str, Field)] = &[
    ("Min. Order", Field::MinimumOrder),
    ("Min Order", Field::MinimumOrder),
    ("MOQ", Field::MinimumOrder),
    ("Unit Price", Field::Price),
    ("Price", Field::Price),
    ("Price Range", Field::Price),
    ("Stock", Field::Stock),
    ("Model Number", Field::Model),
    ("Model No.", Field::Model),
    ("Specification", Field::Specification),
    ("Manufacturer", Field::Manufacturer),
    ("Supplier", Field::Supplier),
    ("Company", Field::Supplier),
    ("Lead Time", Field::LeadTime),
    ("Delivery", Field::LeadTime),
    ("Packaging", Field::OrderMultiple),
    ("Contact Supplier", Field::InquiryOnly),
    ("Request for Quotation", Field::InquiryOnly),
    ("Get Latest Price", Field::InquiryOnly),
];

/// The semantic field for a curated label.
pub fn field_for_label(label: &str) -> Option<Field> {
    let trimmed = label.trim().trim_end_matches([':', '：']);
    FIELD_LABELS
        .iter()
        .find(|(known, _)| *known == trimmed)
        .map(|(_, field)| *field)
}

/// Curated expansion pairs for buyer sourcing queries.
pub const QUERY_EXPANSIONS: &[(&str, &[&str])] = &[
    ("usb cable", &["usb charger cable", "type-c cable"]),
    ("power adapter", &["ac dc adapter", "power supply"]),
    ("wireless earbuds", &["tws earbuds", "bluetooth earphones"]),
    ("enclosure", &["housing", "case"]),
    ("motor", &["electric motor", "dc motor"]),
    ("capacitor", &["electrolytic capacitor"]),
    ("connector", &["terminal", "plug"]),
    ("screw", &["fastener", "bolt"]),
    ("packaging box", &["gift box", "paper box"]),
];

/// Deterministic, bounded query expansion (input preserved verbatim).
pub fn expand_query(query: &str) -> String {
    const MAX_EXPANSIONS: usize = 3;
    const MAX_QUERY_BYTES: usize = 512;
    let mut out = crate::normalize::truncate_chars(query.trim(), MAX_QUERY_BYTES);
    let mut appended = 0usize;
    for (token, synonyms) in QUERY_EXPANSIONS {
        if appended >= MAX_EXPANSIONS {
            break;
        }
        if !out.to_lowercase().contains(token) {
            continue;
        }
        for synonym in *synonyms {
            if appended >= MAX_EXPANSIONS {
                break;
            }
            let candidate = format!(" {synonym}");
            if out.len() + candidate.len() > MAX_QUERY_BYTES || out.contains(synonym) {
                continue;
            }
            out.push_str(&candidate);
            appended += 1;
        }
    }
    out
}

/// True when the text states contact-supplier / RFQ-only pricing.
pub fn is_inquiry_text(text: &str) -> bool {
    let lowered = text.to_lowercase();
    [
        "contact supplier",
        "request for quotation",
        "get latest price",
        "rfq",
        "inquire",
        "询价",
        "联系供应商",
    ]
    .iter()
    .any(|marker| lowered.contains(&marker.to_lowercase()))
}

/// The Alibaba challenge markers.
pub const CHALLENGE_MARKERS: ChallengeMarkers = ChallengeMarkers {
    login: &["sign in", "log in to continue", "请登录"],
    captcha: &["captcha", "verification code", "验证码"],
    slider: &["slide to verify", "security slider", "拖动滑块"],
    interstitial: &["verify you are human", "security check", "安全验证"],
    access_denied: &["access denied", "forbidden", "访问受限"],
    rate_limit: &["too many requests", "rate limit", "访问过于频繁"],
};

/// The stable supplier-profile fields the Alibaba DOM exposes.
pub const SUPPLIER_FIELDS: &[&str] = &[
    "companyName",
    "country",
    "businessType",
    "yearEstablished",
    "responseRate",
    "verified",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_expansion_are_curated() {
        assert_eq!(field_for_label("Min. Order"), Some(Field::MinimumOrder));
        assert_eq!(
            field_for_label("Contact Supplier"),
            Some(Field::InquiryOnly)
        );
        assert_eq!(field_for_label("nope"), None);
        assert_eq!(
            expand_query("usb cable"),
            "usb cable usb charger cable type-c cable"
        );
        assert_eq!(expand_query("usb cable"), expand_query("usb cable"));
        assert!(is_inquiry_text("Contact Supplier"));
        assert!(is_inquiry_text("Get Latest Price"));
        assert!(!is_inquiry_text("US $36.00"));
    }
}
