//! Lazy tool exposure and deterministic activation (`docs/acquire.md` §4).
//!
//! A tool registered [`ToolExposure::Normal`] is exposed exactly as before:
//! the per-phase registry policy decides. A tool registered
//! [`ToolExposure::Lazy`] is invisible to the model — zero schema bytes,
//! zero schema tokens — until it is ACTIVATED, and once active it is exposed
//! only in the phases its [`PhaseMask`] names.
//!
//! Activation is deterministic and local. There is NO classifier, embedding,
//! model call or network probe anywhere on this path: an ASCII word-boundary
//! signal match, a first-party product-URL authority match, or the explicit
//! `/source on` session flag. The false-negative policy is normative (spec
//! §2/§4): an unrelated turn must not pay for the tool, so every rule errs
//! toward NOT activating.
//!
//! Policy, precisely:
//!
//! * A signal matches only as a WHOLE TOKEN: the byte before and the byte
//!   after the matched span must be a non-word byte (ASCII alphanumeric or
//!   `_` is a word byte) or a string edge. Matching is ASCII
//!   case-insensitive. `supplier` therefore matches neither `suppliers` nor
//!   `supplier.rs`, and `1688` matches neither `TPS1688` nor `21688`.
//!   Non-ASCII triggers never match (a deliberate false negative).
//! * [`ToolTrigger::Signal`] is decisive: one match activates.
//! * [`ToolTrigger::WeakSignal`] is never sufficient. `supplier` alone — the
//!   Rust trait discussion — does not activate, and weak signals never
//!   accumulate into a verdict. When a weak signal co-occurs with a decisive
//!   signal, it is the decisive signal that activated.
//! * [`ToolTrigger::ProductUrl`] activates immediately when the text carries
//!   an `http(s)://` URL whose authority is exactly the configured host or a
//!   subdomain of it AND whose path is non-root. A spoofed path
//!   (`https://evil.com/detail.1688.com/x`) never matches on authority; a
//!   bare homepage link does not activate.
//! * [`ToolTrigger::SessionFlag`] is strongest: the LAST flag occurrence in
//!   one text wins (`on` wins a same-position tie) and a flag verdict
//!   OVERRIDES signal matches in the same text, so `/source off` reliably
//!   deactivates even next to a marketplace mention.
//!
//! Activation is sticky: a signal activates, and only an explicit `off` flag
//! (or an explicit [`ToolActivationSet::deactivate`]) turns a tool off again.

use std::collections::BTreeSet;

use faktor_core::model::RouterPhase;

// --------------------------------------------------------------------------
// Phase mask
// --------------------------------------------------------------------------

/// The set of router phases a lazy tool may be exposed in. A bitmask over
/// [`RouterPhase::ALL`] (append-only order): the mask IS the lazy tool's
/// complete phase policy, because the class-based policy alone would refuse a
/// `Network`-class tool in `Plan`/`Explore`/`Retrieve` (spec §4 requires it
/// there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PhaseMask(u16);

const PHASE_COUNT: usize = RouterPhase::ALL.len();

fn phase_bit(phase: RouterPhase) -> u16 {
    let index = RouterPhase::ALL
        .iter()
        .position(|candidate| *candidate == phase)
        .expect("RouterPhase::ALL is exhaustive");
    1u16 << index
}

impl PhaseMask {
    /// No phase at all.
    pub const NONE: PhaseMask = PhaseMask(0);
    /// Every router phase (used by tests and by tools that genuinely belong
    /// everywhere; the normative Acquire policy is narrower).
    pub const ALL: PhaseMask = PhaseMask((1u16 << PHASE_COUNT) - 1);

    /// The mask containing exactly `phases`.
    pub fn of(phases: impl IntoIterator<Item = RouterPhase>) -> Self {
        phases
            .into_iter()
            .fold(Self::NONE, |mask, phase| mask.with(phase))
    }

    /// `self` plus `phase`.
    pub fn with(self, phase: RouterPhase) -> Self {
        PhaseMask(self.0 | phase_bit(phase))
    }

    /// `self` minus `phase`.
    pub fn without(self, phase: RouterPhase) -> Self {
        PhaseMask(self.0 & !phase_bit(phase))
    }

