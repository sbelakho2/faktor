//! The curated 1688 site dictionary: semantic field labels, deterministic
//! query expansion and challenge markers.
//!
//! The dictionary is curated data, not a model: query expansion only appends
//! synonyms listed here (deterministic, bounded), and field labels map the
//! Chinese marketplace vocabulary onto [`Field`]s. Nothing here is learned at
//! runtime.

use faktor_commerce::money::Currency;
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::StockState;

use crate::contract::capture::ChallengeMarkers;
use crate::contract::extract::Field;
use crate::normalize;

/// `(label, field)` — the semantic mapping. Longer labels first so `起批量`
/// is not shadowed by `起批`.
pub const FIELD_LABELS: &[(&str, Field)] = &[
    ("起订量", Field::MinimumOrder),
    ("最小起订量", Field::MinimumOrder),
    ("起批量", Field::MinimumOrder),
    ("起批", Field::MinimumOrder),
    ("库存", Field::Stock),
    ("现货", Field::Stock),
    ("可售数量", Field::Stock),
    ("价格区间", Field::Price),
    ("区间价", Field::Price),
    ("单 价", Field::Price),
    ("价格", Field::Price),
    ("单价", Field::Price),
    ("规格", Field::Specification),
    ("尺码", Field::Specification),
    ("型号", Field::Model),
    ("产品型号", Field::Model),
    ("货号", Field::Model),
    ("厂家", Field::Manufacturer),
    ("工厂", Field::Manufacturer),
    ("生产厂家", Field::Manufacturer),
    ("供应商", Field::Supplier),
    ("商家", Field::Supplier),
    ("货期", Field::LeadTime),
    ("交期", Field::LeadTime),
    ("发货时间", Field::LeadTime),
    ("整箱数量", Field::OrderMultiple),
    ("装箱数", Field::OrderMultiple),
    ("询价", Field::InquiryOnly),
    ("面议", Field::InquiryOnly),
    ("联系供应商", Field::InquiryOnly),
    ("求购", Field::InquiryOnly),
    ("报价", Field::InquiryOnly),
];

/// The semantic field for a curated label, when it exists.
pub fn field_for_label(label: &str) -> Option<Field> {
    let trimmed = label.trim().trim_end_matches([':', '：']);
    FIELD_LABELS
        .iter()
        .find(|(known, _)| *known == trimmed)
        .map(|(_, field)| *field)
}

/// Curated query expansion pairs: `(token, synonyms)`.
pub const QUERY_EXPANSIONS: &[(&str, &[&str])] = &[
    ("数据线", &["USB线", "充电线"]),
    ("充电器", &["适配器", "电源适配器"]),
    ("蓝牙耳机", &["无线耳机", "TWS耳机"]),
    ("外壳", &["机壳", "壳体"]),
    ("电机", &["马达"]),
    ("电容", &["电容器"]),
    ("电阻", &["电阻器"]),
    ("连接器", &["接插件", "端子"]),
    ("螺丝", &["螺钉", "紧固件"]),
    ("包装盒", &["彩盒", "纸盒"]),
];

/// Deterministic, bounded query expansion. The input is preserved verbatim
/// and at most [`MAX_EXPANSIONS`] curated synonyms are appended; the result
/// never exceeds [`MAX_QUERY_BYTES`]. No model is involved.
pub fn expand_query(query: &str) -> String {
    /// At most this many synonyms are appended.
    const MAX_EXPANSIONS: usize = 3;
    const MAX_QUERY_BYTES: usize = 512;
    let mut out = normalize::truncate_chars(query.trim(), MAX_QUERY_BYTES);
    let mut appended = 0usize;
    for (token, synonyms) in QUERY_EXPANSIONS {
        if appended >= MAX_EXPANSIONS {
            break;
        }
        if !out.contains(token) {
            continue;
        }
        for synonym in *synonyms {
            if appended >= MAX_EXPANSIONS {
                break;
            }
            let candidate = format!(" {synonym}");
            if out.len() + candidate.len() > MAX_QUERY_BYTES {
                break;
            }
            if out.contains(synonym) {
                continue;
            }
            out.push_str(&candidate);
            appended += 1;
        }
    }
    out
}

/// True when the text states inquiry-only pricing (询价 / RFQ).
pub fn is_inquiry_text(text: &str) -> bool {
    ["询价", "面议", "联系供应商", "求购", "报价"]
        .iter()
        .any(|marker| text.contains(marker))
}

