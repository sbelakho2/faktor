//! Additive commercial provider-attempt debits (Wave 5 residual): the ONE
//! agent-side call site that turns a Faktor-managed provider attempt into a
//! durable pre-call credit hold, a settle at the actual cost, or a refund.
//!
//! The runtime owns an optional [`ProviderAttemptDebits`] authority. Without
//! one (`None` — billing disabled) every machine below is a documented no-op
//! and the agent behaves byte-identically to the pre-billing runtime: no
//! debit call is ever made, no field of the dispatch path changes.
//!
//! The decision "is this model Faktor-managed" is CONFIG-derived: the
//! authority answers [`ProviderAttemptDebits::is_managed`] from the
//! operator's configured managed-provider set (the cloud billing service's
//! `BillingConfig`), never from a provider-name check in the agent.
//!
//! Record-before-call durability: [`AttemptDebits::begin`] must complete
//! BEFORE the provider stream is opened. A crash between the hold and the
//! call leaves the hold in place — never a silently free managed call. Only
//! a provably-never-dispatched attempt refunds; every post-dispatch outcome
//! (error, stall, cancel, refused settle) keeps the hold for settlement or
//! reconciliation.
//!
//! BYOK models take the `None`-authority path at the dispatch site: the
//! authority is consulted and answers `Byok`, so usage is recorded exactly
//! as before and no credit entry is ever written.

use std::sync::Arc;

/// Hard bound on one debit request's text fields (attempt/provider/model ids
/// and the reason label).
pub const MAX_DEBIT_TEXT_BYTES: usize = 256;

/// One physical provider attempt's debit identity. Built at the dispatch
/// site from the attempt-keyed identity, so a retry is a NEW attempt with a
/// fresh id and its own hold (never a double debit of one id, never a shared
/// hold across attempts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAttemptDebit {
    /// The attempt's globally fresh op id (idempotency key of the hold).
    pub attempt_id: String,
    pub provider: String,
    pub model: String,
    pub session_id: u64,
    pub task_id: Option<u64>,
    /// The conservative pre-call estimate the hold is opened at.
    pub estimate_micro: u64,
    /// The bounded reason label recorded on the durable entry.
    pub reason: String,
}

impl ProviderAttemptDebit {
    pub fn validate(&self) -> Result<(), DebitError> {
        for (field, value) in [
            ("attempt_id", &self.attempt_id),
            ("provider", &self.provider),
            ("model", &self.model),
            ("reason", &self.reason),
        ] {
            if value.is_empty() || value.len() > MAX_DEBIT_TEXT_BYTES {
                return Err(DebitError::InvalidState {
                    detail: format!("{field} must be 1..={MAX_DEBIT_TEXT_BYTES} bytes"),
                });
            }
        }
        if self.session_id == 0 {
            return Err(DebitError::InvalidState {
                detail: "session_id must be non-zero".into(),
            });
        }
        Ok(())
    }
}

/// One durable hold token: the opaque authority-side entry id the settle or
/// refund names, plus the amount it holds. Never a secret; the `Debug` shape
/// is bounded by the construction bounds above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebitHold {
    pub id: String,
    pub amount_micro: u64,
}

impl DebitHold {
    pub fn new(id: impl Into<String>, amount_micro: u64) -> Result<Self, DebitError> {
        let id = id.into();
        if id.is_empty() || id.len() > MAX_DEBIT_TEXT_BYTES {
            return Err(DebitError::InvalidState {
                detail: format!("hold id must be 1..={MAX_DEBIT_TEXT_BYTES} bytes"),
            });
        }
        Ok(Self { id, amount_micro })
    }
}

/// The authority's answer to one pre-call debit request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebitDecision {
    /// A durable hold was opened (managed spend): the caller MUST dispatch
    /// and later settle it, or refund it before dispatch.
    Hold(DebitHold),
    /// The model is BYOK (or billing is off): usage is recorded, nothing is
    /// debited.
    Byok,
}

/// A typed debit failure. `Refused` is an authoritative billing refusal
/// (insufficient credits, quota) — the attempt must NOT be dispatched.
/// `Unavailable` is an infrastructure failure; callers treat it as a
/// pre-dispatch failure (the attempt is refused, the budget reservation
/// refunded). `InvalidState` is a local machine-order violation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DebitError {
    #[error("managed credit debit refused: {reason}")]
    Refused { reason: String },
    #[error("managed credit debit unavailable: {reason}")]
    Unavailable { reason: String },
    #[error("credit debit machine state violation: {detail}")]
    InvalidState { detail: String },
}