    pub fn contains(self, phase: RouterPhase) -> bool {
        self.0 & phase_bit(phase) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bits (bit `i` = `RouterPhase::ALL[i]`).
    pub fn bits(self) -> u16 {
        self.0
    }

    /// Rebuild from bits; bits outside [`RouterPhase::ALL`] are masked off
    /// (deterministic, never a panic on hostile input).
    pub fn from_bits(bits: u16) -> Self {
        PhaseMask(bits & Self::ALL.0)
    }

    /// The phases in it, in [`RouterPhase::ALL`] order.
    pub fn phases(self) -> Vec<RouterPhase> {
        RouterPhase::ALL
            .into_iter()
            .filter(|phase| self.contains(*phase))
            .collect()
    }
}

impl std::ops::BitOr for PhaseMask {
    type Output = PhaseMask;

    fn bitor(self, rhs: PhaseMask) -> PhaseMask {
        PhaseMask(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PhaseMask {
    fn bitor_assign(&mut self, rhs: PhaseMask) {
        self.0 |= rhs.0;
    }
}

// --------------------------------------------------------------------------
// Triggers
// --------------------------------------------------------------------------

/// One deterministic activation signal of a lazy tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolTrigger {
    /// A whole-token, ASCII case-insensitive phrase. A match activates.
    Signal(String),
    /// A whole-token, ASCII case-insensitive phrase that can never activate
    /// on its own (false-negative preference). `supplier` in a Rust trait
    /// discussion must not expose a commerce tool; a weak signal is inert
    /// unless a decisive signal co-occurs — and then the decisive signal is
    /// what activated.
    WeakSignal(String),
    /// A first-party product URL: `http(s)://` scheme, an authority that is
    /// exactly `host_suffix` or a subdomain of it, and a non-root path.
    /// Activates immediately.
    ProductUrl { host_suffix: String },
    /// The explicit session flag. The last occurrence in one text wins (`on`
    /// wins a same-position tie); a flag verdict overrides signals in the
    /// same text, so `/source off` is reliable.
    SessionFlag { on: String, off: String },
}

impl ToolTrigger {
    pub fn signal(phrase: impl Into<String>) -> Self {
        Self::Signal(phrase.into())
    }

    pub fn weak_signal(phrase: impl Into<String>) -> Self {
        Self::WeakSignal(phrase.into())
    }

    pub fn product_url(host_suffix: impl Into<String>) -> Self {
        Self::ProductUrl {
            host_suffix: host_suffix.into(),
        }
    }

    pub fn session_flag(on: impl Into<String>, off: impl Into<String>) -> Self {
        Self::SessionFlag {
            on: on.into(),
            off: off.into(),
        }
    }

    /// Whether one match of this trigger is enough to activate.
    pub fn is_decisive(&self) -> bool {
        matches!(self, Self::Signal(_) | Self::ProductUrl { .. })
    }
}

/// What one trigger scan decided for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerVerdict {
    Activate,
    Deactivate,
    None,
}

/// The normative Acquire strong signals (`docs/acquire.md` §4, verbatim).
/// `1688.com`/`Alibaba.com` are subsumed by `1688`/`Alibaba` under
/// whole-token matching; they are kept so the vocabulary mirrors the spec.
pub const ACQUIRE_STRONG_SIGNALS: &[&str] = &[
    "1688",
    "1688.com",
    "Alibaba",
    "Alibaba.com",
    "LCSC",
    "Mouser",
    "DigiKey",
    "sourcing",
    "source this part",
    "BOM pricing",
    "component pricing",
    "buy 5,000",
    "find manufacturers",
    "quote this BOM",
];

/// The normative Acquire weak signals: `supplier` alone must NOT activate
/// (a Rust `supplier` trait discussion is not procurement).
pub const ACQUIRE_WEAK_SIGNALS: &[&str] = &["supplier"];

/// The first-party marketplace hosts whose product URLs activate immediately.
pub const ACQUIRE_PRODUCT_URL_HOSTS: &[&str] = &[
    "1688.com",
    "alibaba.com",
    "lcsc.com",
    "mouser.com",
    "digikey.com",
];

/// The explicit Acquire session flag (spec §4).
pub const ACQUIRE_FLAG_ON: &str = "/source on";
/// The explicit Acquire deactivation flag (spec §4).
pub const ACQUIRE_FLAG_OFF: &str = "/source off";

/// The complete normative Acquire trigger vocabulary (spec §4): strong
/// signals, the weak `supplier` co-signal, first-party product URLs, and the
/// `/source on|off` session flag.
pub fn acquire_source_triggers() -> Vec<ToolTrigger> {
    let mut triggers: Vec<ToolTrigger> = ACQUIRE_STRONG_SIGNALS
        .iter()
        .map(|phrase| ToolTrigger::signal(*phrase))
        .collect();
    triggers.extend(
        ACQUIRE_WEAK_SIGNALS
            .iter()
            .map(|phrase| ToolTrigger::weak_signal(*phrase)),
    );
    triggers.extend(
        ACQUIRE_PRODUCT_URL_HOSTS
            .iter()
            .map(|host| ToolTrigger::product_url(*host)),
    );
    triggers.push(ToolTrigger::session_flag(ACQUIRE_FLAG_ON, ACQUIRE_FLAG_OFF));
    triggers
}

/// The §4 phase policy of the future `source_market` tool when active:
/// `Plan`/`Explore`/`Retrieve`/`Implement` yes; `Debug` optional — chosen ON
/// here (a failing acquisition is debugged with the same live query surface);
/// `Review` usually no — chosen OFF (review inspects existing evidence, it
/// does not acquire); `TestAnalysis`/`Summarize`/`Compact`/`Title`/`Embed`
/// no.
pub fn acquire_source_phases() -> PhaseMask {
    PhaseMask::of([
        RouterPhase::Plan,
        RouterPhase::Explore,
        RouterPhase::Retrieve,
        RouterPhase::Implement,
        RouterPhase::Debug,
    ])
}

// --------------------------------------------------------------------------
// Matching engine
// --------------------------------------------------------------------------

/// A word byte for whole-token matching: ASCII alphanumerics and `_`.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Both match edges must sit on a non-word byte or a string edge.
fn boundary_ok(bytes: &[u8], start: usize, len: usize) -> bool {
    let before = start == 0 || !is_word_byte(bytes[start - 1]);
    let after = start + len == bytes.len() || !is_word_byte(bytes[start + len]);
    before && after
}

/// First whole-token occurrence of `needle` in an already ASCII-lowercased
/// haystack. Empty or non-ASCII needles never match (false negative).
fn find_word_signal_lower(lower: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || !needle.is_ascii() {
        return None;
    }
    let needle_lower = needle.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut search_from = 0;
    while let Some(offset) = lower[search_from..].find(&needle_lower) {
        let position = search_from + offset;
        if boundary_ok(bytes, position, needle_lower.len()) {
            return Some(position);
        }
        search_from = position + 1;
        if search_from >= lower.len() {
            break;
        }
    }
    None
}

/// Last whole-token occurrence of `needle` in an already ASCII-lowercased
/// haystack.
fn find_last_word_signal_lower(lower: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || !needle.is_ascii() {
        return None;
    }
    let needle_lower = needle.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found = None;
    let mut search_from = 0;
    while let Some(offset) = lower[search_from..].find(&needle_lower) {
        let position = search_from + offset;
        if boundary_ok(bytes, position, needle_lower.len()) {
            found = Some(position);
        }
        search_from = position + 1;
        if search_from >= lower.len() {
            break;
        }
    }
    found
}

/// Byte that terminates a URL authority.
fn is_authority_end(byte: u8) -> bool {
    matches!(
        byte,
        b'/' | b'?'
            | b'#'
            | b' '
            | b'\t'
            | b'\n'
            | b'\r'
            | b'"'
            | b'\''
            | b')'
            | b']'
            | b'<'
            | b'>'
            | b'`'
    )
}

fn host_matches(host: &str, suffix: &str) -> bool {
    host == suffix
        || host
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// One `http(s)://` occurrence in an already ASCII-lowercased text: does its
/// authority match `host_suffix` and does it carry a non-root path?
fn url_at_matches(lower: &str, authority_start: usize, host_suffix: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut end = authority_start;
    while end < bytes.len() && !is_authority_end(bytes[end]) {
        end += 1;
    }
    let authority = &lower[authority_start..end];
    // Userinfo is never part of the host; the port is not either.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if !host_matches(host, host_suffix) {
        return false;
    }
    // A product URL, not a homepage: a path segment must follow the host.
    let rest = lower[end..].split_whitespace().next().unwrap_or("");
    let Some(slash) = rest.find('/') else {
        return false;
    };
    let path = &rest[slash + 1..];
    let path = path
        .split(['?', '#', '"', '\'', ')', ']', '>', '<', '`'])
        .next()
        .unwrap_or("");
    !path.is_empty()
}

/// Does an already ASCII-lowercased `text` carry a first-party product URL
/// for `host_suffix`?
fn find_product_url_lower(lower: &str, host_suffix: &str) -> bool {
    if host_suffix.is_empty() || !host_suffix.is_ascii() {
        return false;
    }
    for scheme in ["https://", "http://"] {
        let mut search_from = 0;
        while let Some(offset) = lower[search_from..].find(scheme) {
            let authority_start = search_from + offset + scheme.len();
            if url_at_matches(lower, authority_start, host_suffix) {
                return true;
            }
            search_from = authority_start;
            if search_from >= lower.len() {
                break;
            }
        }
    }
    false
}

/// Deterministically evaluate `triggers` over one text. No classifier, no
/// embedding, no model call: one ASCII-lowercase copy of `text` plus
/// whole-token scans.
pub fn evaluate_triggers(triggers: &[ToolTrigger], text: &str) -> TriggerVerdict {
    let lower = text.to_ascii_lowercase();
    // Explicit flags are strongest: the LAST occurrence wins, `on` wins a
    // same-position tie, and the verdict overrides signals in this text.
    let mut last_flag: Option<(usize, bool)> = None;
    for trigger in triggers {
        let ToolTrigger::SessionFlag { on, off } = trigger else {
            continue;
        };
        if let Some(position) = find_last_word_signal_lower(&lower, off) {
            if last_flag.is_none_or(|(seen, _)| position >= seen) {
                last_flag = Some((position, false));
            }
        }
        if let Some(position) = find_last_word_signal_lower(&lower, on) {
            if last_flag.is_none_or(|(seen, _)| position >= seen) {
                last_flag = Some((position, true));
            }
        }
    }
    if let Some((_, on)) = last_flag {
        return if on {
            TriggerVerdict::Activate
        } else {
            TriggerVerdict::Deactivate
        };
    }
    let mut decisive = false;
    for trigger in triggers {
        match trigger {
            ToolTrigger::Signal(phrase) => {
                decisive |= find_word_signal_lower(&lower, phrase).is_some();
            }
            ToolTrigger::ProductUrl { host_suffix } => {
                decisive |= find_product_url_lower(&lower, host_suffix);
            }
            // Weak signals are never sufficient, and never accumulate.
            ToolTrigger::WeakSignal(_) | ToolTrigger::SessionFlag { .. } => {}
        }
    }
    if decisive {
        TriggerVerdict::Activate
    } else {
        TriggerVerdict::None
    }
}

// --------------------------------------------------------------------------
// Exposure + activation set
// --------------------------------------------------------------------------

/// How a registered tool is exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolExposure {
    /// Historical behavior: the per-phase class policy decides (this is what
    /// `register` means).
    #[default]
    Normal,
    /// Invisible until activated, then exposed only in `phases`.
    Lazy {
        phases: PhaseMask,
        triggers: Vec<ToolTrigger>,
    },
}

impl ToolExposure {
    pub fn lazy(phases: PhaseMask, triggers: Vec<ToolTrigger>) -> Self {
        Self::Lazy { phases, triggers }
    }