/// Parse a CNY amount from rendered text (`¥36.00`, `36.00元`, `2.00-40.00`).
pub fn money_from_text(raw: &str) -> Option<faktor_commerce::Money> {
    let trimmed = raw.trim().trim_end_matches(['元', '圆', '块']);
    let first_token = trimmed
        .split(['-', '~', '—'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    normalize::parse_money_text(Currency::CNY, first_token).ok()
}

/// Parse a headline low/high range from text.
pub fn money_range_from_text(
    raw: &str,
) -> Option<(faktor_commerce::Money, faktor_commerce::Money)> {
    let trimmed = raw.trim().trim_end_matches(['元', '圆', '块']);
    for separator in ['-', '~', '—'] {
        if let Some((low, high)) = trimmed.split_once(separator) {
            let low = normalize::parse_money_text(Currency::CNY, low.trim()).ok()?;
            let high = normalize::parse_money_text(Currency::CNY, high.trim()).ok()?;
            if low.micros <= high.micros {
                return Some((low, high));
            }
        }
    }
    None
}

/// Parse a quantity from text (`2 件`, `起订量 10`).
pub fn quantity_from_text(raw: &str) -> Option<NonZeroQuantity> {
    normalize::parse_leading_u64(raw)
        .and_then(|value| normalize::nonzero_quantity(value).ok().flatten())
}

/// Parse a stock state from text (`库存 1200 件`, `缺货`).
pub fn stock_from_text(raw: &str) -> StockState {
    if raw.contains("缺货") || raw.contains("无货") {
        return StockState::OutOfStock;
    }
    let digits: String = raw
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    if let Ok(value) = digits.parse::<u64>() {
        if let Ok(Some(quantity)) = normalize::nonzero_quantity(value) {
            return StockState::InStock { quantity };
        }
    }
    normalize::parse_stock_label(raw)
}

/// The 1688 challenge markers (curated, deterministic).
pub const CHALLENGE_MARKERS: ChallengeMarkers = ChallengeMarkers {
    login: &["请登录", "登录后查看", "请先登录"],
    captcha: &["验证码", "captcha", "请输入验证码"],
    slider: &["拖动滑块", "安全滑块", "slide to verify"],
    interstitial: &["安全验证", "verify you are human", "人机验证"],
    access_denied: &["访问受限", "access denied", "无访问权限"],
    rate_limit: &["访问过于频繁", "too many requests", "请求过于频繁"],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_map_to_semantic_fields() {
        assert_eq!(field_for_label("起订量"), Some(Field::MinimumOrder));
        assert_eq!(field_for_label("价格："), Some(Field::Price));
        assert_eq!(field_for_label("型号"), Some(Field::Model));
        assert_eq!(field_for_label("厂家"), Some(Field::Manufacturer));
        assert_eq!(field_for_label("库存"), Some(Field::Stock));
        assert_eq!(field_for_label("未知标签"), None);
    }

    #[test]
    fn query_expansion_is_deterministic_and_bounded() {
        let expanded = expand_query("USB 数据线");
        assert_eq!(expanded, "USB 数据线 USB线 充电线");
        assert_eq!(expand_query("USB 数据线"), expanded);
        assert_eq!(expand_query("generic cable"), "generic cable");
        let long = format!("数据线 {}", "x".repeat(600));
        assert!(expand_query(&long).len() <= 512);
    }

    #[test]
    fn text_parsing_is_exact() {
        assert_eq!(
            money_from_text("¥36.00")
                .expect("money")
                .to_decimal_string(),
            "36.000000"
        );
        assert_eq!(
            money_from_text("36.00元")
                .expect("money")
                .to_decimal_string(),
            "36.000000"
        );
        let (low, high) = money_range_from_text("¥2.00 - ¥40.00").expect("range");
        assert_eq!(low.to_decimal_string(), "2.000000");
        assert_eq!(high.to_decimal_string(), "40.000000");
        assert!(money_range_from_text("¥40.00 - ¥2.00").is_none());
        assert_eq!(quantity_from_text("2 件").expect("qty").get(), 2);
        assert_eq!(
            stock_from_text("库存 1200 件"),
            StockState::InStock {
                quantity: NonZeroQuantity::new(1200).expect("qty")
            }
        );
        assert_eq!(stock_from_text("缺货"), StockState::OutOfStock);
        assert!(is_inquiry_text("价格：询价"));
        assert!(!is_inquiry_text("价格：¥36.00"));
    }
}
