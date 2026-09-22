//! The browser-acquisition seam: bounded captures, challenge detection and
//! the connector-maintained first-party interception policy (spec §9, §10).
//!
//! Connectors never launch Chromium. The acquisition runtime injects a
//! [`BrowserExtraction`] into [`AcquireCtx`](crate::context::AcquireCtx); the
//! implementation owns the profile boundary, the egress broker and the
//! capture bounds. What crosses this seam is a [`CaptureBundle`]: at most
//! [`MAX_CAPTURES`] bounded payloads (browser-network JSON, embedded
//! application state, structural markup, rendered text), an optional typed
//! [`Challenge`], and the stable profile it was captured under.
//!
//! Two rules are structural here:
//!
//! * **First-party only.** A connector filters every bundle through its own
//!   [`FirstPartyPolicy`]; third-party payloads are dropped before any
//!   extractor sees them, so an ad/tracker JSON blob can never become a
//!   price.
//! * **A challenge stops the profile.** A detected login form, CAPTCHA
//!   container, security slider, access-denied interstitial or rate-limit
//!   page becomes a typed state; the connector records the profile stop and
//!   surfaces [`SourceError::VerificationRequired`] (or the matching typed
//!   error) instead of retrying — no bypass subsystem exists.

use std::sync::Arc;

use async_trait::async_trait;
use faktor_commerce::connector::ProfileIdentity;
use faktor_commerce::error::VerificationKind;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{SourceError, SourceId};

use crate::context::AcquireCtx;
use crate::contract::extract::Strategy;
use crate::quota::MINUTE_MS;

/// Hard bound on captured payloads per bundle.
pub const MAX_CAPTURES: usize = 32;
/// Hard bound on one captured payload body.
pub const MAX_CAPTURE_BYTES: usize = 512 * 1024;
/// Hard bound on the total captured bytes per bundle.
pub const MAX_CAPTURE_TOTAL_BYTES: usize = 2 * 1024 * 1024;

/// What kind of capture a payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CaptureKind {
    /// A captured XHR/fetch JSON response.
    NetworkJson,
    /// Embedded application state (`__INIT_DATA__`-style script blocks).
    EmbeddedState,
    /// Structural markup (the page's DOM source).
    StructuredMarkup,
    /// Rendered text (last resort).
    RenderedText,
}

impl CaptureKind {
    /// The extraction strategy this capture feeds.
    pub const fn strategy(self) -> Strategy {
        match self {
            Self::NetworkJson => Strategy::NetworkJson,
            Self::EmbeddedState => Strategy::EmbeddedState,
            Self::StructuredMarkup => Strategy::StructuralDom,
            Self::RenderedText => Strategy::RenderedText,
        }
    }
}

/// One bounded captured payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedPayload {
    /// What the payload is.
    pub kind: CaptureKind,
    /// The URL it was captured from.
    pub url: CanonicalUrl,
    /// The content type, when stated.
    pub content_type: Option<Text<64>>,
    /// The bounded body.
    body: Vec<u8>,
    /// When it was captured.
    pub observed_at_ms: u64,
}

impl CapturedPayload {
    /// Validate and construct.
    pub fn new(
        kind: CaptureKind,
        url: CanonicalUrl,
        content_type: Option<Text<64>>,
        body: Vec<u8>,
        observed_at_ms: u64,
    ) -> Result<Self, SourceError> {
        if body.len() > MAX_CAPTURE_BYTES {
            return Err(SourceError::ResponseTooLarge);
        }
        Ok(Self {
            kind,
            url,
            content_type,
            body,
            observed_at_ms,
        })
    }

    /// The lossy UTF-8 body.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    /// The body length.
    pub fn len(&self) -> usize {
        self.body.len()
    }

    /// True when the body is empty.
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
}

/// A detected human-verification or blocking state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChallengeKind {
    /// A login form / sign-in wall.
    LoginForm,
    /// A verification interstitial ("verify you are human").
    VerificationInterstitial,
    /// A CAPTCHA container.
    Captcha,
    /// A security slider.
    SecuritySlider,
    /// An access-denied page.
    AccessDenied,
    /// A rate-limit / traffic page.
    RateLimitPage,
}

