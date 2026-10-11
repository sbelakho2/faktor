//! Live-canary registry and evidence discipline (marketplace audit item 8).
//!
//! The fixture-based suites replay sanitized vendor payloads offline; this
//! module is the registry and shared vocabulary for the **opt-in** live
//! canaries that exercise a connector against the real vendor only when an
//! operator explicitly provides that source's credentials. Nothing here
//! performs I/O: the canary tests (`tests/live_canary.rs`) read the
//! environment, and the daemon's doctor reports the same per-source
//! configured/absent status from [`canary_statuses_from_env`] so the two can
//! never drift.
//!
//! Non-negotiables:
//!
//! * **Opt-in only.** A source is [`CanaryStatus::Configured`] only when
//!   every one of its `required_env` variables is set and non-empty. An
//!   absent source makes no request; there is no default credential.
//! * **Never in CI by default.** The canary tests are `#[ignore]`-gated and
//!   assigned to the key-gated `live-paid` lane; the lane itself is a
//!   recorded skip until an operator provides credentials.
//! * **Structural validity, never golden constants.** Live vendor data
//!   changes; the canaries assert shape invariants ([`structural_discovery_check`],
//!   [`structural_offer_check`]) and never compare against captured vendor
//!   values.
//! * **Evidence digests.** Each canary records a BLAKE3 digest
//!   ([`evidence_digest`]) over the *scrubbed* structural summary of what it
//!   observed, plus a bounded machine-readable [`evidence_line`].

use faktor_commerce::{CommercialOffer, SourceId};

use crate::contract::Discovery;

/// The evidence line schema id.
pub const EVIDENCE_SCHEMA: &str = "faktor-live-canary/v1";

/// Hard bound on one evidence line (bytes).
pub const MAX_EVIDENCE_LINE_BYTES: usize = 2048;

/// Hard bound on observed labels rendered into one evidence line.
pub const MAX_EVIDENCE_LABELS: usize = 16;

/// Hard bound on one observed label (bytes).
pub const MAX_EVIDENCE_LABEL_BYTES: usize = 96;

/// The mechanism a live canary exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveCanaryMechanism {
    /// A documented first-party API through the injected transport.
    OfficialApi,
    /// A stored logged-in browser profile through the browser authority.
    BrowserProfile,
}

impl LiveCanaryMechanism {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfficialApi => "official-api",
            Self::BrowserProfile => "browser-profile",
        }
    }
}

/// One live-canary source: the connector id, the mechanism the canary
/// exercises and the environment variables that must be explicitly provided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveCanarySource {
    /// The connector source id.
    pub source: &'static str,
    /// The mechanism the canary exercises.
    pub mechanism: LiveCanaryMechanism,
    /// The environment variables that must ALL be set and non-empty.
    pub required_env: &'static [&'static str],
}

/// The live-canary sources, in deterministic order. The environment variable
/// names are the contract between an operator, the canary tests and the
/// doctor line; they are names only, never values.
pub const LIVE_CANARY_SOURCES: [LiveCanarySource; 5] = [
    LiveCanarySource {
        source: "mouser",
        mechanism: LiveCanaryMechanism::OfficialApi,
        required_env: &["FAKTOR_LIVE_MOUSER_KEY"],
    },
    LiveCanarySource {
        source: "digikey",
        mechanism: LiveCanaryMechanism::OfficialApi,
        required_env: &[
            "FAKTOR_LIVE_DIGIKEY_CLIENT_ID",
            "FAKTOR_LIVE_DIGIKEY_CLIENT_SECRET",
        ],
    },
    LiveCanarySource {
        source: "lcsc",
        mechanism: LiveCanaryMechanism::OfficialApi,
        required_env: &["FAKTOR_LIVE_LCSC_KEY"],
    },
    LiveCanarySource {
        source: "1688",
        mechanism: LiveCanaryMechanism::BrowserProfile,
        required_env: &["FAKTOR_LIVE_1688_PROFILE", "FAKTOR_LIVE_DATA_DIR"],
    },
    LiveCanarySource {
        source: "alibaba",
        mechanism: LiveCanaryMechanism::BrowserProfile,
        required_env: &["FAKTOR_LIVE_ALIBABA_PROFILE", "FAKTOR_LIVE_DATA_DIR"],
    },
];

