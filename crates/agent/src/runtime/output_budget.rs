//! Faktor-side cumulative model-output enforcement.
//!
//! The wire request already carries a resolved `max_output`. That is only a
//! REQUEST: a provider (or an OpenAI-compatible endpoint fronting one) may
//! ignore it and keep streaming. The runtime's durable part flushes bound
//! resident memory per segment, but they do NOT bound the cumulative output
//! of one physical attempt — a hostile stream could flush forever.
//!
//! This module owns the per-ATTEMPT envelope and counter:
//!
//! - text tokens (conservative estimate over the cumulative character
//!   count, so a chunk boundary can never round the budget up or reset it),
//! - reasoning tokens (same estimate),
//! - tool-call payload cost (the accumulated JSON value plus id/name), and
//! - absolute bytes across all of the above.
//!
//! ## The safety factor
//!
//! The local hard ceiling is deliberately ABOVE the resolved wire cap:
//!
//! ```text
//! token_ceiling = ceil(max_output * 3 / 2) + 256
//! byte_ceiling  = max(token_ceiling * 8, 64 KiB)
//! ```
//!
//! `3/2` absorbs the difference between the provider's real tokenizer and
//! this crate's conservative chars/3 estimator (which is already stricter
//! than the chars/3.4 floor) plus per-message structural tokens; `256` is
//! headroom for short responses and stream envelopes. The byte ceiling is a
//! second, independent bound: 8 bytes per budgeted token covers multi-byte
//! UTF-8 (up to 4 bytes per character), JSON punctuation/keys and short
//! token payloads, while staying linear in the resolved envelope. Whichever
//! ceiling trips first is the violation.
//!
//! Exceeding either ceiling is a REFUSAL, never a completion: the attempt
//! is cancelled, the violating chunk is not admitted, and the caller ends
//! the turn with the typed [`ProviderOutputLimitExceeded`] failure. The
//! counter lives for exactly one physical attempt, so durable flush
//! boundaries inside that attempt cannot reset it.

use faktor_context::Estimator;
use faktor_provider::{ProviderError, ProviderErrorKind};

pub use faktor_provider::{
    OutputEnvelope, OUTPUT_LIMIT_BYTES_PER_TOKEN, OUTPUT_LIMIT_HEADROOM_TOKENS,
    OUTPUT_LIMIT_MIN_BYTES, OUTPUT_LIMIT_SAFETY_DENOMINATOR, OUTPUT_LIMIT_SAFETY_NUMERATOR,
};

/// Explicit, user-visible marker appended to the durable assistant message
/// when a stream is cut off by the Faktor-side ceiling. Already-emitted
/// text is preserved; this marker makes the truncation unambiguous instead
/// of presenting a partial answer as complete.
pub const OUTPUT_LIMIT_INCOMPLETE_MARKER: &str =
    "[incomplete: provider output limit exceeded; response truncated]";

/// Which cumulative dimension tripped the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLimitKind {
    TextTokens,
    ReasoningTokens,
    ToolPayloadTokens,
    TextBytes,
    ReasoningBytes,
    ToolPayloadBytes,
}

impl OutputLimitKind {
    pub fn label(self) -> &'static str {
        match self {
            OutputLimitKind::TextTokens => "text tokens",
            OutputLimitKind::ReasoningTokens => "reasoning tokens",
            OutputLimitKind::ToolPayloadTokens => "tool-call payload tokens",
            OutputLimitKind::TextBytes => "text bytes",
            OutputLimitKind::ReasoningBytes => "reasoning bytes",
            OutputLimitKind::ToolPayloadBytes => "tool-call payload bytes",
        }
    }
}

/// The typed local refusal: the attempt's cumulative output exceeded the
/// envelope derived from the resolved request. `observed_*` describe the
/// would-be totals including the rejected chunk; the rejected chunk itself
/// is never admitted to the durable message or the pending tool batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOutputLimitExceeded {
    pub kind: OutputLimitKind,
    pub observed_tokens: u64,
    pub observed_bytes: u64,
    pub envelope: OutputEnvelope,
}

impl ProviderOutputLimitExceeded {
    /// Bounded human/machine detail used by the stop reason, the journaled
    /// provider-call row and the session health record (all fields are
    /// numbers or fixed labels — no provider text rides here).
    pub fn detail(&self) -> String {
        format!(
            "provider output limit exceeded ({}): cumulative {} tokens / {} bytes would \
             exceed the attempt envelope of {} tokens / {} bytes (resolved max_output={})",
            self.kind.label(),
            self.observed_tokens,
            self.observed_bytes,
            self.envelope.token_ceiling,
            self.envelope.byte_ceiling,
            self.envelope.resolved_max_output,
        )
    }

    /// The typed provider error the agent journals and surfaces.
    pub fn into_provider_error(self) -> ProviderError {
        ProviderError::new(ProviderErrorKind::OutputLimitExceeded, self.detail())
    }
}