    pub fn is_lazy(&self) -> bool {
        matches!(self, Self::Lazy { .. })
    }

    pub fn phase_mask(&self) -> Option<PhaseMask> {
        match self {
            Self::Normal => None,
            Self::Lazy { phases, .. } => Some(*phases),
        }
    }

    pub fn triggers(&self) -> &[ToolTrigger] {
        match self {
            Self::Normal => &[],
            Self::Lazy { triggers, .. } => triggers,
        }
    }
}

/// The set of currently activated lazy tools. Deterministic by construction
/// (`BTreeSet`): iteration order is name order, independent of registration
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolActivationSet {
    active: BTreeSet<String>,
}

impl ToolActivationSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self, tool: &str) -> bool {
        self.active.contains(tool)
    }

    /// Activate `tool`; returns whether it was newly activated.
    pub fn activate(&mut self, tool: impl Into<String>) -> bool {
        self.active.insert(tool.into())
    }

    /// Deactivate `tool`; returns whether it was active.
    pub fn deactivate(&mut self, tool: &str) -> bool {
        self.active.remove(tool)
    }

    pub fn names(&self) -> Vec<String> {
        self.active.iter().cloned().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.active.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn clear(&mut self) {
        self.active.clear();
    }

    /// Fold one text into the set for ONE tool's triggers. Activation is
    /// sticky; only an explicit `off` flag (or [`Self::deactivate`]) turns a
    /// tool off.
    pub fn observe(&mut self, tool: &str, triggers: &[ToolTrigger], text: &str) -> TriggerVerdict {
        match evaluate_triggers(triggers, text) {
            TriggerVerdict::Activate => {
                self.activate(tool);
                TriggerVerdict::Activate
            }
            TriggerVerdict::Deactivate => {
                self.deactivate(tool);
                TriggerVerdict::Deactivate
            }
            TriggerVerdict::None => TriggerVerdict::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquire() -> Vec<ToolTrigger> {
        acquire_source_triggers()
    }

    #[test]
    fn acquire_strong_signals_activate() {
        for text in [
            "check 1688 for this connector",
            "see 1688.com/offer/1",
            "search Alibaba for a reel",
            "Alibaba.com listing",
            "LCSC has it in stock",
            "Mouser price break",
            "DigiKey quote",
            "we need sourcing help",
            "please source this part",
            "BOM pricing for the board",
            "component pricing question",
            "buy 5,000 of these",
            "find manufacturers for the housing",
            "quote this BOM",
        ] {
            assert_eq!(
                evaluate_triggers(&acquire(), text),
                TriggerVerdict::Activate,
                "{text:?} must activate"
            );
        }
    }

    #[test]
    fn supplier_alone_never_activates() {
        for text in [
            "the supplier trait in Rust is interesting",
            "Supplier abstraction and dependency injection",
            "impl Supplier for Foo",
            "suppliers are hard to test",
            "supplier.rs should not exist",
            "my supplier of coffee",
        ] {
            assert_eq!(
                evaluate_triggers(&acquire(), text),
                TriggerVerdict::None,
                "{text:?} must NOT activate"
            );
        }
        // A weak signal alone is inert even when repeated.
        assert_eq!(
            evaluate_triggers(&acquire(), "supplier supplier supplier"),
            TriggerVerdict::None
        );
    }

    #[test]
    fn weak_signal_with_a_decisive_cosignal_activates_via_the_decisive_one() {
        assert_eq!(
            evaluate_triggers(&acquire(), "supplier sourcing event"),
            TriggerVerdict::Activate
        );
        assert_eq!(
            evaluate_triggers(
                &[
                    ToolTrigger::weak_signal("frob"),
                    ToolTrigger::signal("widget")
                ],
                "frob the widget"
            ),
            TriggerVerdict::Activate
        );
        assert_eq!(
            evaluate_triggers(
                &[
                    ToolTrigger::weak_signal("frob"),
                    ToolTrigger::weak_signal("quux")
                ],
                "frob the quux"
            ),
            TriggerVerdict::None,
            "weak signals never accumulate into a verdict"
        );
    }

    #[test]
    fn whole_token_boundaries_reject_part_numbers_and_suffixes() {
        for text in [
            "TPS1688 is a regulator",
            "21688 units",
            "1688x",
            "x1688",
            "resourcing the team",
            "AlibabaCloud SDK",
            "MouserConfig::load()",
            "DigiKeys",
            "unsourcing",
        ] {
            assert_eq!(
                evaluate_triggers(&acquire(), text),
                TriggerVerdict::None,
                "{text:?} must NOT activate (whole-token match)"
            );
        }
        // Punctuation and whitespace are boundaries.
        assert_eq!(
            evaluate_triggers(&acquire(), "quotes from (Mouser)."),
            TriggerVerdict::Activate
        );
    }

    #[test]
    fn signals_are_ascii_case_insensitive() {
        for text in [
            "1688.COM",
            "ALIBABA listing",
            "mouser",
            "DIGIKEY",
            "bom PRICING",
            "Component Pricing",
            "BUY 5,000",
            "Quote This Bom",
            "SOURCE THIS PART",
        ] {
            assert_eq!(
                evaluate_triggers(&acquire(), text),
                TriggerVerdict::Activate,
                "{text:?} must activate case-insensitively"
            );
        }
    }

    #[test]
    fn non_ascii_and_fullwidth_forms_do_not_match() {
        // A Chinese procurement sentence around the ASCII signal still
        // matches (non-ASCII neighbours are non-word bytes) ...
        assert_eq!(
            evaluate_triggers(&acquire(), "供应商 1688 采购"),
            TriggerVerdict::Activate
        );
        // ... but a non-ASCII trigger never matches (false negative), and
        // full-width digits are not the ASCII signal.
        assert_eq!(
            evaluate_triggers(&[ToolTrigger::signal("零件")], "零件"),
            TriggerVerdict::None
        );
        assert_eq!(
            evaluate_triggers(&acquire(), "１６８８ supplier"),
            TriggerVerdict::None
        );
    }

    #[test]
    fn product_url_requires_first_party_authority_and_non_root_path() {
        let triggers = vec![ToolTrigger::product_url("example-market.com")];
        for text in [
            "https://detail.example-market.com/offer/123.html",
            "http://example-market.com/p/9",
            "see [part](https://detail.example-market.com/offer/1.html) please",
            "https://user:pass@detail.example-market.com:8443/offer/1",
            "HTTPS://DETAIL.EXAMPLE-MARKET.COM/offer/1",
        ] {
            assert_eq!(
                evaluate_triggers(&triggers, text),
                TriggerVerdict::Activate,
                "{text:?} must activate"
            );
        }
        for text in [
            "https://example-market.com/",
            "https://example-market.com",
            "https://evil.com/detail.example-market.com/offer/1",
            "https://example-market.com.evil.com/offer/1",
            "https://notexample-market.com/offer/1",
            "example-market.com/offer/1",
            "https://example-market.com?x=1",
        ] {
            assert_eq!(
                evaluate_triggers(&triggers, text),
                TriggerVerdict::None,
                "{text:?} must NOT activate"
            );
        }
    }

    #[test]
    fn session_flag_on_off_and_precedence() {
        let t = acquire();
        assert_eq!(
            evaluate_triggers(&t, "/source on"),
            TriggerVerdict::Activate
        );
        assert_eq!(
            evaluate_triggers(&t, "please /source on for this task"),
            TriggerVerdict::Activate
        );
        assert_eq!(
            evaluate_triggers(&t, "/source off"),
            TriggerVerdict::Deactivate
        );
        // The LAST flag wins ...
        assert_eq!(
            evaluate_triggers(&t, "/source on then /source off"),
            TriggerVerdict::Deactivate
        );
        assert_eq!(
            evaluate_triggers(&t, "/source off then /source on"),
            TriggerVerdict::Activate
        );
        // ... and a flag overrides signals in the same text.
        assert_eq!(
            evaluate_triggers(&t, "/source off, forget the 1688 quote"),
            TriggerVerdict::Deactivate
        );
        assert_eq!(
            evaluate_triggers(&t, "no 1688 here /source on"),
            TriggerVerdict::Activate
        );
        // Not a token: embedded in a path or with a broken literal.
        assert_eq!(evaluate_triggers(&t, "see/source on"), TriggerVerdict::None);
        assert_eq!(evaluate_triggers(&t, "/source\non"), TriggerVerdict::None);
        assert_eq!(evaluate_triggers(&t, "/source  on"), TriggerVerdict::None);
        assert_eq!(
            evaluate_triggers(&t, "/SOURCE OFF"),
            TriggerVerdict::Deactivate
        );
    }

    #[test]
    fn empty_texts_and_empty_trigger_lists_are_inert() {
        assert_eq!(evaluate_triggers(&acquire(), ""), TriggerVerdict::None);
        assert_eq!(
            evaluate_triggers(&acquire(), "   \n\t"),
            TriggerVerdict::None
        );
        assert_eq!(evaluate_triggers(&[], "1688 Mouser"), TriggerVerdict::None);
    }

    #[test]
    fn a_signal_at_the_far_end_of_a_large_text_still_matches() {
        let mut text = "x ".repeat(200_000);
        text.push_str("1688");
        assert_eq!(
            evaluate_triggers(&acquire(), &text),
            TriggerVerdict::Activate
        );
    }

    #[test]
    fn phase_mask_is_a_deterministic_bitmask() {
        assert!(PhaseMask::NONE.is_empty());
        assert_eq!(PhaseMask::NONE.phases(), Vec::new());
        assert_eq!(PhaseMask::ALL.phases(), RouterPhase::ALL.to_vec());
        assert_eq!(PhaseMask::ALL.bits(), (1u16 << PHASE_COUNT) - 1);

        let plan_plus = PhaseMask::NONE
            .with(RouterPhase::Plan)
            .with(RouterPhase::Debug);
        assert!(plan_plus.contains(RouterPhase::Plan));
        assert!(plan_plus.contains(RouterPhase::Debug));
        assert!(!plan_plus.contains(RouterPhase::Explore));
        assert_eq!(
            plan_plus.phases(),
            vec![RouterPhase::Plan, RouterPhase::Debug],
            "phase order follows RouterPhase::ALL"
        );
        assert_eq!(
            plan_plus.without(RouterPhase::Plan),
            PhaseMask::of([RouterPhase::Debug])
        );
        assert_eq!(
            PhaseMask::of([RouterPhase::Plan]) | PhaseMask::of([RouterPhase::Explore]),
            PhaseMask::of([RouterPhase::Plan, RouterPhase::Explore])
        );
        // Unknown high bits are masked off, never a panic.
        assert_eq!(PhaseMask::from_bits(u16::MAX), PhaseMask::ALL);
        assert_eq!(
            PhaseMask::from_bits(!PhaseMask::ALL.bits()),
            PhaseMask::NONE
        );
        assert_eq!(
            PhaseMask::from_bits(PhaseMask::of([RouterPhase::Embed]).bits()).phases(),
            vec![RouterPhase::Embed]
        );
    }

    #[test]
    fn acquire_phase_mask_follows_spec_4() {
        let mask = acquire_source_phases();
        for phase in [
            RouterPhase::Plan,
            RouterPhase::Explore,
            RouterPhase::Retrieve,
            RouterPhase::Implement,
            RouterPhase::Debug,
        ] {
            assert!(mask.contains(phase), "{phase:?} must be allowed");
        }
        for phase in [
            RouterPhase::Review,
            RouterPhase::TestAnalysis,
            RouterPhase::Summarize,
            RouterPhase::Compact,
            RouterPhase::Title,
            RouterPhase::Embed,
        ] {
            assert!(!mask.contains(phase), "{phase:?} must not be allowed");
        }
    }

    #[test]
    fn activation_set_is_ordered_and_sticky() {
        let mut set = ToolActivationSet::new();
        assert!(set.is_empty());
        assert!(!set.is_active("source_market"));
        assert!(set.activate("source_market"));
        assert!(!set.activate("source_market"), "already active");
        assert!(set.activate("alpha"));
        assert_eq!(
            set.names(),
            vec!["alpha", "source_market"],
            "BTreeSet order"
        );
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec!["alpha", "source_market"]
        );
        assert_eq!(set.len(), 2);
        assert!(set.deactivate("alpha"));
        assert!(!set.deactivate("alpha"), "already inactive");
        assert!(!set.deactivate("never_registered"));
        set.clear();
        assert!(set.is_empty());
    }

    #[test]
    fn observe_folds_activation_and_deactivation_for_one_tool() {
        let t = acquire();
        let mut set = ToolActivationSet::new();
        assert_eq!(
            set.observe("source_market", &t, "fix the parser"),
            TriggerVerdict::None
        );
        assert!(!set.is_active("source_market"));
        assert_eq!(
            set.observe("source_market", &t, "the supplier trait"),
            TriggerVerdict::None
        );
        assert!(!set.is_active("source_market"));
        assert_eq!(
            set.observe("source_market", &t, "quote this BOM"),
            TriggerVerdict::Activate
        );
        assert!(set.is_active("source_market"));
        // Sticky: an unrelated later text does not deactivate.
        assert_eq!(
            set.observe("source_market", &t, "fix the parser"),
            TriggerVerdict::None
        );
        assert!(set.is_active("source_market"));
        assert_eq!(
            set.observe("source_market", &t, "/source off"),
            TriggerVerdict::Deactivate
        );
        assert!(!set.is_active("source_market"));
    }

    #[test]
    fn exposure_accessors() {
        let normal = ToolExposure::Normal;
        assert!(!normal.is_lazy());
        assert_eq!(normal.phase_mask(), None);
        assert!(normal.triggers().is_empty());

        let lazy = ToolExposure::lazy(PhaseMask::of([RouterPhase::Plan]), acquire());
        assert!(lazy.is_lazy());
        assert_eq!(lazy.phase_mask(), Some(PhaseMask::of([RouterPhase::Plan])));
        assert_eq!(lazy.triggers().len(), acquire().len());
        assert!(lazy.triggers().iter().any(ToolTrigger::is_decisive));
    }
}