/// The live-canary status of one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryStatus {
    /// Every required environment variable is set and non-empty.
    Configured,
    /// At least one required environment variable is absent or empty.
    Absent,
}

impl CanaryStatus {
    /// The stable label (`configured` / `absent`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Absent => "absent",
        }
    }
}

/// The status of one source under an injected environment lookup.
pub fn canary_status(
    source: &LiveCanarySource,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> CanaryStatus {
    let configured = source
        .required_env
        .iter()
        .all(|name| lookup(name).is_some_and(|value| !value.trim().is_empty()));
    if configured {
        CanaryStatus::Configured
    } else {
        CanaryStatus::Absent
    }
}

/// The status of every source under an injected environment lookup. The
/// canary tests and the doctor share this function so their notions of
/// "configured" can never drift.
pub fn canary_statuses(
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<(&'static str, CanaryStatus)> {
    LIVE_CANARY_SOURCES
        .iter()
        .map(|source| (source.source, canary_status(source, lookup)))
        .collect()
}

/// The status of every source under the process environment.
pub fn canary_statuses_from_env() -> Vec<(&'static str, CanaryStatus)> {
    canary_statuses(&|name| std::env::var(name).ok())
}

/// The registry entry for one source id, when it is a known canary source.
pub fn canary_source(source: &str) -> Option<&'static LiveCanarySource> {
    LIVE_CANARY_SOURCES
        .iter()
        .find(|entry| entry.source == source)
}

/// Structural validity of a live discovery set: every invariant a live
/// payload must satisfy regardless of the vendor's current data — never a
/// comparison against captured vendor values.
pub fn structural_discovery_check(
    source: &SourceId,
    discoveries: &[Discovery],
) -> Result<(), String> {
    if discoveries.is_empty() {
        return Err(format!("{source}: live discovery returned zero listings"));
    }
    for (index, discovery) in discoveries.iter().enumerate() {
        if &discovery.source != source {
            return Err(format!(
                "{source}: discovery {index} carries source {}",
                discovery.source
            ));
        }
        if discovery.title.as_str().trim().is_empty() {
            return Err(format!("{source}: discovery {index} has an empty title"));
        }
        discovery
            .identity
            .validate()
            .map_err(|error| format!("{source}: discovery {index} identity invalid: {error}"))?;
        if discovery.observed_at_ms == 0 {
            return Err(format!(
                "{source}: discovery {index} has no observation timestamp"
            ));
        }
        if let Some(hint) = &discovery.price_hint {
            if hint.micros < 0 {
                return Err(format!(
                    "{source}: discovery {index} price hint is negative"
                ));
            }
        }
    }
    Ok(())
}

/// Structural validity of a live offer: the domain's own cross-field
/// invariants plus source/currency consistency, never a value comparison.
pub fn structural_offer_check(source: &SourceId, offer: &CommercialOffer) -> Result<(), String> {
    if &offer.source != source {
        return Err(format!(
            "{source}: live offer carries source {}",
            offer.source
        ));
    }
    if offer.title.as_str().trim().is_empty() {
        return Err(format!("{source}: live offer has an empty title"));
    }
    if offer.observed_at_ms == 0 {
        return Err(format!("{source}: live offer has no observation timestamp"));
    }
    offer
        .validate()
        .map_err(|error| format!("{source}: live offer violates its own invariants: {error}"))?;
    for (index, tier) in offer.price_breaks.iter().enumerate() {
        if tier.unit_price.currency != offer.currency {
            return Err(format!(
                "{source}: live offer tier {index} currency {} != offer currency {}",
                tier.unit_price.currency, offer.currency
            ));
        }
        if tier.unit_price.micros < 0 {
            return Err(format!("{source}: live offer tier {index} is negative"));
        }
    }
    Ok(())
}

/// A BLAKE3 evidence digest over the SCRUBBED structural summary of one live
/// observation. The caller hashes the secret-scrubbed text only; a digest
/// never becomes a secret channel.
pub fn evidence_digest(scrubbed_evidence: &str) -> String {
    format!(
        "blake3:{}",
        blake3::hash(scrubbed_evidence.as_bytes()).to_hex()
    )
}

/// One bounded machine-readable evidence line. Observed labels are bounded
/// in count and length, and the rendered line is clipped at
/// [`MAX_EVIDENCE_LINE_BYTES`] on a UTF-8 boundary.
pub fn evidence_line(
    source: &str,
    mechanism: &str,
    operation: &str,
    outcome: &str,
    digest: &str,
    observed: &[String],
) -> String {
    let labels: Vec<&str> = observed
        .iter()
        .take(MAX_EVIDENCE_LABELS)
        .map(|label| clip(label, MAX_EVIDENCE_LABEL_BYTES))
        .collect();
    let line = format!(
        "{EVIDENCE_SCHEMA} source={source} mechanism={mechanism} operation={operation} \
         outcome={outcome} digest={digest} observed={}",
        labels.join(",")
    );
    clip(line.as_str(), MAX_EVIDENCE_LINE_BYTES).to_string()
}

fn clip(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let end = text
        .char_indices()
        .take_while(|(index, _)| *index < max_bytes)
        .last()
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    &text[..end]
}

#[cfg(test)]
mod tests {
    use faktor_commerce::offer::OfferProvenance;
    use faktor_commerce::{Money, ObservationOrigin, StockState, Text};

    use super::*;

    fn source() -> SourceId {
        SourceId::new("mouser").expect("source")
    }

    fn identity() -> faktor_commerce::ProductIdentity {
        let mut identity = faktor_commerce::ProductIdentity::empty();
        identity.manufacturer_part_number = Some(Text::new("TPS5430DDAR").expect("mpn"));
        identity
    }

    fn discovery() -> Discovery {
        Discovery {
            source: source(),
            identity: identity(),
            title: Text::<512>::new("TPS5430DDAR").expect("title"),
            url: None,
            price_hint: None,
            price_visibility: faktor_commerce::PriceVisibility::Unknown,
            stock_hint: None,
            freshness: faktor_commerce::Freshness::Live,
            origin: ObservationOrigin::OfficialApi,
            documented_max_staleness_ms: None,
            observed_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_source_is_configured_only_when_every_env_name_is_set_and_non_empty() {
        let mouser = canary_source("mouser").expect("mouser");
        assert_eq!(
            canary_status(mouser, &|_| None),
            CanaryStatus::Absent,
            "no environment => absent"
        );
        assert_eq!(
            canary_status(mouser, &|_| Some("  ".to_string())),
            CanaryStatus::Absent,
            "whitespace-only credentials are absent"
        );
        assert_eq!(
            canary_status(mouser, &|_| Some("key".to_string())),
            CanaryStatus::Configured
        );

        let digikey = canary_source("digikey").expect("digikey");
        let lookup_all_but_one = |name: &str| {
            if name == "FAKTOR_LIVE_DIGIKEY_CLIENT_SECRET" {
                None
            } else {
                Some("value".to_string())
            }
        };
        assert_eq!(
            canary_status(digikey, &lookup_all_but_one),
            CanaryStatus::Absent,
            "an OAuth pair requires BOTH names"
        );
    }

    #[test]
    fn statuses_cover_every_source_in_deterministic_order() {
        let statuses = canary_statuses(&|_| Some("set".to_string()));
        let names: Vec<&str> = statuses.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["mouser", "digikey", "lcsc", "1688", "alibaba"]);
        assert!(statuses
            .iter()
            .all(|(_, status)| *status == CanaryStatus::Configured));
        let absent = canary_statuses_from_env();
        assert_eq!(absent.len(), LIVE_CANARY_SOURCES.len());
    }

    #[test]
    fn structural_discovery_check_rejects_corrupt_shapes_and_accepts_a_valid_one() {
        let good = discovery();
        assert!(structural_discovery_check(&source(), std::slice::from_ref(&good)).is_ok());
        assert!(structural_discovery_check(&source(), &[]).is_err());

        let mut wrong_source = good.clone();
        wrong_source.source = SourceId::new("lcsc").expect("source");
        assert!(structural_discovery_check(&source(), &[wrong_source]).is_err());

        let mut no_time = good.clone();
        no_time.observed_at_ms = 0;
        assert!(structural_discovery_check(&source(), &[no_time]).is_err());

        let mut negative_hint = good;
        negative_hint.price_hint = Some(Money::from_micros(faktor_commerce::Currency::USD, -1));
        assert!(structural_discovery_check(&source(), &[negative_hint]).is_err());
    }

    #[test]
    fn structural_offer_check_validates_through_the_domain_invariants() {
        let offer = faktor_commerce::CommercialOffer {
            source: source(),
            identity: identity(),
            title: Text::<512>::new("TPS5430DDAR").expect("title"),
            description: None,
            currency: faktor_commerce::Currency::USD,
            price_breaks: Vec::new(),
            variants: Vec::new(),
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            lead_time: None,
            packaging: Vec::new(),
            supplier: None,
            manufacturer: None,
            provenance: OfferProvenance {
                origin: ObservationOrigin::OfficialApi,
                source: source(),
                extractor_version: None,
                connector_version: None,
                normalization_version: None,
                content_digest: None,
                account_scope: None,
                locale: None,
                market: None,
                source_confidence_bp: None,
            },
            observed_at_ms: 1_700_000_000_000,
            price_visibility: faktor_commerce::PriceVisibility::Public,
            lifecycle: faktor_commerce::LifecycleStatus::Active,
        };
        assert!(structural_offer_check(&source(), &offer).is_ok());

        let mut wrong_source = offer.clone();
        wrong_source.source = SourceId::new("lcsc").expect("source");
        assert!(structural_offer_check(&source(), &wrong_source).is_err());
    }

    #[test]
    fn evidence_digests_are_prefixed_stable_and_scrubbed_input_only() {
        let first = evidence_digest("source=mouser observed=3");
        let second = evidence_digest("source=mouser observed=3");
        assert_eq!(first, second);
        assert!(first.starts_with("blake3:"));
        assert_eq!(first.len(), "blake3:".len() + 64);
        assert_ne!(first, evidence_digest("source=mouser observed=4"));
    }

    #[test]
    fn evidence_lines_are_bounded_in_labels_and_bytes() {
        let labels: Vec<String> = (0..64).map(|index| format!("label-{index}")).collect();
        let line = evidence_line(
            "mouser",
            "official-api",
            "discover",
            "ok",
            "blake3:x",
            &labels,
        );
        assert!(line.starts_with(EVIDENCE_SCHEMA));
        assert!(line.contains("outcome=ok"));
        assert!(
            line.len() <= MAX_EVIDENCE_LINE_BYTES,
            "evidence line must stay bounded"
        );
        assert_eq!(line.matches("label-").count(), MAX_EVIDENCE_LABELS);

        let huge = vec!["x".repeat(MAX_EVIDENCE_LABEL_BYTES * 4)];
        let line = evidence_line(
            "mouser",
            "official-api",
            "discover",
            "ok",
            "blake3:x",
            &huge,
        );
        assert!(line.len() <= MAX_EVIDENCE_LINE_BYTES);
    }
}