/// Cumulative per-attempt output counter. Counters are monotonic for the
/// life of the attempt; durable part flushes happen elsewhere and never
/// touch this state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBudget {
    envelope: OutputEnvelope,
    text_chars: u64,
    reasoning_chars: u64,
    tool_tokens: u64,
    text_bytes: u64,
    reasoning_bytes: u64,
    tool_bytes: u64,
}

impl OutputBudget {
    pub fn new(envelope: OutputEnvelope) -> Self {
        Self {
            envelope,
            text_chars: 0,
            reasoning_chars: 0,
            tool_tokens: 0,
            text_bytes: 0,
            reasoning_bytes: 0,
            tool_bytes: 0,
        }
    }

    pub fn envelope(&self) -> &OutputEnvelope {
        &self.envelope
    }

    pub fn tokens(&self) -> u64 {
        token_estimate(self.text_chars)
            .saturating_add(token_estimate(self.reasoning_chars))
            .saturating_add(self.tool_tokens)
    }

    pub fn bytes(&self) -> u64 {
        self.text_bytes
            .saturating_add(self.reasoning_bytes)
            .saturating_add(self.tool_bytes)
    }

    /// Account one accepted text delta. The estimate is computed over the
    /// CUMULATIVE character count, so splitting the same text across any
    /// number of chunks yields the same (conservative) token count.
    pub fn record_text(&mut self, text: &str) -> Result<(), ProviderOutputLimitExceeded> {
        let next_chars = self.text_chars.saturating_add(chars_u64(text));
        let next_bytes = self.text_bytes.saturating_add(text.len() as u64);
        let (tokens, bytes) = (
            token_estimate(next_chars)
                .saturating_add(token_estimate(self.reasoning_chars))
                .saturating_add(self.tool_tokens),
            next_bytes
                .saturating_add(self.reasoning_bytes)
                .saturating_add(self.tool_bytes),
        );
        self.check(
            tokens,
            bytes,
            OutputLimitKind::TextTokens,
            OutputLimitKind::TextBytes,
        )?;
        self.text_chars = next_chars;
        self.text_bytes = next_bytes;
        Ok(())
    }

    /// Account one accepted reasoning delta (same cumulative estimator).
    pub fn record_reasoning(&mut self, text: &str) -> Result<(), ProviderOutputLimitExceeded> {
        let next_chars = self.reasoning_chars.saturating_add(chars_u64(text));
        let next_bytes = self.reasoning_bytes.saturating_add(text.len() as u64);
        let (tokens, bytes) = (
            token_estimate(self.text_chars)
                .saturating_add(token_estimate(next_chars))
                .saturating_add(self.tool_tokens),
            self.text_bytes
                .saturating_add(next_bytes)
                .saturating_add(self.tool_bytes),
        );
        self.check(
            tokens,
            bytes,
            OutputLimitKind::ReasoningTokens,
            OutputLimitKind::ReasoningBytes,
        )?;
        self.reasoning_chars = next_chars;
        self.reasoning_bytes = next_bytes;
        Ok(())
    }

    /// Account one complete tool-call payload. `serialized_bytes` is the
    /// byte length of the JSON arguments (the caller already owns the
    /// serialized value it will persist); id/name bytes are included too.
    pub fn record_tool_call(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        serialized_bytes: u64,
    ) -> Result<(), ProviderOutputLimitExceeded> {
        let next_tool_tokens = self
            .tool_tokens
            .saturating_add(u64::try_from(Estimator.estimate_json(input)).unwrap_or(u64::MAX))
            .saturating_add(token_estimate(chars_u64(id)))
            .saturating_add(token_estimate(chars_u64(name)));
        let next_tool_bytes = self
            .tool_bytes
            .saturating_add(serialized_bytes)
            .saturating_add(id.len() as u64)
            .saturating_add(name.len() as u64);
        let tokens = token_estimate(self.text_chars)
            .saturating_add(token_estimate(self.reasoning_chars))
            .saturating_add(next_tool_tokens);
        let bytes = self
            .text_bytes
            .saturating_add(self.reasoning_bytes)
            .saturating_add(next_tool_bytes);
        self.check(
            tokens,
            bytes,
            OutputLimitKind::ToolPayloadTokens,
            OutputLimitKind::ToolPayloadBytes,
        )?;
        self.tool_tokens = next_tool_tokens;
        self.tool_bytes = next_tool_bytes;
        Ok(())
    }

    fn check(
        &self,
        tokens: u64,
        bytes: u64,
        token_kind: OutputLimitKind,
        byte_kind: OutputLimitKind,
    ) -> Result<(), ProviderOutputLimitExceeded> {
        if tokens > self.envelope.token_ceiling || bytes > self.envelope.byte_ceiling {
            let kind = if bytes > self.envelope.byte_ceiling {
                byte_kind
            } else {
                token_kind
            };
            return Err(ProviderOutputLimitExceeded {
                kind,
                observed_tokens: tokens,
                observed_bytes: bytes,
                envelope: self.envelope,
            });
        }
        Ok(())
    }
}

fn chars_u64(text: &str) -> u64 {
    u64::try_from(text.chars().count()).unwrap_or(u64::MAX)
}