impl ChallengeKind {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LoginForm => "login_form",
            Self::VerificationInterstitial => "verification_interstitial",
            Self::Captcha => "captcha",
            Self::SecuritySlider => "security_slider",
            Self::AccessDenied => "access_denied",
            Self::RateLimitPage => "rate_limit_page",
        }
    }

    /// The domain verification kind.
    pub const fn verification_kind(self) -> VerificationKind {
        match self {
            Self::Captcha => VerificationKind::Captcha,
            Self::LoginForm => VerificationKind::Login,
            Self::VerificationInterstitial | Self::SecuritySlider => VerificationKind::Manual,
            Self::AccessDenied | Self::RateLimitPage => VerificationKind::Unknown,
        }
    }

    /// The typed error the connector must surface.
    pub const fn to_source_error(self) -> SourceError {
        match self {
            Self::AccessDenied => SourceError::AuthenticationRequired,
            Self::RateLimitPage => SourceError::RateLimited {
                retry_after_ms: MINUTE_MS,
            },
            other => SourceError::VerificationRequired {
                kind: other.verification_kind(),
            },
        }
    }
}

/// One detected challenge with bounded evidence (a matched marker, never a
/// payload excerpt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    /// The challenge kind.
    pub kind: ChallengeKind,
    /// The marker that matched.
    pub evidence: Text<128>,
}

impl Challenge {
    /// Construct one challenge.
    pub fn new(kind: ChallengeKind, evidence: &str) -> Self {
        let bounded = crate::normalize::sanitize_excerpt(evidence, 96);
        Self {
            kind,
            evidence: Text::<128>::new(&bounded)
                .unwrap_or_else(|_| Text::<128>::new("challenge").expect("literal")),
        }
    }

    /// The typed error this challenge must produce.
    pub const fn to_source_error(&self) -> SourceError {
        self.kind.to_source_error()
    }
}

/// Per-site challenge markers (curated; deterministic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChallengeMarkers {
    /// Login/sign-in markers.
    pub login: &'static [&'static str],
    /// CAPTCHA markers.
    pub captcha: &'static [&'static str],
    /// Security-slider markers.
    pub slider: &'static [&'static str],
    /// Verification-interstitial markers.
    pub interstitial: &'static [&'static str],
    /// Access-denied markers.
    pub access_denied: &'static [&'static str],
    /// Rate-limit markers.
    pub rate_limit: &'static [&'static str],
}

impl ChallengeMarkers {
    /// Detect the strongest matching challenge in one text blob. The order is
    /// deliberate: blocking states first, verification states second.
    pub fn detect(&self, text: &str) -> Option<Challenge> {
        let haystack = text.to_lowercase();
        for (kind, markers) in [
            (ChallengeKind::RateLimitPage, self.rate_limit),
            (ChallengeKind::AccessDenied, self.access_denied),
            (ChallengeKind::Captcha, self.captcha),
            (ChallengeKind::SecuritySlider, self.slider),
            (ChallengeKind::LoginForm, self.login),
            (ChallengeKind::VerificationInterstitial, self.interstitial),
        ] {
            for marker in markers {
                if haystack.contains(&marker.to_lowercase()) {
                    return Some(Challenge::new(kind, marker));
                }
            }
        }
        None
    }
}

/// The first-party destination policy a connector maintains for its own
/// captures. Suffix matching only: `1688.com` allows `detail.1688.com` and
/// `s.1688.com`, never `1688.com.evil.test`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstPartyPolicy {
    suffixes: &'static [&'static str],
}

impl FirstPartyPolicy {
    /// Construct one policy from host suffixes.
    pub const fn new(suffixes: &'static [&'static str]) -> Self {
        Self { suffixes }
    }

    /// True when the URL's host is first-party.
    pub fn allows(&self, url: &CanonicalUrl) -> bool {
        let host = url.host().to_ascii_lowercase();
        self.suffixes.iter().any(|suffix| {
            let suffix = suffix.to_ascii_lowercase();
            host == suffix || host.ends_with(&format!(".{suffix}"))
        })
    }
}

/// A bounded capture bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureBundle {
    /// The page URL that was captured.
    pub url: CanonicalUrl,
    /// The captured payloads.
    pub payloads: Vec<CapturedPayload>,
    /// A detected challenge, when the page stopped the profile.
    pub challenge: Option<Challenge>,
    /// When the capture was made.
    pub observed_at_ms: u64,
}

