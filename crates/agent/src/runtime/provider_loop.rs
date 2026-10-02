//! `runtime::provider_loop`: cohesive slice of the agent runtime.

use super::*;

/// One live model chunk (audit round 11 "session.next.* frames"): the
/// agent forwards streaming text/reasoning and tool calls to an optional
/// sink; the daemon broadcasts them as the frozen `session.next.*.delta`
/// SSE frames. Absent sink: no overhead (streaming stays journal-free).
#[derive(Debug, Clone)]
pub struct ChunkEvent {
    pub session_id: SessionId,
    pub message_id: Option<i64>,
    pub kind: &'static str,
    pub text: String,
}

/// Bounded, non-blocking live-chunk sender (audit 41).
///
/// The historic `mpsc::unbounded_channel` let a fast model grow memory
/// without any structural cap when the daemon's drainer fell behind. This
/// sink wraps a bounded `mpsc::Sender` ([`CHUNK_CHANNEL_CAPACITY`] events)
/// and NEVER blocks the emitting turn:
///
/// - healthy path: the frame is delivered as-is via `try_send` (no added
///   latency, no reordering);
/// - channel full (slow consumer): text deltas coalesce into ONE pending
///   frame, bounded at [`CHUNK_COALESCE_CAP_BYTES`] keeping the NEWEST
///   bytes (drop-oldest beyond the cap). Merging only ever happens between
///   frames of the SAME (session, message, kind); a delta of a different
///   stream while full replaces the pending frame — frames never mix
///   sessions or messages;
/// - every subsequent emit first flushes the pending frame, so delivery
///   resumes automatically as soon as the channel has room;
/// - receiver dropped: the sink closes permanently and drops content (no
///   consumer exists — buffering on would be unbounded garbage).
///
/// Durable events are untouched: this is the EPHEMERAL live-delta path only
/// (the durable journal is separate).
pub struct ChunkSink {
    pub(crate) state: std::sync::Mutex<SinkState>,
}

pub(crate) struct SinkState {
    pub(crate) tx: Option<tokio::sync::mpsc::Sender<ChunkEvent>>,
    /// One coalesced frame awaiting channel capacity (drop-oldest bounded).
    pub(crate) pending: Option<ChunkEvent>,
    /// Delta bytes that never reached a consumer (trimmed past the coalesce
    /// cap, replaced by a newer stream's frame, or lost on close).
    pub(crate) dropped_bytes: u64,
}

impl ChunkSink {
    /// A fresh bounded chunk channel: the sender half wrapped in the
    /// coalescing sink, the receiver half for the daemon drainer.
    pub fn channel() -> (Arc<Self>, tokio::sync::mpsc::Receiver<ChunkEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(CHUNK_CHANNEL_CAPACITY);
        (Arc::new(Self::new(tx)), rx)
    }

    pub fn new(tx: tokio::sync::mpsc::Sender<ChunkEvent>) -> Self {
        Self {
            state: std::sync::Mutex::new(SinkState {
                tx: Some(tx),
                pending: None,
                dropped_bytes: 0,
            }),
        }
    }