/// The ONE debit authority the runtime consults at the provider dispatch
/// site. Implementations are thread-safe and synchronous (the durable
/// billing store is a local transactional write; the dispatch path must not
/// block on anything else).
pub trait ProviderAttemptDebits: Send + Sync {
    /// Whether this provider's spend is Faktor-managed under the configured
    /// managed-provider set.
    fn is_managed(&self, provider: &str) -> bool;
    /// Record-before-call: durably hold the estimate for this attempt.
    /// Idempotent per `attempt_id`: a replay returns the same hold and
    /// nothing is double-debited.
    fn begin(&self, attempt: &ProviderAttemptDebit) -> Result<DebitDecision, DebitError>;
    /// Settle the hold at the attempt's actual cost. A zero actual refunds
    /// the full hold (the authority decides; never a zero-amount settle).
    fn settle(
        &self,
        attempt: &ProviderAttemptDebit,
        hold: &DebitHold,
        actual_micro: u64,
    ) -> Result<(), DebitError>;
    /// Refund a definitely-not-sent attempt's hold in full.
    fn refund(
        &self,
        attempt: &ProviderAttemptDebit,
        hold: &DebitHold,
        reason: &str,
    ) -> Result<(), DebitError>;
}

/// One physical attempt's credit-debit machine. Guards the ordering so a
/// settle/refund can never be scattered wrongly:
///
/// ```text
///   begin (record-before-call) --> [mark_dispatched] --> settle(actual)
///      |                                  |
///   refund (pre-dispatch only)      close_uncertain (hold stays)
/// ```
///
/// `authority == None` (billing disabled) keeps every transition a no-op:
/// the pre-billing byte-identical path.
pub struct AttemptDebits {
    authority: Option<Arc<dyn ProviderAttemptDebits>>,
    attempt: ProviderAttemptDebit,
    decision: Option<DebitDecision>,
    began: bool,
    dispatched: bool,
    closed: bool,
}