impl CaptureBundle {
    /// Validate and construct.
    pub fn new(
        url: CanonicalUrl,
        payloads: Vec<CapturedPayload>,
        challenge: Option<Challenge>,
        observed_at_ms: u64,
    ) -> Result<Self, SourceError> {
        let total: usize = payloads.iter().map(CapturedPayload::len).sum();
        if payloads.len() > MAX_CAPTURES || total > MAX_CAPTURE_TOTAL_BYTES {
            return Err(SourceError::ResponseTooLarge);
        }
        Ok(Self {
            url,
            payloads,
            challenge,
            observed_at_ms,
        })
    }

    /// True when this bundle stops the profile (a challenge was detected).
    pub const fn stops_profile(&self) -> bool {
        self.challenge.is_some()
    }

    /// The typed error this bundle must surface, when it stopped the profile.
    pub fn challenge_error(&self) -> Option<SourceError> {
        self.challenge.as_ref().map(Challenge::to_source_error)
    }

    /// The payloads of one kind, in captured order.
    pub fn payloads_of(&self, kind: CaptureKind) -> Vec<&CapturedPayload> {
        self.payloads
            .iter()
            .filter(|payload| payload.kind == kind)
            .collect()
    }

    /// Drop every payload that is not first-party. Returns the number of
    /// dropped payloads so the caller can record the interception.
    pub fn retain_first_party(&mut self, policy: &FirstPartyPolicy) -> usize {
        let before = self.payloads.len();
        self.payloads.retain(|payload| policy.allows(&payload.url));
        before - self.payloads.len()
    }
}

/// The injected browser-acquisition authority.
#[async_trait]
pub trait BrowserExtraction: Send + Sync {
    /// Capture one URL under the stable `profile`. The implementation owns
    /// navigation, interception, bounds and challenge detection; it returns a
    /// typed error rather than a partial page on failure.
    async fn capture(
        &self,
        ctx: &AcquireCtx,
        source: &SourceId,
        profile: &ProfileIdentity,
        url: &CanonicalUrl,
    ) -> Result<CaptureBundle, SourceError>;
}