    /// Best-effort delivery that never blocks: try the bounded channel;
    /// on a full channel coalesce into the bounded pending frame (see the
    /// type docs for the exact drop-oldest semantics).
    pub fn try_send(&self, event: ChunkEvent) {
        use tokio::sync::mpsc::error::TrySendError;
        let mut st = self.state.lock().unwrap();
        let event_bytes = event.text.len() as u64;
        let Some(tx) = st.tx.clone() else {
            // The drainer is gone: drop content instead of buffering a
            // garbage pile nobody can ever consume. Every byte that never
            // reaches a consumer must be accounted, including this event.
            if let Some(p) = st.pending.take() {
                st.dropped_bytes = st.dropped_bytes.saturating_add(p.text.len() as u64);
            }
            st.dropped_bytes = st.dropped_bytes.saturating_add(event_bytes);
            return;
        };
        // 1. FIFO recovery: flush the buffered frame first so coalesced
        // content leaves before anything newer — delivery resumes as soon
        // as the channel has room.
        if let Some(p) = st.pending.take() {
            match tx.try_send(p) {
                Ok(()) => {}
                Err(TrySendError::Full(p)) => st.pending = Some(p),
                Err(TrySendError::Closed(p)) => {
                    // The pending frame AND this incoming event are lost.
                    st.dropped_bytes = st
                        .dropped_bytes
                        .saturating_add(p.text.len() as u64)
                        .saturating_add(event_bytes);
                    st.tx = None;
                    return;
                }
            }
        }
        // 2. Healthy path: the channel has room (or the flush just freed
        // the only slot) — deliver the event as-is.
        if st.pending.is_none() {
            match tx.try_send(event) {
                Ok(()) => return,
                Err(TrySendError::Full(event)) => {
                    st.pending = Some(event);
                    return;
                }
                Err(TrySendError::Closed(p)) => {
                    // The receiver closed on this event: it never reached a
                    // consumer and must be counted, not discarded silently.
                    st.dropped_bytes = st.dropped_bytes.saturating_add(p.text.len() as u64);
                    st.tx = None;
                    return;
                }
            }
        }
        // 3. Still full: coalesce. Same (session, message, kind) deltas
        // merge into the pending frame; a different stream replaces it
        // (keep-newest). Frames never merge across sessions/messages.
        let same_key = st.pending.as_ref().is_some_and(|p| {
            (p.session_id, p.message_id, p.kind) == (event.session_id, event.message_id, event.kind)
        });
        if same_key {
            let p = st.pending.as_mut().unwrap();
            p.text.push_str(&event.text);
            let trimmed = front_trim(&mut p.text, CHUNK_COALESCE_CAP_BYTES) as u64;
            st.dropped_bytes = st.dropped_bytes.saturating_add(trimmed);
        } else if let Some(p) = st.pending.replace(event) {
            st.dropped_bytes = st.dropped_bytes.saturating_add(p.text.len() as u64);
        }
    }

    /// Bytes currently buffered in the coalescer (never exceeds
    /// [`CHUNK_COALESCE_CAP_BYTES`]; test/observability hook).
    pub fn buffered_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map_or(0, |p| p.text.len())
    }

    /// Delta bytes dropped before reaching the consumer (backpressure
    /// trims, cross-stream replacements, close losses).
    pub fn dropped_bytes(&self) -> u64 {
        self.state.lock().unwrap().dropped_bytes
    }
}

pub(crate) struct StreamingSummarizer {
    pub(crate) provider: Arc<dyn faktor_provider::Provider>,
    pub(crate) model: String,
    /// Real operation/session identity rides the request metadata (ids can
    /// never be 0 — the envelope is mandatory even for interior work).
    pub(crate) op_id: OpId,
    pub(crate) session_id: SessionId,
    /// Turn-scoped cancellation (a CHILD of the logical turn's token): a
    /// user Stop during compaction cancels the summary stream instead of
    /// leaving the compaction model running to the deadline (P0 audit
    /// round 11). The wire request carries a child of this token.
    pub(crate) cancellation: CancellationToken,
    /// Stream bound; the production default is [`DEFAULT_SUMMARY_TIMEOUT`],
    /// tests inject a small value.
    pub(crate) summary_timeout: Duration,
    /// P0-2: the durable dispatch marker of this summarizer's budget
    /// reservation — written immediately BEFORE the provider request is
    /// sent, so crash recovery can tell "dispatch never provably began"
    /// (REFUNDED) from "the provider may have billed" (UNCERTAIN). `None` =
    /// unbudgeted (test graphs that never reserve).
    pub(crate) budget_marker: Option<BudgetDispatchMarker>,
    /// Audit item 3: the ACCEPTED summary as a typed
    /// [`crate::OutputTrust::ContextCompression`] [`ModelOutput`] (raw text
    /// + trust class + this call's durable op id), captured at the moment
    ///   the completion protocol accepts the text. The compaction site reads
    ///   it to record durable compaction provenance; it is never admitted to
    ///   a fact/evidence writer (those refuse this class typed).
    pub(crate) output_slot: Arc<std::sync::Mutex<Option<ModelOutput>>>,
}

