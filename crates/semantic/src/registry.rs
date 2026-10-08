//! Provider registry, capability-driven selection and guarded dispatch
//! (audit 48-54/58/59).
//!
//! Selection is by VALIDATED [`crate::types::SemanticProviderDescriptor`]
//! capabilities only — never by provider name, never by a hardcoded
//! language. External providers are invisible until their descriptor
//! handshake succeeds, so no configured provider ever advertises an
//! unvalidated `ALL`.
//!
//! Dispatch iterates EVERY compatible provider in deterministic registration
//! order: a recoverable failure (crash, panic, malformed response, transport
//! error, provider-internal watchdog timeout) advances to the next provider,
//! while caller cancellation/deadline errors are terminal and never fail
//! over. When no provider is compatible (or all are cooling/failed),
//! ordinary calls degrade to the generic fallback; `require_provider` calls
//! demand that one configured compatible provider actually serves the
//! operation and never accept a generic substitution.
//!
//! Every provider call is wrapped in [`guard_call`]: cancellation and
//! deadline are checked before polling the provider at all, and panics are
//! caught and converted to typed errors. Recoverable failures feed the
//! bounded per-provider [`SemanticHealthTracker`] keyed by `(provider id,
//! transport identity, operation class)`: a cooling provider is skipped
//! entirely until its window elapses, so a crashed provider never adds its
//! full timeout to every call.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use faktor_core::{CancellationToken, Clock, Deadline, SystemClock, WorkspaceId};

use crate::fallback::{GenericSemanticFallback, GENERIC_FALLBACK_ID};
use crate::health::{ProviderHealthKey, SemanticHealthTracker};
use crate::types::{
    AffectedRequest, AffectedSet, BoxFuture, EnvelopeIdentity, SemanticCall, SemanticCapabilities,
    SemanticCompleteness, SemanticContextPack, SemanticContextRequest, SemanticDelta,
    SemanticDeltaRequest, SemanticEnvelope, SemanticError, SemanticExpectation,
    SemanticExplainRequest, SemanticExplanation, SemanticFidelity, SemanticOp,
    SemanticOperationStatus, SemanticPayload, SemanticProvider, SemanticProviderDescriptor,
    SemanticProviderId, SemanticResponseCaps, SemanticSnapshot, SemanticSnapshotId,
    SemanticSnapshotRequest, SemanticVerification, SemanticVerifyRequest, SEMANTIC_SCHEMA_VERSION,
};

/// The provider chosen for one operation.
pub enum SemanticSelection<'a> {
    /// A registered provider whose validated capabilities cover the
    /// requirement.
    Provider(&'a dyn SemanticProvider),
    /// The generic in-crate fallback.
    Fallback(&'a GenericSemanticFallback),
}

/// A provider call guarded for cancellation, deadline and panics.
pub struct GuardedCall<'a, T> {
    provider: SemanticProviderId,
    cancellation: CancellationToken,
    deadline: Option<Deadline>,
    clock: Arc<dyn Clock>,
    future: BoxFuture<'a, Result<T, SemanticError>>,
}

impl<'a, T> GuardedCall<'a, T> {
    fn new(
        provider: SemanticProviderId,
        cancellation: CancellationToken,
        deadline: Option<Deadline>,
        clock: Arc<dyn Clock>,
        future: BoxFuture<'a, Result<T, SemanticError>>,
    ) -> Self {
        Self {
            provider,
            cancellation,
            deadline,
            clock,
            future,
        }
    }
}

/// Wrap one provider call. Cancellation and deadline are observed before
/// every poll of the inner future, so a cancelled or expired call never
/// touches the provider.
pub fn guard_call<'a, T>(
    provider: SemanticProviderId,
    call: SemanticCall,
    clock: Arc<dyn Clock>,
    future: BoxFuture<'a, Result<T, SemanticError>>,
) -> GuardedCall<'a, T> {
    GuardedCall::new(provider, call.cancellation, call.deadline, clock, future)
}

impl<T> Future for GuardedCall<'_, T> {
    type Output = Result<T, SemanticError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.cancellation.is_cancelled() {
            return Poll::Ready(Err(SemanticError::Cancelled {
                provider: this.provider.to_string(),
            }));
        }
        if let Some(deadline) = this.deadline {
            if deadline.is_expired(this.clock.now_ms()) {
                return Poll::Ready(Err(SemanticError::DeadlineExceeded {
                    provider: this.provider.to_string(),
                }));
            }
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.future.as_mut().poll(cx)
        }));
        match outcome {
            Ok(Poll::Ready(result)) => Poll::Ready(result),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(SemanticError::ProviderCrashed {
                provider: this.provider.to_string(),
            })),
        }
    }
}

/// The result of one multi-provider dispatch.
enum DispatchOutcome<T> {
    /// A compatible provider served the operation (validated response).
    Served(T),
    /// No compatible provider exists, or every one is cooling.
    NoCandidate,
    /// Every attempted provider failed recoverably; the last typed error.
    Failed(SemanticError),
    /// The caller cancelled or the call deadline expired: terminal, never
    /// failed over and never degraded to the fallback.
    CallerTerminal(SemanticError),
}

/// Registry of semantic providers plus the always-available generic
/// fallback.
/// The generic fallback's honest per-operation status (audit Tangerine-4).
pub fn fallback_operation_status(op: SemanticOp) -> SemanticOperationStatus {
    let fallback = GenericSemanticFallback::default();
    SemanticOperationStatus {
        available: fallback.capabilities().supports(op),
        fidelity: SemanticFidelity::Heuristic,
        completeness: match op {
            // Degraded packs are bounded slices of the index/metadata, so
            // they are PARTIAL; delta/affected are approximations whose
            // coverage is genuinely unknown.
            SemanticOp::Context | SemanticOp::Explain => SemanticCompleteness::Partial,
            _ => SemanticCompleteness::Unknown,
        },
        source: GENERIC_FALLBACK_ID.to_string(),
    }
}

pub struct SemanticProviderRegistry {
    providers: Vec<Arc<dyn SemanticProvider>>,
    /// Handshake-validated descriptors, parallel to `providers`. Only a
    /// descriptor the registry itself fetched and validated can gate
    /// dispatch — a provider that merely CLAIMS capabilities (or fails its
    /// handshake) never becomes a candidate through this path.
    descriptors: Mutex<Vec<Option<SemanticProviderDescriptor>>>,
    fallback: GenericSemanticFallback,
    clock: Arc<dyn Clock>,
    health: SemanticHealthTracker,
    response_caps: SemanticResponseCaps,
}