impl std::fmt::Debug for AttemptDebits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptDebits")
            .field("attempt_id", &self.attempt.attempt_id)
            .field("enabled", &self.authority.is_some())
            .field("hold", &self.hold().map(|h| h.amount_micro))
            .field("began", &self.began)
            .field("dispatched", &self.dispatched)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl AttemptDebits {
    /// Build the machine for one physical attempt. The attempt's fields are
    /// validated eagerly: a malformed identity is refused BEFORE any durable
    /// hold (the caller treats it as a pre-dispatch refusal).
    pub fn new(
        authority: Option<Arc<dyn ProviderAttemptDebits>>,
        attempt: ProviderAttemptDebit,
    ) -> Result<Self, DebitError> {
        attempt.validate()?;
        Ok(Self {
            authority,
            attempt,
            decision: None,
            began: false,
            dispatched: false,
            closed: false,
        })
    }

    /// Whether a debit authority is installed (billing enabled).
    pub fn is_enabled(&self) -> bool {
        self.authority.is_some()
    }

    /// The config-derived managed classification of this attempt's provider.
    pub fn is_managed(&self) -> bool {
        self.authority
            .as_ref()
            .map(|a| a.is_managed(&self.attempt.provider))
            .unwrap_or(false)
    }

    pub fn attempt(&self) -> &ProviderAttemptDebit {
        &self.attempt
    }

    /// The hold once [`Self::begin`] opened one (visible pre-dispatch).
    pub fn hold(&self) -> Option<&DebitHold> {
        match &self.decision {
            Some(DebitDecision::Hold(hold)) => Some(hold),
            _ => None,
        }
    }

    /// True once money reached a terminal state (settled or refunded).
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// The record-before-call step: open the durable hold (managed) or
    /// classify BYOK. Called exactly once per physical attempt immediately
    /// BEFORE the provider stream; a second call replays the recorded
    /// decision without touching the authority again (the authority is
    /// itself idempotent per attempt id, this is the local guard).
    pub fn begin(&mut self) -> Result<DebitDecision, DebitError> {
        if self.closed {
            return Err(DebitError::InvalidState {
                detail: "attempt debit machine is already closed".into(),
            });
        }
        if self.began {
            return Ok(self.decision.clone().unwrap_or(DebitDecision::Byok));
        }
        let decision = match &self.authority {
            Some(authority) => authority.begin(&self.attempt)?,
            None => DebitDecision::Byok,
        };
        if let DebitDecision::Hold(hold) = &decision {
            if hold.amount_micro != self.attempt.estimate_micro {
                return Err(DebitError::Unavailable {
                    reason: format!(
                        "authority opened a hold of {} micro for a {} micro estimate",
                        hold.amount_micro, self.attempt.estimate_micro
                    ),
                });
            }
        }
        self.decision = Some(decision.clone());
        self.began = true;
        Ok(decision)
    }

    /// The provider request is leaving the process: from here on only
    /// settle or `close_uncertain` are legal (a refund would be a
    /// silently-free call).
    pub fn mark_dispatched(&mut self) {
        self.dispatched = true;
    }

    /// Settle the hold at the attempt's actual cost. A BYOK/disabled attempt
    /// is a no-op (usage was recorded by the ordinary path). Exactly once: a
    /// second terminal transition is refused.
    pub fn settle(&mut self, actual_micro: u64) -> Result<(), DebitError> {
        self.guard_open()?;
        if !self.began {
            return Err(DebitError::InvalidState {
                detail: "settle before the pre-call debit".into(),
            });
        }
        let Some(hold) = self.hold().cloned() else {
            self.closed = true;
            return Ok(());
        };
        let Some(authority) = &self.authority else {
            self.closed = true;
            return Ok(());
        };
        authority.settle(&self.attempt, &hold, actual_micro)?;
        self.closed = true;
        Ok(())
    }

    /// Refund the hold of a definitely-not-sent attempt (pre-dispatch only).
    /// After [`Self::mark_dispatched`] this refuses locally: the provider
    /// may have billed, so the hold must settle or stay for reconciliation.
    pub fn refund(&mut self, reason: &str) -> Result<(), DebitError> {
        self.guard_open()?;
        if !self.began {
            return Err(DebitError::InvalidState {
                detail: "refund before the pre-call debit".into(),
            });
        }
        let Some(hold) = self.hold().cloned() else {
            self.closed = true;
            return Ok(());
        };
        if self.dispatched {
            return Err(DebitError::InvalidState {
                detail:
                    "refund after dispatch is refused; the attempt must settle or stay uncertain"
                        .into(),
            });
        }
        let Some(authority) = &self.authority else {
            self.closed = true;
            return Ok(());
        };
        authority.refund(&self.attempt, &hold, reason)?;
        self.closed = true;
        Ok(())
    }

    /// A post-dispatch terminal outcome (error, stall, cancel, refused
    /// settle): the hold STAYS durable for settlement/reconciliation; the
    /// machine closes without moving money.
    pub fn close_uncertain(&mut self) {
        self.closed = true;
    }

    fn guard_open(&self) -> Result<(), DebitError> {
        if self.closed {
            return Err(DebitError::InvalidState {
                detail: "attempt debit machine is already closed".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A recording stub authority: every call is counted and the ordered
    /// event log proves the pre-dispatch visibility of the hold.
    #[derive(Default)]
    struct Recorder {
        managed: bool,
        events: Mutex<Vec<String>>,
        begins: AtomicUsize,
        settles: Mutex<Vec<u64>>,
        refunds: AtomicUsize,
    }

    impl Recorder {
        fn managed() -> Arc<Self> {
            Arc::new(Self {
                managed: true,
                ..Default::default()
            })
        }
        fn byok() -> Arc<Self> {
            Arc::new(Self {
                managed: false,
                ..Default::default()
            })
        }
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ProviderAttemptDebits for Recorder {
        fn is_managed(&self, _provider: &str) -> bool {
            self.managed
        }
        fn begin(&self, attempt: &ProviderAttemptDebit) -> Result<DebitDecision, DebitError> {
            self.begins.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap()
                .push(format!("begin:{}", attempt.attempt_id));
            if self.managed {
                Ok(DebitDecision::Hold(
                    DebitHold::new(
                        format!("hold-{}", attempt.attempt_id),
                        attempt.estimate_micro,
                    )
                    .unwrap(),
                ))
            } else {
                Ok(DebitDecision::Byok)
            }
        }
        fn settle(
            &self,
            _attempt: &ProviderAttemptDebit,
            _hold: &DebitHold,
            actual_micro: u64,
        ) -> Result<(), DebitError> {
            self.events.lock().unwrap().push("settle".into());
            self.settles.lock().unwrap().push(actual_micro);
            Ok(())
        }
        fn refund(
            &self,
            _attempt: &ProviderAttemptDebit,
            _hold: &DebitHold,
            reason: &str,
        ) -> Result<(), DebitError> {
            self.events.lock().unwrap().push(format!("refund:{reason}"));
            self.refunds.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn attempt() -> ProviderAttemptDebit {
        ProviderAttemptDebit {
            attempt_id: "42".into(),
            provider: "faktor".into(),
            model: "m".into(),
            session_id: 1,
            task_id: Some(2),
            estimate_micro: 100,
            reason: "agent_provider_attempt".into(),
        }
    }

    #[test]
    fn malformed_identities_are_refused_before_any_authority_call() {
        let recorder = Recorder::managed();
        let mut bad = attempt();
        bad.attempt_id = String::new();
        assert!(AttemptDebits::new(Some(recorder.clone()), bad).is_err());
        let mut bad = attempt();
        bad.reason = "x".repeat(MAX_DEBIT_TEXT_BYTES + 1);
        assert!(AttemptDebits::new(Some(recorder.clone()), bad).is_err());
        let mut bad = attempt();
        bad.session_id = 0;
        assert!(AttemptDebits::new(Some(recorder.clone()), bad).is_err());
        assert_eq!(recorder.begins.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn managed_begin_opens_exactly_one_hold_visible_pre_dispatch() {
        let recorder = Recorder::managed();
        let mut machine = AttemptDebits::new(Some(recorder.clone()), attempt()).expect("machine");
        assert!(machine.is_enabled());
        assert!(machine.is_managed());
        assert!(machine.hold().is_none(), "no hold before begin");
        let decision = machine.begin().unwrap();
        assert_eq!(
            decision,
            DebitDecision::Hold(DebitHold::new("hold-42", 100).unwrap())
        );
        // A replayed begin replays the SAME hold without a second authority
        // call: idempotent per attempt id.
        assert_eq!(machine.begin().unwrap(), decision);
        assert_eq!(recorder.begins.load(Ordering::SeqCst), 1);
        assert_eq!(recorder.events(), vec!["begin:42".to_string()]);
        // The hold is visible pre-dispatch (before mark_dispatched).
        assert_eq!(machine.hold().map(|h| h.amount_micro), Some(100));
    }

    #[test]
    fn settle_consumes_the_hold_at_the_actual_and_is_exactly_once() {
        let recorder = Recorder::managed();
        let mut machine = AttemptDebits::new(Some(recorder.clone()), attempt()).expect("machine");
        machine.begin().unwrap();
        machine.mark_dispatched();
        machine.settle(260).unwrap();
        assert!(machine.closed());
        assert_eq!(recorder.settles.lock().unwrap().clone(), vec![260]);
        assert!(machine.settle(1).is_err(), "a second settle is refused");
        assert!(machine.refund("late").is_err(), "refund after settle");
        assert_eq!(recorder.refunds.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pre_dispatch_failure_refunds_and_post_dispatch_failure_never_does() {
        let recorder = Recorder::managed();
        let mut machine = AttemptDebits::new(Some(recorder.clone()), attempt()).expect("machine");
        machine.begin().unwrap();
        machine.refund("dispatch_marker_failed").unwrap();
        assert!(machine.closed());
        assert_eq!(
            recorder.events(),
            vec![
                "begin:42".to_string(),
                "refund:dispatch_marker_failed".to_string()
            ]
        );

        let recorder = Recorder::managed();
        let mut machine = AttemptDebits::new(Some(recorder.clone()), attempt()).expect("machine");
        machine.begin().unwrap();
        machine.mark_dispatched();
        assert!(
            machine.refund("provider_error").is_err(),
            "a post-dispatch refund would be a silently-free call"
        );
        machine.close_uncertain();
        assert!(machine.closed());
        assert!(machine.settle(10).is_err(), "settle after close_uncertain");
        assert_eq!(recorder.refunds.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn byok_never_debits_and_disabled_is_a_no_op() {
        let byok = Recorder::byok();
        let mut machine = AttemptDebits::new(Some(byok.clone()), attempt()).expect("machine");
        assert!(!machine.is_managed());
        assert_eq!(machine.begin().unwrap(), DebitDecision::Byok);
        assert!(machine.hold().is_none());
        assert_eq!(byok.begins.load(Ordering::SeqCst), 1);
        machine.mark_dispatched();
        machine.settle(0).unwrap();
        assert!(machine.closed());

        // Billing disabled: no authority at all, every transition a no-op.
        let mut machine = AttemptDebits::new(None, attempt()).expect("machine");
        assert!(!machine.is_enabled());
        assert_eq!(machine.begin().unwrap(), DebitDecision::Byok);
        machine.mark_dispatched();
        machine.settle(10).unwrap();
        assert!(machine.closed());
    }

    #[test]
    fn machine_order_violations_are_typed() {
        let recorder = Recorder::managed();
        let mut machine = AttemptDebits::new(Some(recorder.clone()), attempt()).expect("machine");
        assert!(machine.settle(1).is_err(), "settle before begin");
        assert!(machine.refund("x").is_err(), "refund before begin");
        machine.begin().unwrap();
        machine.settle(5).unwrap();
        assert!(machine.begin().is_err(), "begin after close");
        assert!(machine.refund("x").is_err());
        assert!(machine.settle(5).is_err());
        assert_eq!(recorder.begins.load(Ordering::SeqCst), 1);
    }
}