/// The same conservative `Estimator` formula the wire planner uses, applied
/// to a cumulative character count (never to a single chunk).
fn token_estimate(chars: u64) -> u64 {
    let chars = usize::try_from(chars).unwrap_or(usize::MAX);
    u64::try_from(Estimator.estimate_chars(chars)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_scales_the_resolved_cap_with_headroom() {
        let env = OutputEnvelope::from_max_output(128);
        assert_eq!(env.resolved_max_output, 128);
        assert_eq!(env.token_ceiling, 192 + 256);
        assert_eq!(env.byte_ceiling, OUTPUT_LIMIT_MIN_BYTES);
        assert!(OutputEnvelope::from_max_output(0).token_ceiling > 0);
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_cumulative_estimate() {
        let env = OutputEnvelope::from_max_output(8_192);
        let text = "x".repeat(30_000);
        let mut whole = OutputBudget::new(env);
        whole.record_text(&text).expect("one chunk fits");
        let mut split = OutputBudget::new(env);
        for chunk in text.as_bytes().chunks(7) {
            split
                .record_text(std::str::from_utf8(chunk).unwrap())
                .expect("split chunks fit");
        }
        assert_eq!(
            whole.tokens(),
            split.tokens(),
            "the estimate is over cumulative characters, not chunk count"
        );
        assert_eq!(whole.bytes(), split.bytes());
    }

    #[test]
    fn a_rejected_chunk_never_advances_the_counter() {
        let env = OutputEnvelope::from_max_output(2);
        let mut budget = OutputBudget::new(env);
        budget.record_text("ab").expect("fits");
        let before = (budget.tokens(), budget.bytes());
        let err = budget
            .record_text(&"z".repeat(4_096))
            .expect_err("the cumulative ceiling must refuse");
        assert_eq!(
            (budget.tokens(), budget.bytes()),
            before,
            "a refused chunk is not admitted"
        );
        assert_eq!(err.kind, OutputLimitKind::TextTokens);
        assert!(err.detail().contains("resolved max_output=2"));
    }

    #[test]
    fn later_text_cannot_ignore_tool_payload_already_spent() {
        let env = OutputEnvelope::from_max_output(64);
        let mut budget = OutputBudget::new(env);
        let payload = serde_json::json!({ "a": "x".repeat(900) });
        let serialized = serde_json::to_string(&payload).unwrap();
        budget
            .record_tool_call("id", "tool", &payload, serialized.len() as u64)
            .expect("the tool payload alone fits");
        // The same text alone would fit; only the combined attempt total
        // crosses, so the cumulative counter must still refuse it.
        let err = budget
            .record_text(&"y".repeat(200))
            .expect_err("tool payload + text must share one envelope");
        assert_eq!(err.kind, OutputLimitKind::TextTokens);
        assert_eq!(budget.tokens(), 306, "the refused text is not admitted");
    }

    #[test]
    fn reasoning_cannot_ignore_tool_payload_already_spent() {
        let env = OutputEnvelope::from_max_output(64);
        let mut budget = OutputBudget::new(env);
        let payload = serde_json::json!({ "a": "x".repeat(900) });
        let serialized = serde_json::to_string(&payload).unwrap();
        budget
            .record_tool_call("id", "tool", &payload, serialized.len() as u64)
            .expect("the tool payload alone fits");
        let err = budget
            .record_reasoning(&"z".repeat(200))
            .expect_err("tool payload + reasoning must share one envelope");
        assert_eq!(err.kind, OutputLimitKind::ReasoningTokens);
        assert_eq!(
            budget.tokens(),
            306,
            "the refused reasoning is not admitted"
        );
    }

    #[test]
    fn multi_byte_text_trips_the_absolute_byte_ceiling_first() {
        let env = OutputEnvelope::from_max_output(8_192);
        let mut budget = OutputBudget::new(env);
        // 26k characters fit the token ceiling (~8.7k estimated tokens) but
        // their 104 KB UTF-8 encoding exceeds the independent byte ceiling.
        let err = budget
            .record_text(&"😀".repeat(26_000))
            .expect_err("the absolute byte ceiling is independent");
        assert_eq!(err.kind, OutputLimitKind::TextBytes);
        assert!(err.observed_tokens <= env.token_ceiling);
        assert!(err.observed_bytes > env.byte_ceiling);
        assert_eq!(budget.bytes(), 0, "the refused chunk is not admitted");
    }

    #[test]
    fn bytes_and_tool_payload_are_tracked_independently() {
        let env = OutputEnvelope::from_max_output(1_000);
        let mut budget = OutputBudget::new(env);
        let payload = serde_json::json!({ "blob": "x".repeat(2_000_000) });
        let serialized = serde_json::to_string(&payload).unwrap();
        let err = budget
            .record_tool_call("call_1", "write_file", &payload, serialized.len() as u64)
            .expect_err("a huge tool payload must trip the byte ceiling");
        assert_eq!(err.kind, OutputLimitKind::ToolPayloadBytes);
        assert_eq!(budget.tokens(), 0);
        assert_eq!(budget.bytes(), 0);
    }
}
