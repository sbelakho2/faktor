//! faktor-browser — the Chromium/CDP authority (spec §9) and the mandatory
//! local Egress Broker (spec §9, build order §17 steps 9–10).
//!
//! # What this crate is
//!
//! A browser authority with **no site knowledge** and **no model
//! dependencies**: it knows how to launch/attach Chromium through the
//! workspace `ProcessSupervisor`, drive CDP, capture bounded network/DOM
//! data, intercept requests, manage profiles, and route every byte through a
//! local broker that enforces the caller's destination policy. Which sites
//! to visit and what to extract is decided by connectors outside this crate.
//!
//! # Non-negotiables encoded here
//!
//! * Chromium is launched **only** through `faktor-terminal`'s
//!   `ProcessSupervisor` with `ProcessOwner::Browser { source, profile }`
//!   and an `EnvSpec` built by `browser_env_spec` — the child never inherits
//!   provider keys, tokens, API secrets, `FAKTOR_SERVER_PASSWORD`, or proxy
//!   passwords.
//! * Chromium's only configured egress is `127.0.0.1:<broker-port>`
//!   (`--proxy-server` plus a forced-loopback bypass list, with extra args
//!   validated so proxy-only cannot be overridden).
//! * The Egress Broker does destination filtering (per-connector first-party
//!   policies), request accounting, upstream proxy selection with credential
//!   isolation (Chromium never receives upstream credentials), redacted
//!   logging, and health.
//! * Request interception drops video/audio/ads/tracking/images/fonts and
//!   other blocked resource types unless the policy allows them.
//! * Downloads are disabled by default; screenshots only on explicit
//!   request; captured bodies/DOM are bounded; cookies stay inside the
//!   profile directory and are never model-visible.
//! * There is **no anti-bot bypass subsystem**: no CAPTCHA solving, no
//!   fingerprint spoofing, no challenge bypass, no per-block IP rotation, no
//!   cookie theft. Verification interstitials are *detected* and surface as
//!   typed `VerificationRequired`/`AuthenticationRequired`/`RateLimited`
//!   states that stop the profile's automated work.
//!
//! # Honest isolation semantics
//!
//! Today's isolation is **process-level and application-level**, not an OS
//! sandbox: Chromium is a normal supervised child process; its network
//! confinement comes from the broker + interception (fail-closed policy
//! decisions), and its filesystem confinement from a per-profile directory
//! with restrictive permissions. A permitted Chromium could still open raw
//! sockets that bypass the proxy — we do not claim otherwise. The crate
//! never silently downgrades a stricter guarantee: an operator who needs
//! OS-level isolation must add it at the spawn layer
//! (`NetworkIsolation::DenyAll`, which fails closed where no backend
//! exists) knowing that a deny-all child cannot reach the loopback broker
//! either.
//!
//! # Test fixture
//!
//! `src/bin/fake_chromium.rs` is a fake Chromium/CDP server used by this
//! crate's offline test suite (it speaks minimal CDP over a WebSocket and
//! emits a launch dump/journal). It is not part of the runtime surface.

pub mod capture;
pub mod cdp;
pub mod download;
pub mod egress;
pub mod error;
pub mod interception;
pub mod launch;
pub mod manager;
pub mod network;
pub mod page;
pub mod profile;

pub(crate) mod timeutil;

pub use capture::{bound_bytes, bound_text, redact_headers, redact_url, CaptureLimits};
pub use cdp::{CdpClient, CdpConfig, CdpEvent, CdpOutcome};
pub use download::{DownloadDecision, DownloadPolicy};
pub use egress::{
    BlockReason, BrokerConfig, BrokerHandle, BrokerHealth, BrokerState, DestinationDecision,
    DestinationPolicy, EgressAccounting, EgressBroker, HostPattern, ProxyCredentials,
    UpstreamProxy, UpstreamSelector,
};
pub use error::{BrowserError, VerificationKind};
pub use interception::{
    InterceptionDecision, InterceptionPolicy, InterceptionStats, Interceptor, ResourceType,
};
pub use launch::{chromium_args, ChromiumLauncher, LaunchOptions, LaunchedBrowser};
pub use manager::{
    BrowserConfig, BrowserHealth, BrowserIdentity, BrowserManager, BrowserState, PagePurpose,
};
pub use network::{BodyCapturer, CapturedBody, NetworkLimits, NetworkRequest, NetworkTracker};
pub use page::{Lifecycle, NavigationOutcome, Page, PageState, VerificationSignal};
pub use profile::{validate_profile_name, IncognitoProfile, ProfileStore};