impl StreamingSummarizer {
    /// Stream ONE compaction summary request and return the accepted text.
    ///
    /// Completion protocol (P0 audit round 11): text is accepted ONLY when
    /// the stream ended cleanly, tracked explicitly:
    ///   - `ProviderChunk::Done` marks Complete. FakeProvider's `End` chunk
    ///     maps to `ProviderChunk::Done` (its unfold always emits a
    ///     terminal Done before exhaustion), and every real transport
    ///     (anthropic/google/ollama adapters, guarded transport) signals a
    ///     successful end with Done — see their stream ends;
    ///   - plain exhaustion after content (`None` from the stream, no error,
    ///     no Done) ALSO marks Complete: a transport that ends without an
    ///     explicit Done chunk is a clean end, never a failure. (FakeProvider
    ///     never produces this shape, but the status logic must be correct
    ///     for both);
    ///   - a provider `Err` marks the run FAILED;
    ///   - the bounded deadline marks the run FAILED;
    ///   - turn cancellation marks the run FAILED.
    ///
    /// The size cap is a REFUSAL, never a completion: a chunk that would
    /// push the accumulated text past `SUMMARY_MAX_CHARS` marks the run
    /// oversized and it is refused exactly like a failed stream. The cap
    /// is checked BEFORE appending, so an oversized buffer is never
    /// constructed and a truncated prefix of a still-streaming summary is
    /// never accepted.
    ///
    /// Any status other than Complete (and any oversized run) discards
    /// EVERY accumulated character below and returns `None` — a truncated
    /// summary is small, so it would slip under the compactor's hard cap
    /// and replace the real history with a partial state transfer.
    pub(crate) async fn run(&self, history: &[faktor_context::RecentTurn]) -> Option<String> {
        use futures::StreamExt as _;
        const SUMMARY_MAX_CHARS: usize = 60_000;
        // Cancellation is polled at this cadence even while the stream is
        // silent (std CancellationToken has no async wait primitive; a
        // bounded tick mirrors the guarded transports' cancellation checks).
        const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
        // Capabilities decide: a non-streaming compaction model is skipped.
        if !self.provider.capabilities(&self.model).streaming {
            return None;
        }
        let request = GenericAgentRequest {
            model: self.model.clone(),
            system: COMPACTOR_SYSTEM.to_string(),
            messages: history
                .iter()
                .map(|t| RequestMessage {
                    role: if t.role == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    content: vec![ContentPart::text(&t.text)],
                })
                .collect(),
            tools: vec![],
            max_output: Some(4096),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: self.op_id,
                session_id: self.session_id,
                provider: self.provider.id().into(),
                attempt: 0,
                deadline_ms: self.summary_timeout.as_millis().min(u64::MAX as u128) as u64,
                // A CHILD of the turn-scoped token: cancellation of the
                // logical turn cascades into the wire request, and the
                // provider double/transport can observe it.
                cancellation: self.cancellation.child(),
            },
        };
        // P0-2: the durable dispatch marker is written immediately BEFORE
        // the provider request is sent. A crash after billing but before the
        // compaction settlement must recover as UNCERTAIN (the reserved
        // amount keeps consuming), never as a $0 refund — and a summarizer
        // that cannot be durably marked must not start its stream.
        if let Some(m) = &self.budget_marker {
            if let Err(e) = m.machine.lock().await.mark_dispatched().await {
                tracing::error!(
                    session = %self.session_id,
                    "cannot mark the compaction reservation dispatched: {e}"
                );
                return None;
            }
        }
        let mut stream = self.provider.stream(request);
        let mut text = String::new();
        let mut complete = false;
        // The size cap is NOT a completion: exceeding it refuses the whole
        // run (see the completion protocol above).
        let mut oversized = false;
        let deadline = tokio::time::timeout(self.summary_timeout, async {
            let mut cancel_ticks = tokio::time::interval(CANCEL_POLL_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancel_ticks.tick() => {
                        if self.cancellation.is_cancelled() {
                            // Turn cancelled: FAILED (complete stays false).
                            return;
                        }
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(ProviderChunk::Text { text: t }))
                            | Some(Ok(ProviderChunk::Reasoning { text: t })) => {
                                // Capacity is checked BEFORE appending so an
                                // oversized accepted buffer is never built.
                                if t.len() > SUMMARY_MAX_CHARS.saturating_sub(text.len()) {
                                    oversized = true;
                                    return;
                                }
                                text.push_str(&t);
                            }
                            Some(Ok(ProviderChunk::Done)) => {
                                // Clean end: the ONLY unconditional Complete.
                                complete = true;
                                return;
                            }
                            Some(Ok(_)) => {}
                            // Clean exhaustion after content: Complete (see
                            // the completion protocol above).
                            None => {
                                complete = true;
                                return;
                            }
                            // Provider failure: FAILED (complete stays false).
                            Some(Err(_)) => return,
                        }
                    }
                }
            }
        });
        // On timeout the inner future is dropped mid-stream with `complete`
        // still false: FAILED, every accumulated character discarded below.
        let _ = deadline.await;
        if !complete || oversized || text.is_empty() {
            return None;
        }
        // Audit item 3: capture the ACCEPTED summary as a typed
        // context-compression output (raw text + trust + durable call id)
        // so the compaction site can record its provenance. Nothing was
        // accepted unless the completion protocol above held, so the slot
        // is only ever filled by a clean end.
        if let Ok(mut slot) = self.output_slot.lock() {
            *slot = Some(ModelOutput::new(
                text.clone(),
                OutputTrust::ContextCompression,
                self.op_id.raw(),
            ));
        }
        Some(text)
    }
}

