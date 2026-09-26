//! Shared unit-test scaffolding (never compiled into the library).
//!
//! Site tests build their own context through these helpers so a connector
//! unit test always runs with an injected fixture transport, a manual clock,
//! a real quota state and a secret guard — never the process environment and
//! never a network. The mechanism the runtime planner would have chosen is
//! set explicitly; the default is the primary API mechanism.

use std::sync::Arc;

use faktor_commerce::text::Text;

use crate::context::{AcquireCtx, ManualClock, MemoryDiagnostics};
use crate::contract::Mechanism;
use crate::quota::QuotaState;
use crate::secrets::SecretGuard;
use crate::testing::{FixtureTransport, RecordingBrowser};

/// Fixed deterministic clock origin for tests.
pub(crate) const TEST_NOW_MS: u64 = 1_700_000_000_000;

/// Build a bounded text value (panics only in test setup).
pub(crate) fn text<const MAX: usize>(raw: &str) -> Text<MAX> {
    Text::<MAX>::new(raw).expect("test text is valid")
}

/// A connector test rig: fixture transport, quota, secret guard, bounded
/// diagnostics, a recording browser and a manual clock.
pub(crate) struct Rig {
    pub(crate) transport: Arc<FixtureTransport>,
    pub(crate) quota: Arc<QuotaState>,
    pub(crate) secrets: Arc<SecretGuard>,
    pub(crate) diagnostics: Arc<MemoryDiagnostics>,
    pub(crate) browser: Arc<RecordingBrowser>,
    pub(crate) mechanism: Mechanism,
    pub(crate) clock: Arc<ManualClock>,
}

impl Rig {
    /// A rig with the API mechanism selected and no browser answer.
    pub(crate) fn new() -> Self {
        Self {
            transport: Arc::new(FixtureTransport::new()),
            quota: Arc::new(QuotaState::new()),
            secrets: Arc::new(SecretGuard::new()),
            diagnostics: Arc::new(MemoryDiagnostics::new()),
            browser: Arc::new(RecordingBrowser::new()),
            mechanism: Mechanism::OfficialApi,
            clock: Arc::new(ManualClock::new(TEST_NOW_MS)),
        }
    }

    /// Replace the recording browser.
    pub(crate) fn browser(mut self, browser: Arc<RecordingBrowser>) -> Self {
        self.browser = browser;
        self
    }

    /// Set the mechanism the runtime planner chose.
    pub(crate) fn mechanism(mut self, mechanism: Mechanism) -> Self {
        self.mechanism = mechanism;
        self
    }

    /// Build a context bound to this rig.
    pub(crate) fn ctx(&self) -> AcquireCtx {
        AcquireCtx::builder(
            self.transport.clone(),
            self.quota.clone(),
            self.secrets.clone(),
        )
        .clock(self.clock.clone())
        .diagnostics(self.diagnostics.clone())
        .browser(self.browser.clone())
        .mechanism(self.mechanism)
        .build()
    }
}

/// A context that can never reach the network (no transport responses, no
/// browser, quota not registered by the caller yet).
pub(crate) fn ctx() -> AcquireCtx {
    Rig::new().ctx()
}