/// The order in which a connector runs its strategies (strongest first).
pub const STRATEGY_ORDER: [Strategy; 4] = Strategy::ORDER;

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct State {
        queue: VecDeque<Result<CaptureBundle, SourceError>>,
        calls: Vec<(String, String)>,
    }

    /// A scripted browser: replays queued bundles offline and records the
    /// `(url, profile)` pairs it was asked for, so tests can prove the
    /// profile identity is stable and no per-request switching happens.
    #[derive(Default)]
    pub struct ScriptedCapture {
        state: Mutex<State>,
    }

    impl ScriptedCapture {
        /// An empty script.
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue one bundle.
        pub fn push(&self, bundle: CaptureBundle) {
            self.lock().queue.push_back(Ok(bundle));
        }

        /// Every `(url, profile-name)` pair, in order.
        pub fn calls(&self) -> Vec<(String, String)> {
            self.lock().calls.clone()
        }

        /// How many captures were attempted.
        pub fn call_count(&self) -> usize {
            self.lock().calls.len()
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    #[async_trait]
    impl BrowserExtraction for ScriptedCapture {
        async fn capture(
            &self,
            _ctx: &AcquireCtx,
            _source: &SourceId,
            profile: &ProfileIdentity,
            url: &CanonicalUrl,
        ) -> Result<CaptureBundle, SourceError> {
            let mut state = self.lock();
            state.calls.push((
                url.as_str().to_string(),
                profile.profile.as_str().to_string(),
            ));
            match state.queue.pop_front() {
                Some(result) => result,
                None => Err(SourceError::BrowserUnavailable),
            }
        }
    }

    /// A minimal bundle around one payload.
    pub fn bundle(url: &str, payloads: Vec<CapturedPayload>) -> CaptureBundle {
        CaptureBundle::new(
            CanonicalUrl::parse(url).expect("test url"),
            payloads,
            None,
            1_700_000_000_000,
        )
        .expect("bundle")
    }

    /// A minimal payload.
    pub fn payload(kind: CaptureKind, url: &str, body: &str) -> CapturedPayload {
        CapturedPayload::new(
            kind,
            CanonicalUrl::parse(url).expect("test url"),
            None,
            body.as_bytes().to_vec(),
            1_700_000_000_000,
        )
        .expect("payload")
    }

    /// A bundle carrying one challenge.
    pub fn challenged(url: &str, kind: ChallengeKind, evidence: &str) -> CaptureBundle {
        CaptureBundle::new(
            CanonicalUrl::parse(url).expect("test url"),
            Vec::new(),
            Some(Challenge::new(kind, evidence)),
            1_700_000_000_000,
        )
        .expect("bundle")
    }
}

/// A shared capture authority handle.
pub type SharedBrowserExtraction = Arc<dyn BrowserExtraction>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::capture::test_support::{bundle, payload};

    fn cny_url() -> CanonicalUrl {
        CanonicalUrl::parse("https://detail.1688.com/offer/678.html").expect("url")
    }

    #[test]
    fn first_party_policy_rejects_lookalikes_and_third_parties() {
        let policy = FirstPartyPolicy::new(&["1688.com"]);
        assert!(policy.allows(&cny_url()));
        assert!(policy.allows(&CanonicalUrl::parse("https://s.1688.com/x").expect("url")));
        assert!(
            !policy.allows(&CanonicalUrl::parse("https://evil.test/detail.1688.com").expect("url"))
        );
        assert!(!policy.allows(&CanonicalUrl::parse("https://1688.com.evil.test/x").expect("url")));
        assert!(
            !policy.allows(&CanonicalUrl::parse("https://tracker.test/price.json").expect("url"))
        );
    }

    #[test]
    fn third_party_payloads_are_dropped_before_extraction() {
        let mut bundle = bundle(
            "https://detail.1688.com/offer/678.html",
            vec![
                payload(
                    CaptureKind::NetworkJson,
                    "https://detail.1688.com/api/x",
                    "{}",
                ),
                payload(
                    CaptureKind::NetworkJson,
                    "https://tracker.test/price",
                    "{\"price\":\"2.00\"}",
                ),
            ],
        );
        let dropped = bundle.retain_first_party(&FirstPartyPolicy::new(&["1688.com"]));
        assert_eq!(dropped, 1);
        assert_eq!(bundle.payloads.len(), 1);
        assert!(bundle.payloads[0].url.host().ends_with("1688.com"));
    }

    #[test]
    fn challenges_map_to_typed_errors() {
        let markers = ChallengeMarkers {
            login: &["请登录", "sign in"],
            captcha: &["captcha", "验证码"],
            slider: &["拖动滑块", "slide to verify"],
            interstitial: &["verify you are human", "安全验证"],
            access_denied: &["access denied", "访问受限"],
            rate_limit: &["too many requests", "访问过于频繁"],
        };
        assert_eq!(
            markers.detect("请登录后继续").expect("login").kind,
            ChallengeKind::LoginForm
        );
        assert_eq!(
            markers.detect("请输入验证码").expect("captcha").kind,
            ChallengeKind::Captcha
        );
        assert_eq!(
            markers.detect("Too Many Requests").expect("rate").kind,
            ChallengeKind::RateLimitPage
        );
        assert_eq!(markers.detect("normal product page"), None);
        assert!(matches!(
            ChallengeKind::Captcha.to_source_error(),
            SourceError::VerificationRequired {
                kind: VerificationKind::Captcha
            }
        ));
        assert!(matches!(
            ChallengeKind::AccessDenied.to_source_error(),
            SourceError::AuthenticationRequired
        ));
        assert!(matches!(
            ChallengeKind::RateLimitPage.to_source_error(),
            SourceError::RateLimited { .. }
        ));
    }

    #[test]
    fn bundles_are_bounded() {
        let oversized = vec![0u8; MAX_CAPTURE_BYTES + 1];
        assert_eq!(
            CapturedPayload::new(CaptureKind::NetworkJson, cny_url(), None, oversized, 0),
            Err(SourceError::ResponseTooLarge)
        );
        let payloads: Vec<CapturedPayload> = (0..MAX_CAPTURES + 1)
            .map(|_| {
                CapturedPayload::new(CaptureKind::NetworkJson, cny_url(), None, Vec::new(), 0)
                    .expect("payload")
            })
            .collect();
        assert_eq!(
            CaptureBundle::new(cny_url(), payloads, None, 0),
            Err(SourceError::ResponseTooLarge)
        );
    }
}