impl SemanticProviderRegistry {
    pub fn new(fallback: GenericSemanticFallback) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Self {
            providers: Vec::new(),
            descriptors: Mutex::new(Vec::new()),
            fallback,
            health: SemanticHealthTracker::new(clock.clone()),
            clock,
            response_caps: SemanticResponseCaps::default(),
        }
    }

    /// Register a provider. Order is preference order: compatible providers
    /// are attempted in registration order, and a recoverable failure
    /// advances to the next one.
    pub fn register(&mut self, provider: Arc<dyn SemanticProvider>) -> &mut Self {
        self.providers.push(provider);
        self.descriptors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(None);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.health = SemanticHealthTracker::new(clock.clone());
        self.clock = clock;
        self
    }

    pub fn with_response_caps(mut self, caps: SemanticResponseCaps) -> Self {
        self.response_caps = caps;
        self
    }

    /// The per-operation status (audit Tangerine-4) over the SAME selection
    /// rules consumers use: a registered provider whose VALIDATED descriptor
    /// covers the operation reports its declared fidelity/completeness;
    /// otherwise the generic fallback reports its honest degradation.
    pub fn operation_status(&self, op: SemanticOp) -> SemanticOperationStatus {
        for (index, provider) in self.providers.iter().enumerate() {
            let Some(descriptor) = self.cached_descriptor(index) else {
                continue;
            };
            if descriptor
                .capabilities
                .covers(SemanticCapabilities::for_op(op))
            {
                return SemanticOperationStatus {
                    available: true,
                    // Absent declarations are UNTRUSTED (audit P1-SEMANTIC).
                    fidelity: descriptor.fidelity.unwrap_or(SemanticFidelity::None),
                    completeness: descriptor
                        .completeness
                        .unwrap_or(SemanticCompleteness::Unknown),
                    source: provider.id().as_str().to_string(),
                };
            }
        }
        fallback_operation_status(op)
    }

    /// TRUE when at least one EXTERNAL provider is registered (the generic
    /// fallback is not a provider for model-visible advertising): the
    /// semantic_query tool is only offered to the model when a real
    /// semantic service could answer it.
    pub fn has_external_provider(&self) -> bool {
        !self.providers.is_empty()
    }

    /// TRUE when at least one registered EXTERNAL provider currently stands
    /// behind a VALIDATED descriptor covering `op` (audit P1-UX): model
    /// surfaces are advertised on real readiness, never on registration
    /// alone — a provider that cannot serve the surface must not cause it to
    /// appear. In-process providers validate immediately; an external
    /// provider counts only after its handshake.
    pub fn has_external_provider_for(&self, op: SemanticOp) -> bool {
        let required = SemanticCapabilities::for_op(op);
        self.providers.iter().any(|provider| {
            provider
                .validated_descriptor()
                .is_some_and(|descriptor| descriptor.covers_validated(required))
        })
    }

    pub fn providers(&self) -> &[Arc<dyn SemanticProvider>] {
        &self.providers
    }

    pub fn fallback(&self) -> &GenericSemanticFallback {
        &self.fallback
    }

    /// The bounded per-provider health tracker dispatch records into.
    pub fn health(&self) -> &SemanticHealthTracker {
        &self.health
    }

    /// Select by VALIDATED capabilities only. No provider name or language is
    /// ever inspected; an external provider without a validated descriptor
    /// is not selectable (never `ALL`).
    pub fn select(&self, required: &SemanticCapabilities) -> SemanticSelection<'_> {
        for provider in &self.providers {
            if let Some(descriptor) = provider.validated_descriptor() {
                if descriptor.covers_validated(*required) {
                    return SemanticSelection::Provider(provider.as_ref());
                }
            }
        }
        SemanticSelection::Fallback(&self.fallback)
    }

    /// Async capability selection: fetch/validate any missing descriptors
    /// first (the handshake happens before the FIRST dispatch, so a remote
    /// provider never serves an operation on unvalidated claims), then pick
    /// the first compatible provider in registration order. Health cooldown
    /// is enforced by dispatch, not here: a configured-but-cooling provider
    /// still selects, and the consult then degrades to the generic fallback
    /// (conservative Unknown at the agent), never a silent absence.
    pub fn select_validated<'a>(
        &'a self,
        required: &'a SemanticCapabilities,
        op: SemanticOp,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, SemanticSelection<'a>> {
        Box::pin(async move {
            if self
                .fetch_descriptors(op, cancellation, None)
                .await
                .is_err()
            {
                // Caller-terminal handshake (cancel/deadline): no provider.
                return SemanticSelection::Fallback(&self.fallback);
            }
            for (index, provider) in self.providers.iter().enumerate() {
                let Some(descriptor) = self.cached_descriptor(index) else {
                    continue;
                };
                if !descriptor.covers_validated(*required) {
                    continue;
                }
                return SemanticSelection::Provider(provider.as_ref());
            }
            SemanticSelection::Fallback(&self.fallback)
        })
    }

    fn cached_descriptor(&self, index: usize) -> Option<SemanticProviderDescriptor> {
        self.descriptors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(index)
            .cloned()
            .flatten()
    }

    fn validate_response<T: SemanticPayload>(
        &self,
        provider: &SemanticProviderId,
        workspace: WorkspaceId,
        expected_snapshot: SemanticSnapshotId,
        envelope: &SemanticEnvelope<T>,
    ) -> Result<(), SemanticError> {
        envelope.validate_provider_id(provider)?;
        envelope.validate(
            &SemanticExpectation::new(workspace, expected_snapshot),
            &self.response_caps,
        )
    }

    /// The typed refusal of a provider-required call that no provider served
    /// (absent, cooling, or a provider failure that would otherwise fall
    /// back).
    fn provider_required(&self, op: SemanticOp) -> SemanticError {
        SemanticError::ProviderRequired {
            op: op.as_str().to_string(),
        }
    }

    fn health_key(&self, provider: &dyn SemanticProvider, op: SemanticOp) -> ProviderHealthKey {
        ProviderHealthKey::new(provider.id(), provider.transport_identity(), op)
    }

    /// Fetch + validate the descriptor of every provider that has none yet,
    /// in deterministic registration order. A cooling provider is skipped
    /// (its handshake timeout is not paid again); a recoverable handshake
    /// failure records health and moves on; a caller-terminal outcome stops
    /// immediately.
    async fn fetch_descriptors(
        &self,
        op: SemanticOp,
        cancellation: &CancellationToken,
        deadline: Option<Deadline>,
    ) -> Result<(), SemanticError> {
        for (index, provider) in self.providers.iter().enumerate() {
            if self.cached_descriptor(index).is_some() {
                continue;
            }
            let key = self.health_key(provider.as_ref(), op);
            if self.health.is_cooling(&key) {
                continue;
            }
            let attempt = GuardedCall::new(
                provider.id(),
                cancellation.clone(),
                deadline,
                self.clock.clone(),
                provider.handshake(cancellation.clone()),
            );
            match attempt.await {
                Ok(descriptor) => {
                    let mut descriptors = self
                        .descriptors
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(slot) = descriptors.get_mut(index) {
                        *slot = Some(descriptor);
                    }
                }
                Err(err) if err.caller_terminal() => return Err(err),
                Err(_) => self.health.record_failure(key),
            }
        }
        Ok(())
    }

    /// Compatible, non-cooling providers in deterministic registration order.
    fn compatible(
        &self,
        required: &SemanticCapabilities,
        op: SemanticOp,
        call: &SemanticCall,
    ) -> Vec<Arc<dyn SemanticProvider>> {
        let now = self.clock.now_ms();
        let mut candidates = Vec::new();
        for (index, provider) in self.providers.iter().enumerate() {
            // A pinned call is served ONLY by the pinned provider (audit
            // P1-SEMANTIC): identity derived from that provider must never be
            // answered by a fail-over substitute.
            if let Some(pin) = &call.provider_pin {
                if provider.id() != *pin {
                    continue;
                }
            }
            let Some(descriptor) = self.cached_descriptor(index) else {
                continue;
            };
            if !descriptor.covers_validated(*required) {
                continue;
            }
            if self
                .health
                .is_cooling_at(&self.health_key(provider.as_ref(), op), now)
            {
                continue;
            }
            candidates.push(provider.clone());
        }
        candidates
    }

    /// Iterate every compatible provider until one serves. Recoverable
    /// failures advance to the next provider and feed health; caller
    /// cancellation/deadline is terminal. Validation failures (wrong
    /// identity/workspace/snapshot, malformed payload) are recoverable
    /// provider faults, never accepted data.
    async fn dispatch<T: EnvelopeIdentity, F, V>(
        &self,
        op: SemanticOp,
        required: SemanticCapabilities,
        call: &SemanticCall,
        invoke: F,
        validate: V,
    ) -> DispatchOutcome<T>
    where
        F: for<'p> Fn(&'p dyn SemanticProvider) -> BoxFuture<'p, Result<T, SemanticError>>,
        V: Fn(&SemanticProviderId, &T) -> Result<(), SemanticError>,
    {
        if let Err(err) = self
            .fetch_descriptors(op, &call.cancellation, call.deadline)
            .await
        {
            return DispatchOutcome::CallerTerminal(err);
        }
        let candidates = self.compatible(&required, op, call);
        if candidates.is_empty() {
            return DispatchOutcome::NoCandidate;
        }
        let mut last_error: Option<SemanticError> = None;
        for provider in candidates {
            let key = self.health_key(provider.as_ref(), op);
            if self.health.is_cooling(&key) {
                continue;
            }
            let provider_id = provider.id();
            let started_ms = self.clock.now_ms();
            let attempt = guard_call(
                provider_id.clone(),
                call.clone(),
                self.clock.clone(),
                invoke(provider.as_ref()),
            );
            match attempt.await {
                Ok(value) => match validate(&provider_id, &value).and_then(|()| {
                    value.validate_payload_identity(provider.as_ref(), SEMANTIC_SCHEMA_VERSION)
                }) {
                    Ok(()) => {
                        let latency_ms = self
                            .clock
                            .now_ms()
                            .saturating_sub(started_ms)
                            .max(0)
                            .unsigned_abs();
                        self.health.record_success(key, latency_ms);
                        return DispatchOutcome::Served(value);
                    }
                    Err(err) => {
                        self.health.record_failure(key);
                        last_error = Some(err);
                    }
                },
                Err(err) if err.caller_terminal() => {
                    return DispatchOutcome::CallerTerminal(err);
                }
                Err(err) => {
                    self.health.record_failure(key);
                    last_error = Some(err);
                }
            }
        }
        match last_error {
            Some(err) => DispatchOutcome::Failed(err),
            None => DispatchOutcome::NoCandidate,
        }
    }

    pub fn snapshot(
        &self,
        request: SemanticSnapshotRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticSnapshot>, SemanticError>> {
        Box::pin(async move {
            let outcome = self
                .dispatch(
                    SemanticOp::Snapshot,
                    SemanticCapabilities::SNAPSHOT,
                    &request.call,
                    |provider| provider.snapshot(request.clone()),
                    |provider, envelope: &SemanticEnvelope<SemanticSnapshot>| {
                        self.validate_response(
                            provider,
                            request.workspace,
                            envelope.snapshot_id,
                            envelope,
                        )
                    },
                )
                .await;
            match outcome {
                DispatchOutcome::Served(envelope) => Ok(envelope),
                DispatchOutcome::CallerTerminal(err) => Err(err),
                DispatchOutcome::Failed(err) if request.call.require_provider => Err(err),
                DispatchOutcome::NoCandidate if request.call.require_provider => {
                    Err(self.provider_required(SemanticOp::Snapshot))
                }
                DispatchOutcome::Failed(_) | DispatchOutcome::NoCandidate => {
                    let fallback = guard_call(
                        self.fallback.id(),
                        request.call.clone(),
                        self.clock.clone(),
                        self.fallback.snapshot(request.clone()),
                    )
                    .await?;
                    self.validate_response(
                        &self.fallback.id(),
                        request.workspace,
                        fallback.snapshot_id,
                        &fallback,
                    )?;
                    Ok(fallback)
                }
            }
        })
    }

    pub fn context(
        &self,
        request: SemanticContextRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticContextPack>, SemanticError>> {
        Box::pin(async move {
            request.validate()?;
            let outcome = self
                .dispatch(
                    SemanticOp::Context,
                    SemanticCapabilities::CONTEXT,
                    &request.call,
                    |provider| provider.context(request.clone()),
                    |provider, envelope: &SemanticEnvelope<SemanticContextPack>| {
                        self.validate_response(
                            provider,
                            request.workspace,
                            request.snapshot_id,
                            envelope,
                        )
                    },
                )
                .await;
            match outcome {
                DispatchOutcome::Served(envelope) => Ok(envelope),
                DispatchOutcome::CallerTerminal(err) => Err(err),
                DispatchOutcome::Failed(err) if request.call.require_provider => Err(err),
                DispatchOutcome::NoCandidate if request.call.require_provider => {
                    Err(self.provider_required(SemanticOp::Context))
                }
                DispatchOutcome::Failed(_) | DispatchOutcome::NoCandidate => {
                    let fallback = guard_call(
                        self.fallback.id(),
                        request.call.clone(),
                        self.clock.clone(),
                        self.fallback.context(request.clone()),
                    )
                    .await?;
                    self.validate_response(
                        &self.fallback.id(),
                        request.workspace,
                        request.snapshot_id,
                        &fallback,
                    )?;
                    Ok(fallback)
                }
            }
        })
    }

    pub fn delta(
        &self,
        request: SemanticDeltaRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticDelta>, SemanticError>> {
        Box::pin(async move {
            let outcome = self
                .dispatch(
                    SemanticOp::Delta,
                    SemanticCapabilities::DELTA,
                    &request.call,
                    |provider| provider.delta(request.clone()),
                    |provider, envelope: &SemanticEnvelope<SemanticDelta>| {
                        self.validate_response(
                            provider,
                            request.workspace,
                            envelope.snapshot_id,
                            envelope,
                        )
                    },
                )
                .await;
            match outcome {
                DispatchOutcome::Served(envelope) => Ok(envelope),
                DispatchOutcome::CallerTerminal(err) => Err(err),
                DispatchOutcome::Failed(err) if request.call.require_provider => Err(err),
                DispatchOutcome::NoCandidate if request.call.require_provider => {
                    Err(self.provider_required(SemanticOp::Delta))
                }
                DispatchOutcome::Failed(_) | DispatchOutcome::NoCandidate => {
                    let fallback = guard_call(
                        self.fallback.id(),
                        request.call.clone(),
                        self.clock.clone(),
                        self.fallback.delta(request.clone()),
                    )
                    .await?;
                    self.validate_response(
                        &self.fallback.id(),
                        request.workspace,
                        fallback.snapshot_id,
                        &fallback,
                    )?;
                    Ok(fallback)
                }
            }
        })
    }

    pub fn affected(
        &self,
        request: AffectedRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<AffectedSet>, SemanticError>> {
        Box::pin(async move {
            let outcome = self
                .dispatch(
                    SemanticOp::Affected,
                    SemanticCapabilities::AFFECTED,
                    &request.call,
                    |provider| provider.affected(request.clone()),
                    |provider, envelope: &SemanticEnvelope<AffectedSet>| {
                        self.validate_response(
                            provider,
                            request.workspace,
                            request.snapshot_id,
                            envelope,
                        )
                    },
                )
                .await;
            match outcome {
                DispatchOutcome::Served(envelope) => Ok(envelope),
                DispatchOutcome::CallerTerminal(err) => Err(err),
                DispatchOutcome::Failed(err) if request.call.require_provider => Err(err),
                DispatchOutcome::NoCandidate if request.call.require_provider => {
                    Err(self.provider_required(SemanticOp::Affected))
                }
                DispatchOutcome::Failed(_) | DispatchOutcome::NoCandidate => {
                    let fallback = guard_call(
                        self.fallback.id(),
                        request.call.clone(),
                        self.clock.clone(),
                        self.fallback.affected(request.clone()),
                    )
                    .await?;
                    self.validate_response(
                        &self.fallback.id(),
                        request.workspace,
                        request.snapshot_id,
                        &fallback,
                    )?;
                    Ok(fallback)
                }
            }
        })
    }

    pub fn verify(
        &self,
        request: SemanticVerifyRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticVerification>, SemanticError>> {
        Box::pin(async move {
            let outcome = self
                .dispatch(
                    SemanticOp::Verify,
                    SemanticCapabilities::VERIFY,
                    &request.call,
                    |provider| provider.verify(request.clone()),
                    |provider, envelope: &SemanticEnvelope<SemanticVerification>| {
                        self.validate_response(
                            provider,
                            request.workspace,
                            request.snapshot_id,
                            envelope,
                        )
                    },
                )
                .await;
            match outcome {
                DispatchOutcome::Served(envelope) => Ok(envelope),
                DispatchOutcome::CallerTerminal(err) => Err(err),
                DispatchOutcome::Failed(err) if request.call.require_provider => Err(err),
                DispatchOutcome::NoCandidate if request.call.require_provider => {
                    Err(self.provider_required(SemanticOp::Verify))
                }
                DispatchOutcome::Failed(_) | DispatchOutcome::NoCandidate => {
                    let fallback = guard_call(
                        self.fallback.id(),
                        request.call.clone(),
                        self.clock.clone(),
                        self.fallback.verify(request.clone()),
                    )
                    .await?;
                    self.validate_response(
                        &self.fallback.id(),
                        request.workspace,
                        request.snapshot_id,
                        &fallback,
                    )?;
                    Ok(fallback)
                }
            }
        })
    }

    pub fn explain(
        &self,
        request: SemanticExplainRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticExplanation>, SemanticError>> {
        Box::pin(async move {
            let outcome = self
                .dispatch(
                    SemanticOp::Explain,
                    SemanticCapabilities::EXPLAIN,
                    &request.call,
                    |provider| provider.explain(request.clone()),
                    |provider, envelope: &SemanticEnvelope<SemanticExplanation>| {
                        self.validate_response(
                            provider,
                            request.workspace,
                            request.snapshot_id,
                            envelope,
                        )
                    },
                )
                .await;
            match outcome {
                DispatchOutcome::Served(envelope) => Ok(envelope),
                DispatchOutcome::CallerTerminal(err) => Err(err),
                DispatchOutcome::Failed(err) if request.call.require_provider => Err(err),
                DispatchOutcome::NoCandidate if request.call.require_provider => {
                    Err(self.provider_required(SemanticOp::Explain))
                }
                DispatchOutcome::Failed(_) | DispatchOutcome::NoCandidate => {
                    let fallback = guard_call(
                        self.fallback.id(),
                        request.call.clone(),
                        self.clock.clone(),
                        self.fallback.explain(request.clone()),
                    )
                    .await?;
                    self.validate_response(
                        &self.fallback.id(),
                        request.workspace,
                        request.snapshot_id,
                        &fallback,
                    )?;
                    Ok(fallback)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{block_on, call, entity, provider_id, snapshot};
    use crate::types::{
        SemanticContextPack, SemanticEntityId, SemanticEntityRef, SemanticOp, WorkspacePath,
        SEMANTIC_SCHEMA_VERSION,
    };
    use faktor_core::{TestClock, WorkspaceId};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Waker;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ProbeMode {
        Ready,
        Panic,
        Park,
    }

    struct ProbeProvider {
        id: SemanticProviderId,
        caps: SemanticCapabilities,
        mode: ProbeMode,
        polled: Arc<AtomicBool>,
    }

    impl ProbeProvider {
        fn new(id: &str, caps: SemanticCapabilities, mode: ProbeMode) -> Self {
            Self {
                id: provider_id(id),
                caps,
                mode,
                polled: Arc::new(AtomicBool::new(false)),
            }
        }

        fn context_envelope(
            &self,
            request: &SemanticContextRequest,
        ) -> SemanticEnvelope<SemanticContextPack> {
            SemanticEnvelope::new(
                self.id.clone(),
                1,
                request.workspace,
                request.snapshot_id,
                7,
                SemanticContextPack {
                    items: Vec::new(),
                    truncated: false,
                    total_bytes: 0,
                    degraded: false,
                },
            )
        }
    }

    impl SemanticProvider for ProbeProvider {
        fn id(&self) -> SemanticProviderId {
            self.id.clone()
        }

        fn version(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> SemanticCapabilities {
            self.caps
        }

        fn context(
            &self,
            request: SemanticContextRequest,
        ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticContextPack>, SemanticError>> {
            match self.mode {
                ProbeMode::Ready => {
                    let envelope = self.context_envelope(&request);
                    Box::pin(async move { Ok(envelope) })
                }
                ProbeMode::Panic => Box::pin(async move { panic!("probe provider crash") }),
                ProbeMode::Park => {
                    let polled = self.polled.clone();
                    Box::pin(async move {
                        polled.store(true, Ordering::SeqCst);
                        std::future::pending::<
                            Result<SemanticEnvelope<SemanticContextPack>, SemanticError>,
                        >()
                        .await
                    })
                }
            }
        }
    }

    fn context_request() -> SemanticContextRequest {
        SemanticContextRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            source_revision: "rev-1".to_string(),
            snapshot_id: snapshot(WorkspaceId::new(1), "rev-1"),
            query: "where is the scheduler".to_string(),
            max_items: 8,
            max_bytes: 4096,
        }
    }

    fn verify_request() -> SemanticVerifyRequest {
        SemanticVerifyRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            snapshot_id: snapshot(WorkspaceId::new(1), "rev-1"),
            entity: entity("src/lib.rs", "lib"),
            claim: "scheduler starts".to_string(),
        }
    }

    #[test]
    fn selection_is_capability_driven_not_name_driven() {
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        registry.register(Arc::new(ProbeProvider::new(
            "alpha",
            SemanticCapabilities::CONTEXT,
            ProbeMode::Ready,
        )));
        registry.register(Arc::new(ProbeProvider::new(
            "beta",
            SemanticCapabilities::EXPLAIN,
            ProbeMode::Ready,
        )));
        assert!(matches!(
            registry.select(&SemanticCapabilities::CONTEXT),
            SemanticSelection::Provider(provider) if provider.id().as_str() == "alpha"
        ));
        assert!(matches!(
            registry.select(&SemanticCapabilities::EXPLAIN),
            SemanticSelection::Provider(provider) if provider.id().as_str() == "beta"
        ));
        // Uncovered operation falls to the generic fallback.
        assert!(matches!(
            registry.select(&SemanticCapabilities::VERIFY),
            SemanticSelection::Fallback(_)
        ));
    }

    #[test]
    fn provider_absence_falls_back_and_never_fails_ordinary_operation() {
        let registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let envelope = block_on(registry.context(context_request())).unwrap();
        assert!(envelope.payload.degraded);
        assert_eq!(envelope.provider_id.as_str(), "generic-fallback");

        let snapshot_env = block_on(registry.snapshot(SemanticSnapshotRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            source_revision: "rev-1".to_string(),
        }))
        .unwrap();
        assert_eq!(snapshot_env.workspace, WorkspaceId::new(1));
    }

    #[test]
    fn provider_crash_is_typed_and_degrades_to_fallback() {
        let direct = ProbeProvider::new("crasher", SemanticCapabilities::CONTEXT, ProbeMode::Panic);
        let result = block_on(guard_call(
            direct.id(),
            call(),
            Arc::new(TestClock::new(0)),
            direct.context(context_request()),
        ));
        match result {
            Err(SemanticError::ProviderCrashed { provider }) => assert_eq!(provider, "crasher"),
            other => panic!("expected ProviderCrashed, got {other:?}"),
        }

        // Registry: ordinary operation still succeeds through the fallback.
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        registry.register(Arc::new(ProbeProvider::new(
            "crasher",
            SemanticCapabilities::CONTEXT,
            ProbeMode::Panic,
        )));
        let envelope = block_on(registry.context(context_request())).unwrap();
        assert!(envelope.payload.degraded);
    }

    #[test]
    fn slow_provider_cancel_is_typed_and_stops_before_polling() {
        // Pre-cancelled: the provider is never polled at all.
        let provider = ProbeProvider::new("slow", SemanticCapabilities::CONTEXT, ProbeMode::Park);
        let polled = provider.polled.clone();
        let mut request = context_request();
        let token = CancellationToken::new();
        token.cancel();
        request.call.cancellation = token;
        let result = block_on(guard_call(
            provider.id(),
            request.call.clone(),
            Arc::new(TestClock::new(0)),
            provider.context(request),
        ));
        assert!(matches!(result, Err(SemanticError::Cancelled { .. })));
        assert!(
            !polled.load(Ordering::SeqCst),
            "a cancelled call must never poll the provider"
        );

        // Parked call: first poll pends, cancel then resolves typed.
        let provider = ProbeProvider::new("slow2", SemanticCapabilities::CONTEXT, ProbeMode::Park);
        let polled = provider.polled.clone();
        let token = CancellationToken::new();
        let mut request = context_request();
        request.call.cancellation = token.clone();
        let mut future = Box::pin(guard_call(
            provider.id(),
            request.call.clone(),
            Arc::new(TestClock::new(0)),
            provider.context(request),
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(polled.load(Ordering::SeqCst), "the parked call ran once");
        token.cancel();
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Err(SemanticError::Cancelled { .. })) => {}
            other => panic!("expected typed Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn expired_deadline_is_typed_before_any_provider_poll() {
        let provider = ProbeProvider::new("slow", SemanticCapabilities::CONTEXT, ProbeMode::Park);
        let polled = provider.polled.clone();
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default())
            .with_clock(Arc::new(TestClock::new(1_000)));
        registry.register(Arc::new(provider));
        let mut request = context_request();
        request.call.deadline = Some(Deadline::at(999));
        let result = block_on(registry.context(request));
        match result {
            Err(SemanticError::DeadlineExceeded { provider }) => assert_eq!(provider, "slow"),
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
        assert!(
            !polled.load(Ordering::SeqCst),
            "an expired call must never poll the provider"
        );
    }

    #[test]
    fn fallback_serves_every_operation_when_no_provider_is_registered() {
        let registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let workspace = WorkspaceId::new(1);
        let snapshot_id = snapshot(workspace, "rev-1");
        let entity = entity("src/lib.rs", "lib");

        assert!(block_on(registry.snapshot(SemanticSnapshotRequest {
            call: call(),
            workspace,
            source_revision: "rev-1".to_string(),
        }))
        .is_ok());
        assert!(block_on(registry.context(context_request())).is_ok());
        assert!(block_on(registry.delta(SemanticDeltaRequest {
            call: call(),
            workspace,
            from_snapshot: snapshot_id,
            from_source_revision: "rev-0".to_string(),
            to_source_revision: "rev-1".to_string(),
        }))
        .is_ok());
        assert!(block_on(registry.affected(AffectedRequest {
            call: call(),
            workspace,
            snapshot_id,
            changed: vec![entity.clone()],
            max_depth: 1,
        }))
        .is_ok());
        assert!(block_on(registry.verify(SemanticVerifyRequest {
            call: call(),
            workspace,
            snapshot_id,
            entity: entity.clone(),
            claim: "scheduler starts".to_string(),
        }))
        .is_ok());
        assert!(block_on(registry.explain(SemanticExplainRequest {
            call: call(),
            workspace,
            snapshot_id,
            entity,
            question: "why".to_string(),
        }))
        .is_ok());
        // TRUTHFUL capability set (audit Tangerine-4): the fallback serves
        // degraded heuristic context/delta/affected/explain data and does NOT
        // claim snapshot or verification.
        for op in [
            SemanticOp::Context,
            SemanticOp::Delta,
            SemanticOp::Affected,
            SemanticOp::Explain,
        ] {
            assert!(registry.fallback().capabilities().supports(op), "{op:?}");
        }
        for op in [SemanticOp::Snapshot, SemanticOp::Verify] {
            assert!(
                !registry.fallback().capabilities().supports(op),
                "the fallback must not claim {op:?}"
            );
        }
    }

    /// Audit Tangerine-4: per-operation status carries fidelity and
    /// completeness; the fallback is heuristic/partial, never Tangerine.
    #[test]
    fn operation_status_distinguishes_fallback_from_a_registered_provider() {
        let registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let context = registry.operation_status(SemanticOp::Context);
        assert!(context.available);
        assert_eq!(context.fidelity, SemanticFidelity::Heuristic);
        assert_eq!(context.completeness, SemanticCompleteness::Partial);
        assert_eq!(context.source, GENERIC_FALLBACK_ID);
        let verify = registry.operation_status(SemanticOp::Verify);
        assert!(!verify.available, "fallback cannot verify");
        // The minimum-fidelity table (audit Tangerine-3/4).
        assert_eq!(
            SemanticOp::Context.minimum_fidelity(),
            SemanticFidelity::Heuristic
        );
        assert_eq!(
            SemanticOp::Affected.minimum_fidelity(),
            SemanticFidelity::Structural
        );
        assert_eq!(
            SemanticOp::Delta.minimum_fidelity(),
            SemanticFidelity::CompilerExact
        );
        assert_eq!(
            SemanticOp::Verify.minimum_fidelity(),
            SemanticFidelity::Proof
        );
    }

    /// P1-SEMANTIC: an ABSENT fidelity/completeness declaration is UNTRUSTED
    /// (`None`/`Unknown`) — trust is claimed explicitly, never inherited
    /// from silence — and a declared level is reported verbatim.
    #[test]
    fn provider_status_reports_declared_or_untrusted_fidelity() {
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let provider = Arc::new(ScriptedProvider::new(
            "fidelity-probe",
            SemanticCapabilities::CONTEXT,
            ScriptedBehavior::Ready,
        ));
        registry.register(provider);
        assert!(block_on(registry.fetch_descriptors(
            SemanticOp::Context,
            &CancellationToken::new(),
            None
        ))
        .is_ok());
        let status = registry.operation_status(SemanticOp::Context);
        assert!(status.available);
        assert_eq!(status.fidelity, SemanticFidelity::None);
        assert_eq!(status.completeness, SemanticCompleteness::Unknown);
        assert_eq!(status.source, "fidelity-probe");

        // A declared level is reported as declared.
        let mut declared = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        declared.register(Arc::new(
            ScriptedProvider::new(
                "declared-probe",
                SemanticCapabilities::CONTEXT,
                ScriptedBehavior::Ready,
            )
            .declaring(
                SemanticFidelity::CompilerExact,
                SemanticCompleteness::ConservativeComplete,
            ),
        ));
        assert!(block_on(declared.fetch_descriptors(
            SemanticOp::Context,
            &CancellationToken::new(),
            None
        ))
        .is_ok());
        let status = declared.operation_status(SemanticOp::Context);
        assert_eq!(status.fidelity, SemanticFidelity::CompilerExact);
        assert_eq!(
            status.completeness,
            SemanticCompleteness::ConservativeComplete
        );
    }

    /// Tangerine-2 production wiring: the registry dispatch path ITSELF
    /// refuses a provider response whose entity refs carry Faktor-heuristic
    /// (path-derived) origins. Stable provider identities must originate in
    /// the provider; Faktor never synthesizes them into accepted evidence.
    #[test]
    fn registry_dispatch_refuses_heuristic_origin_entity_refs() {
        let provider = Arc::new(ScriptedProvider::new(
            "heuristic-impostor",
            SemanticCapabilities::AFFECTED,
            ScriptedBehavior::HeuristicRefs,
        ));
        let registry = registry_with(Arc::new(TestClock::new(1_000)), vec![provider]);
        let workspace = WorkspaceId::new(1);
        let mut call = call();
        call.require_provider = true;
        let err = block_on(registry.affected(AffectedRequest {
            call,
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a")],
            max_depth: 1,
        }))
        .expect_err("heuristic-origin refs must be refused typed");
        match err {
            SemanticError::InvalidEntityRef(detail) => {
                assert!(detail.contains("heuristic"), "{detail}")
            }
            other => panic!("expected InvalidEntityRef, got {other:?}"),
        }
    }

    /// P1-SEMANTIC: a PINNED call is served only by the pinned provider — a
    /// healthy fail-over candidate must never answer a call whose identity
    /// (snapshot/view) was derived from the selected provider.
    #[test]
    fn pinned_call_never_fails_over() {
        let pinned = Arc::new(ScriptedProvider::new(
            "pinned",
            SemanticCapabilities::AFFECTED,
            ScriptedBehavior::Refused,
        ));
        let healthy = Arc::new(ScriptedProvider::new(
            "healthy",
            SemanticCapabilities::AFFECTED,
            ScriptedBehavior::Ready,
        ));
        let registry = registry_with(
            Arc::new(TestClock::new(1_000)),
            vec![pinned.clone(), healthy.clone()],
        );
        let workspace = WorkspaceId::new(1);
        let mut call = call();
        call.require_provider = true;
        call.provider_pin = Some(provider_id("pinned"));
        let err = block_on(registry.affected(AffectedRequest {
            call,
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a")],
            max_depth: 1,
        }))
        .expect_err("the pinned provider failing must not fail over");
        assert!(matches!(err, SemanticError::Refused(_)), "{err:?}");
        assert_eq!(
            healthy.calls.load(Ordering::SeqCst),
            0,
            "the fail-over candidate must never be invoked for a pinned call"
        );
    }

    // ------------------------------------------------------------------
    // multi-provider failover + capability truth (audit 48-54/58/59)
    // ------------------------------------------------------------------

    /// One scripted provider behavior, exercised through `verify`.
    #[derive(Clone)]
    enum ScriptedBehavior {
        Ready,
        /// Typed recoverable provider failure.
        Fail(&'static str),
        /// A response envelope claiming a DIFFERENT provider id.
        MalformedResponse,
        /// Panics while producing the response.
        Panic,
        /// First call sleeps then panics (a timeout-priced crash); later
        /// calls panic immediately.
        SlowCrashOnce(u64),
        /// Provider-internal watchdog expiry: the first call sleeps for the
        /// watchdog interval then reports the recoverable
        /// [`SemanticError::ProviderTimeout`]; later calls do too.
        TimeoutOnce(u64),
        /// Caller-terminal cancellation from the provider.
        Cancelled,
        /// The CALLER's own deadline surfaced as a typed error: terminal.
        Deadline,
        /// Handshake refuses a schema this build does not speak.
        HandshakeRefused,
        /// Data operations refuse recoverably.
        Refused,
        /// An affected response whose entity refs are Faktor-HEURISTIC
        /// (path-derived): provider responses must never present them
        /// (audit Tangerine-2).
        HeuristicRefs,
    }

    struct ScriptedProvider {
        id: SemanticProviderId,
        caps: SemanticCapabilities,
        behavior: ScriptedBehavior,
        calls: Arc<AtomicUsize>,
        handshakes: Arc<AtomicUsize>,
        clock: Option<Arc<TestClock>>,
        advance_ms: i64,
        fidelity_declared: Option<SemanticFidelity>,
        completeness_declared: Option<SemanticCompleteness>,
    }

    impl ScriptedProvider {
        fn new(id: &str, caps: SemanticCapabilities, behavior: ScriptedBehavior) -> Self {
            Self {
                id: provider_id(id),
                caps,
                behavior,
                calls: Arc::new(AtomicUsize::new(0)),
                handshakes: Arc::new(AtomicUsize::new(0)),
                clock: None,
                advance_ms: 0,
                fidelity_declared: None,
                completeness_declared: None,
            }
        }

        fn ready(id: &str, caps: SemanticCapabilities) -> Self {
            Self::new(id, caps, ScriptedBehavior::Ready)
        }

        fn declaring(
            mut self,
            fidelity: SemanticFidelity,
            completeness: SemanticCompleteness,
        ) -> Self {
            self.fidelity_declared = Some(fidelity);
            self.completeness_declared = Some(completeness);
            self
        }

        fn advancing(mut self, clock: Arc<TestClock>, advance_ms: i64) -> Self {
            self.clock = Some(clock);
            self.advance_ms = advance_ms;
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn handshakes(&self) -> usize {
            self.handshakes.load(Ordering::SeqCst)
        }

        fn verify_envelope(
            &self,
            request: &SemanticVerifyRequest,
        ) -> SemanticEnvelope<SemanticVerification> {
            SemanticEnvelope::new(
                self.id.clone(),
                1,
                request.workspace,
                request.snapshot_id,
                7,
                SemanticVerification {
                    passed: true,
                    degraded: false,
                    checks: Vec::new(),
                },
            )
        }
    }

    impl SemanticProvider for ScriptedProvider {
        fn id(&self) -> SemanticProviderId {
            self.id.clone()
        }

        fn version(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> SemanticCapabilities {
            self.caps
        }

        fn descriptor(&self) -> SemanticProviderDescriptor {
            let mut descriptor = SemanticProviderDescriptor::new(
                self.id.clone(),
                1,
                SEMANTIC_SCHEMA_VERSION,
                self.caps,
            );
            descriptor.fidelity = self.fidelity_declared;
            descriptor.completeness = self.completeness_declared;
            descriptor
        }

        fn validated_descriptor(&self) -> Option<SemanticProviderDescriptor> {
            if matches!(self.behavior, ScriptedBehavior::HandshakeRefused) {
                return None;
            }
            let descriptor = self.descriptor();
            descriptor.validate().ok().map(|()| descriptor)
        }

        fn handshake(
            &self,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<SemanticProviderDescriptor, SemanticError>> {
            self.handshakes.fetch_add(1, Ordering::SeqCst);
            let behavior = self.behavior.clone();
            let id = self.id.clone();
            Box::pin(async move {
                if matches!(behavior, ScriptedBehavior::HandshakeRefused) {
                    return Err(SemanticError::UnsupportedSchema {
                        supported: SEMANTIC_SCHEMA_VERSION,
                        got: SEMANTIC_SCHEMA_VERSION + 1,
                    });
                }
                let descriptor = self.descriptor();
                if descriptor.id != id {
                    return Err(SemanticError::ProviderMismatch {
                        expected: id,
                        actual: descriptor.id,
                    });
                }
                descriptor.validate()?;
                Ok(descriptor)
            })
        }

        fn affected(
            &self,
            request: AffectedRequest,
        ) -> BoxFuture<'_, Result<SemanticEnvelope<AffectedSet>, SemanticError>> {
            let id = self.id.clone();
            let workspace = request.workspace;
            let snapshot_id = request.snapshot_id;
            let behavior = self.behavior.clone();
            Box::pin(async move {
                match behavior {
                    ScriptedBehavior::Ready => Ok(SemanticEnvelope::new(
                        id,
                        1,
                        workspace,
                        snapshot_id,
                        7,
                        AffectedSet {
                            affected: vec![],
                            tests: vec![],
                            degraded: false,
                        },
                    )),
                    ScriptedBehavior::HeuristicRefs => {
                        let payload = AffectedSet {
                            affected: vec![SemanticEntityRef::heuristic(
                                workspace,
                                WorkspacePath::parse("src/a.rs").expect("test path"),
                                SemanticEntityId::parse("path:src/a.rs").expect("test id"),
                            )],
                            tests: vec![],
                            degraded: false,
                        };
                        Ok(SemanticEnvelope::new(
                            id,
                            1,
                            workspace,
                            snapshot_id,
                            7,
                            payload,
                        ))
                    }
                    ScriptedBehavior::Refused => Err(SemanticError::Refused(
                        "scripted provider refused".to_string(),
                    )),
                    _ => Err(SemanticError::Refused(
                        "scripted provider is not scripted for affected".to_string(),
                    )),
                }
            })
        }

        fn verify(
            &self,
            request: SemanticVerifyRequest,
        ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticVerification>, SemanticError>> {
            let first_call = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            let behavior = self.behavior.clone();
            let id = self.id.clone();
            let clock = self.clock.clone();
            let advance_ms = self.advance_ms;
            Box::pin(async move {
                match behavior {
                    ScriptedBehavior::Ready => {
                        if let Some(clock) = &clock {
                            clock.advance(advance_ms);
                        }
                        let mut envelope = self.verify_envelope(&request);
                        envelope.payload.passed = true;
                        Ok(envelope)
                    }
                    ScriptedBehavior::Fail(detail) => Err(SemanticError::ProviderFailed {
                        provider: id.to_string(),
                        detail: detail.to_string(),
                    }),
                    ScriptedBehavior::MalformedResponse => {
                        let mut envelope = self.verify_envelope(&request);
                        envelope.provider_id = provider_id("other-provider");
                        Ok(envelope)
                    }
                    ScriptedBehavior::Panic => panic!("scripted provider panicked"),
                    ScriptedBehavior::SlowCrashOnce(ms) if first_call => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        panic!("scripted slow provider crashed");
                    }
                    ScriptedBehavior::SlowCrashOnce(_) => {
                        panic!("scripted slow provider crashed")
                    }
                    ScriptedBehavior::TimeoutOnce(ms) if first_call => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Err(SemanticError::ProviderTimeout {
                            provider: id.to_string(),
                        })
                    }
                    ScriptedBehavior::TimeoutOnce(_) => Err(SemanticError::ProviderTimeout {
                        provider: id.to_string(),
                    }),
                    ScriptedBehavior::Cancelled => Err(SemanticError::Cancelled {
                        provider: id.to_string(),
                    }),
                    ScriptedBehavior::Deadline => Err(SemanticError::DeadlineExceeded {
                        provider: id.to_string(),
                    }),
                    ScriptedBehavior::HandshakeRefused => unreachable!("handshake gated"),
                    ScriptedBehavior::HeuristicRefs => {
                        unreachable!("affected-only script")
                    }
                    ScriptedBehavior::Refused => Err(SemanticError::Refused(
                        "scripted provider refused".to_string(),
                    )),
                }
            })
        }
    }

    fn registry_with(
        clock: Arc<TestClock>,
        providers: Vec<Arc<ScriptedProvider>>,
    ) -> SemanticProviderRegistry {
        registry_with_dyn(
            clock,
            providers
                .into_iter()
                .map(|provider| provider as Arc<dyn SemanticProvider>)
                .collect(),
        )
    }

    fn registry_with_dyn(
        clock: Arc<TestClock>,
        providers: Vec<Arc<dyn SemanticProvider>>,
    ) -> SemanticProviderRegistry {
        let mut registry =
            SemanticProviderRegistry::new(GenericSemanticFallback::default()).with_clock(clock);
        for provider in providers {
            registry.register(provider);
        }
        registry
    }

    /// A provider whose capabilities CLAIM coverage but whose descriptor
    /// handshake always fails: only the registry's own validated cache may
    /// gate dispatch, never a claim.
    struct LyingProvider {
        id: SemanticProviderId,
        calls: Arc<AtomicUsize>,
    }

    impl LyingProvider {
        fn new(id: &str) -> Self {
            Self {
                id: provider_id(id),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl SemanticProvider for LyingProvider {
        fn id(&self) -> SemanticProviderId {
            self.id.clone()
        }

        fn version(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> SemanticCapabilities {
            SemanticCapabilities::VERIFY
        }

        fn handshake(
            &self,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<SemanticProviderDescriptor, SemanticError>> {
            let id = self.id.clone();
            Box::pin(async move {
                Err(SemanticError::ProviderFailed {
                    provider: id.to_string(),
                    detail: "descriptor handshake refused".to_string(),
                })
            })
        }

        fn verify(
            &self,
            request: SemanticVerifyRequest,
        ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticVerification>, SemanticError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let envelope = SemanticEnvelope::new(
                self.id.clone(),
                1,
                request.workspace,
                request.snapshot_id,
                7,
                SemanticVerification {
                    passed: true,
                    degraded: false,
                    checks: Vec::new(),
                },
            );
            Box::pin(async move { Ok(envelope) })
        }
    }

    #[tokio::test]
    async fn unvalidated_provider_never_serves_even_when_it_claims_capabilities() {
        let liar = Arc::new(LyingProvider::new("liar"));
        let good = Arc::new(ScriptedProvider::ready(
            "good",
            SemanticCapabilities::VERIFY,
        ));
        let providers: Vec<Arc<dyn SemanticProvider>> = vec![liar.clone(), good.clone()];
        let registry = registry_with_dyn(Arc::new(TestClock::new(0)), providers);
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "good");
        assert_eq!(
            liar.calls(),
            0,
            "an unvalidated provider is never dispatched"
        );

        // require_provider with ONLY the liar: typed ProviderRequired, never
        // a fallback substitution and never the liar's claimed proof.
        let only_liar: Vec<Arc<dyn SemanticProvider>> = vec![liar.clone()];
        let only_liar = registry_with_dyn(Arc::new(TestClock::new(0)), only_liar);
        let mut required = verify_request();
        required.call = required.call.requiring_provider();
        match only_liar.verify(required).await {
            Err(SemanticError::ProviderRequired { op }) => assert_eq!(op, "verify"),
            other => panic!("expected ProviderRequired, got {other:?}"),
        }
        assert_eq!(liar.calls(), 0);
    }

    #[tokio::test]
    async fn verify_goes_straight_to_the_verify_capable_provider() {
        // A serves only Context, B only Verify: Verify must never touch A.
        let context_only = Arc::new(ScriptedProvider::ready("a", SemanticCapabilities::CONTEXT));
        let verify_only = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![context_only.clone(), verify_only.clone()],
        );
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(
            context_only.calls(),
            0,
            "incapable provider is never polled"
        );
        assert_eq!(verify_only.calls(), 1);
        assert!(!envelope.payload.degraded);
    }

    #[tokio::test]
    async fn malformed_provider_response_fails_over_to_the_next_verify_provider() {
        // A and B both advertise Verify; A's response is identity-malformed.
        let hostile = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::MalformedResponse,
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let clock = Arc::new(TestClock::new(0));
        let registry = registry_with(clock, vec![hostile.clone(), good.clone()]);
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(hostile.calls(), 1);
        assert_eq!(good.calls(), 1);
        let hostile_key = ProviderHealthKey::new(
            provider_id("a"),
            hostile.transport_identity(),
            SemanticOp::Verify,
        );
        let health = registry.health().health_for(&hostile_key).unwrap();
        assert_eq!(
            health.consecutive_failures, 1,
            "malformed response is a failure"
        );
        assert!(health.cooldown_until_ms > 0);
    }

    #[tokio::test]
    async fn provider_panic_fails_over_to_the_next_provider_typed() {
        let crasher = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::Panic,
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![crasher.clone(), good.clone()],
        );
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(crasher.calls(), 1);
        let key = ProviderHealthKey::new(
            provider_id("a"),
            crasher.transport_identity(),
            SemanticOp::Verify,
        );
        assert_eq!(
            registry
                .health()
                .health_for(&key)
                .unwrap()
                .consecutive_failures,
            1,
            "a panic is a recoverable provider failure"
        );
    }

    #[tokio::test]
    async fn malformed_descriptor_handshake_fails_over_to_the_valid_provider() {
        // A's descriptor handshake refuses a schema it cannot speak; B's
        // validated descriptor serves. A must never be dispatched.
        let broken = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::HandshakeRefused,
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![broken.clone(), good.clone()],
        );
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(broken.handshakes(), 1);
        assert_eq!(
            broken.calls(),
            0,
            "unvalidated provider is never dispatched"
        );
        assert_eq!(good.calls(), 1);
        let broken_key = ProviderHealthKey::new(
            provider_id("a"),
            broken.transport_identity(),
            SemanticOp::Verify,
        );
        assert_eq!(
            registry
                .health()
                .health_for(&broken_key)
                .unwrap()
                .consecutive_failures,
            1
        );
    }

    #[tokio::test]
    async fn require_provider_succeeds_when_a_later_provider_serves() {
        let failing = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::Fail("a is down"),
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![failing.clone(), good.clone()],
        );
        let mut request = verify_request();
        request.call = request.call.requiring_provider();
        let envelope = registry.verify(request).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(failing.calls(), 1);
        assert_eq!(good.calls(), 1);
    }

    #[tokio::test]
    async fn all_providers_failing_never_substitutes_generic_proof() {
        let a = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::Fail("a is down"),
        ));
        let b = Arc::new(ScriptedProvider::new(
            "b",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::Fail("b is down"),
        ));
        let registry = registry_with(Arc::new(TestClock::new(0)), vec![a.clone(), b.clone()]);

        // require_provider: the last typed provider failure is terminal.
        let mut required = verify_request();
        required.call = required.call.requiring_provider();
        match registry.verify(required).await {
            Err(SemanticError::ProviderFailed { provider, detail }) => {
                assert_eq!(provider, "b");
                assert_eq!(detail, "b is down");
            }
            other => panic!("expected the typed provider failure, got {other:?}"),
        }
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);

        // Absent/degraded providers + require_provider: typed ProviderRequired.
        let empty = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let mut required = verify_request();
        required.call = required.call.requiring_provider();
        match empty.verify(required).await {
            Err(SemanticError::ProviderRequired { op }) => assert_eq!(op, "verify"),
            other => panic!("expected ProviderRequired, got {other:?}"),
        }

        // Ordinary consult: the generic fallback serves, explicitly degraded.
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "generic-fallback");
        assert!(envelope.payload.degraded);
    }

    #[tokio::test]
    async fn cancellation_is_terminal_and_does_not_fail_over() {
        let cancel = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::Cancelled,
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![cancel.clone(), good.clone()],
        );
        match registry.verify(verify_request()).await {
            Err(SemanticError::Cancelled { provider }) => assert_eq!(provider, "a"),
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert_eq!(good.calls(), 0, "caller-terminal errors never fail over");
    }

    #[tokio::test]
    async fn caller_deadline_surfaced_by_a_provider_is_terminal_and_does_not_fail_over() {
        // `DeadlineExceeded` is the CALLER's deadline only; a provider that
        // reports it (or the guard wrapping one) is terminal. Provider
        // watchdog expiries use `ProviderTimeout` and DO fail over.
        let deadline = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::Deadline,
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![deadline.clone(), good.clone()],
        );
        assert!(matches!(
            registry.verify(verify_request()).await,
            Err(SemanticError::DeadlineExceeded { .. })
        ));
        assert_eq!(good.calls(), 0, "caller deadlines never fail over");
    }

    #[tokio::test]
    async fn provider_watchdog_timeout_fails_over_and_cools_down() {
        let hung = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::TimeoutOnce(300),
        ));
        let healthy = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let clock = Arc::new(TestClock::new(0));
        let registry = registry_with(clock.clone(), vec![hung.clone(), healthy.clone()]);

        // Hung A: its internal watchdog expires (recoverable ProviderTimeout)
        // and dispatch fails over to healthy B.
        let start = Instant::now();
        let envelope = registry.verify(verify_request()).await.unwrap();
        let first_elapsed = start.elapsed();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(hung.calls(), 1);
        assert_eq!(healthy.calls(), 1);
        assert!(
            first_elapsed >= Duration::from_millis(250),
            "the first call must actually pay A's watchdog: {first_elapsed:?}"
        );
        // A's failure is recorded and its health key is cooling.
        let key = ProviderHealthKey::new(
            provider_id("a"),
            hung.transport_identity(),
            SemanticOp::Verify,
        );
        let health = registry.health().health_for(&key).unwrap();
        assert_eq!(health.consecutive_failures, 1);
        assert!(health.cooldown_until_ms > clock.now_ms());

        // Subsequent call inside the cooldown: A is skipped, so its watchdog
        // is NOT paid again — bounded wall-clock assertion.
        let start = Instant::now();
        let envelope = registry.verify(verify_request()).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(hung.calls(), 1, "a cooling provider is never polled");
        assert!(
            elapsed < Duration::from_millis(250),
            "cooldown must not add the provider watchdog again: {elapsed:?}"
        );

        // Cooldown elapses: A is retried (its watchdog is attempted again;
        // B still serves the call).
        clock.advance(crate::health::BASE_COOLDOWN_MS + 1);
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "b");
        assert_eq!(hung.calls(), 2, "cooldown expiry retries the provider");
    }

    #[tokio::test]
    async fn provider_timeout_is_typed_for_require_provider_never_a_fallback() {
        let hung = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::TimeoutOnce(0),
        ));
        let registry = registry_with(Arc::new(TestClock::new(0)), vec![hung.clone()]);
        let mut request = verify_request();
        request.call = request.call.requiring_provider();
        match registry.verify(request).await {
            Err(SemanticError::ProviderTimeout { provider }) => assert_eq!(provider, "a"),
            other => panic!("expected ProviderTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cooldown_skips_a_crashed_provider_without_paying_its_timeout() {
        let slow = Arc::new(ScriptedProvider::new(
            "slow",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::SlowCrashOnce(600),
        ));
        let fast = Arc::new(ScriptedProvider::ready(
            "fast",
            SemanticCapabilities::VERIFY,
        ));
        let clock = Arc::new(TestClock::new(0));
        let registry = registry_with(clock.clone(), vec![slow.clone(), fast.clone()]);

        // First call: the crash is typed and B serves (after A's real wait).
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "fast");
        assert_eq!(slow.calls(), 1);

        // Second call inside the cooldown: A is skipped, so its crash
        // timeout is NOT paid again — bounded wall-clock assertion.
        let start = Instant::now();
        let envelope = registry.verify(verify_request()).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(envelope.provider_id.as_str(), "fast");
        assert_eq!(slow.calls(), 1, "cooling provider is never polled");
        assert!(
            elapsed < Duration::from_millis(250),
            "cooldown must not add the provider timeout again: {elapsed:?}"
        );

        // Cooldown elapses: the provider is retried (fast still serves).
        clock.advance(crate::health::BASE_COOLDOWN_MS + 1);
        let envelope = registry.verify(verify_request()).await.unwrap();
        assert_eq!(envelope.provider_id.as_str(), "fast");
        assert_eq!(slow.calls(), 2, "cooldown expiry retries the provider");
    }

    #[tokio::test]
    async fn latency_samples_are_recorded_on_success() {
        let clock = Arc::new(TestClock::new(100));
        let provider = Arc::new(
            ScriptedProvider::ready("a", SemanticCapabilities::VERIFY).advancing(clock.clone(), 42),
        );
        let registry = registry_with(clock.clone(), vec![provider]);
        registry.verify(verify_request()).await.unwrap();
        let key = ProviderHealthKey::new(
            provider_id("a"),
            "in-process:a".to_string(),
            SemanticOp::Verify,
        );
        let health = registry.health().health_for(&key).unwrap();
        assert_eq!(health.latency_samples_ms, vec![42]);
        assert_eq!(health.last_success_ms, Some(142));
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.cooldown_until_ms, 0);
    }

    #[tokio::test]
    async fn select_validated_handshakes_before_selecting() {
        let broken = Arc::new(ScriptedProvider::new(
            "a",
            SemanticCapabilities::VERIFY,
            ScriptedBehavior::HandshakeRefused,
        ));
        let good = Arc::new(ScriptedProvider::ready("b", SemanticCapabilities::VERIFY));
        let registry = registry_with(
            Arc::new(TestClock::new(0)),
            vec![broken.clone(), good.clone()],
        );
        let cancel = CancellationToken::new();
        let selection = registry
            .select_validated(&SemanticCapabilities::VERIFY, SemanticOp::Verify, &cancel)
            .await;
        assert!(matches!(
            selection,
            SemanticSelection::Provider(provider) if provider.id().as_str() == "b"
        ));
        assert_eq!(broken.handshakes(), 1);
        // An uncovered class selects the fallback, never a provider.
        let selection = registry
            .select_validated(&SemanticCapabilities::CONTEXT, SemanticOp::Context, &cancel)
            .await;
        assert!(matches!(selection, SemanticSelection::Fallback(_)));
        // No compatible provider at all: fallback (parity for ordinary use).
        let empty = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let selection = empty
            .select_validated(&SemanticCapabilities::VERIFY, SemanticOp::Verify, &cancel)
            .await;
        assert!(matches!(selection, SemanticSelection::Fallback(_)));
    }
}
