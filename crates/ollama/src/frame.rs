//! Native NDJSON frame parsing, split from the adapter so `lib.rs` stays
//! under the source-size ceiling.

use faktor_provider::{CanonicalUsage, ProviderChunk};

/// One native NDJSON frame -> 0..n chunks, in the order they belong to the
/// conversation: `message.thinking` (a Reasoning chunk) before `content`
/// (a Text chunk), then one ToolCall chunk per `tool_calls` entry in array
/// order. Tool ids: a provider-supplied `id` wins; otherwise
/// `ollama:<response_seq>:<tool_index>` — never a synthesized
/// `ollama_call_<name>` (the old ids collided across calls and responses).
/// The final `done: true` frame also carries the token counters
/// (`prompt_eval_count` / `eval_count`), which become a canonical
/// [`ProviderChunk::Usage`] frame emitted LAST (before the stream's Done) —
/// see the usage mapping notes at the bottom of this function.
pub(crate) fn parse_ollama_frame(
    value: &serde_json::Value,
    response_seq: u64,
) -> Vec<ProviderChunk> {
    let mut chunks: Vec<ProviderChunk> = Vec::new();
    if let Some(msg) = value.get("message") {
        if let Some(thinking) = msg.get("thinking").and_then(|t| t.as_str()) {
            if !thinking.is_empty() {
                chunks.push(ProviderChunk::Reasoning {
                    text: thinking.to_string(),
                });
            }
        }
        if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                chunks.push(ProviderChunk::Text {
                    text: text.to_string(),
                });
            }
        }
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
            for (idx, tc) in tool_calls.iter().enumerate() {
                let function = tc.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let args = function
                    .and_then(|f| f.get("arguments"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|i| !i.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("ollama:{response_seq}:{idx}"));
                chunks.push(ProviderChunk::ToolCall {
                    id,
                    name: name.to_string(),
                    input: args,
                    complete: true,
                });
            }
        }
    }
    // Usage mapping (audit Phase-1 item C): the native /api/chat final
    // frame reports `prompt_eval_count` (the TOTAL prompt tokens: cached
    // prefix + newly evaluated remainder) and `eval_count` (generated
    // tokens; thinking tokens ARE part of the generation, so
    // `output_tokens` already contains them and the informational
    // `reasoning_tokens` subset stays zero — the API reports no separate
    // thinking count, so reasoning can never be double-billed).
    //
    // Ollama 0.40+ also reports `prompt_eval_cached_count`: the prefix the
    // runtime served from its KV cache. Mapping the total as uncached (the
    // old OptionalCacheDetail assumption) overstates uncached input by the whole
    // cached prefix and reports a provably-false zero cache read — which
    // then makes the runtime's prefix-cache classifier believe every turn
    // is cold. The canonical split is therefore:
    //   cache_read = prompt_eval_cached_count
    //   uncached   = prompt_eval_count - prompt_eval_cached_count
    // Older runtimes omit the field (cache_read = 0, uncached = total —
    // the conservative historical behavior). A hostile cached>total row is
    // not evidence of a cache hit: the field is ignored (uncached = total,
    // cache_read = 0), never used to subtract into a negative/wrapped
    // count. Counts are integers; a hostile wrong-typed/negative counter is
    // ignored (0) and yields no frame — unknown fields never panic.
    let prompt_eval_count = value
        .get("prompt_eval_count")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let eval_count = value
        .get("eval_count")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let reported_cached = value
        .get("prompt_eval_cached_count")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = if reported_cached <= prompt_eval_count {
        reported_cached
    } else {
        0
    };
    if prompt_eval_count > 0 || eval_count > 0 {
        let usage = CanonicalUsage {
            uncached_input_tokens: prompt_eval_count.saturating_sub(cache_read_tokens),
            cache_read_tokens,
            cache_write_tokens: 0,
            output_tokens: eval_count,
            reasoning_tokens: 0,
            reported_cost: None,
            request_id: None,
        };
        // Contract with the stream loop: the usage frame must come BEFORE
        // the synthetic Done, so it lands at the END of this frame's chunk
        // list (after any reasoning/text/tool chunks of the same frame).
        chunks.push(ProviderChunk::Usage(usage));
    }
    chunks
}