impl AgentRuntime {
    /// Live chunk broadcast (see [`ChunkEvent`]): bounded, best-effort — a
    /// missing sink is a no-op, and a slow consumer never blocks the turn:
    /// [`ChunkSink::try_send`] coalesces ephemeral deltas (drop-oldest,
    /// [`CHUNK_COALESCE_CAP_BYTES`] cap) instead of waiting for room.
    pub(crate) fn emit_chunk(
        &self,
        session_id: SessionId,
        message_id: Option<i64>,
        kind: &'static str,
        text: &str,
    ) {
        // Output evidence for the stall record: text/reasoning deltas are
        // output; tool announcements are progress events.
        if kind == "text" || kind == "reasoning" {
            self.progress_output(session_id);
        } else {
            self.progress_heartbeat(session_id);
        }
        if let Some(sink) = &self.deps.chunk_sink {
            sink.try_send(ChunkEvent {
                session_id,
                message_id,
                kind,
                text: text.to_string(),
            });
        }
    }

    /// UpdatingMemory phase (spec §8): durable structured facts, written on
    /// every genuine turn end. The ledger is the compact task projection; the
    /// memory facts carry the goal and per-turn summaries. Bounds live in the
    /// session layer (MAX_FACT_VALUE_BYTES) — truncation happens here first.
    pub(crate) fn record_memory(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        ledger: &TaskLedger,
        summary: &faktor_context::ledger::TurnSummary,
    ) -> faktor_core::Result<()> {
        if !ledger.goal.is_empty() {
            handle.upsert_memory_fact("task", "goal", &truncate(&ledger.goal, 200))?;
        }
        let empty = summary.steps_completed.is_empty()
            && summary.steps_opened.is_empty()
            && summary.decisions.is_empty()
            && summary.failures.is_empty()
            && summary.files_changed.is_empty()
            && summary.tests_run.is_empty()
            && summary.tests_failed.is_empty();
        if !empty {
            let rendered = serde_json::to_string(summary).unwrap_or_default();
            handle.upsert_memory_fact("turn", &op_id.to_string(), &truncate(&rendered, 3500))?;
        }
        Ok(())
    }
}
