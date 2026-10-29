// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Live model output: the daemon-owned bounded broadcaster and the
//! session-scoped subscription it feeds.
//!
//! The agent's chunk producer used to be DRAINED (every live token
//! discarded) while the IDE reread saved message pages at the 8 KiB
//! durable-flush cadence — text could never be fluid. This module replaces
//! the discard:
//!
//! * [`LiveOutputHub`] consumes the SAME chunk stream (one consumer, fed by
//!   `api::lifecycle::drain_chunk_stream`) and republishes it as bounded
//!   live frames. The durable journal + message parts stay the recovery
//!   authority: there are NO per-token durable writes and the storage
//!   governor is untouched.
//! * `GET /native/session/{id}/live/output` is an authenticated
//!   session-scoped SSE subscription. The FIRST frame is always a
//!   `snapshot` with the exact starting watermark (subscribe-before-submit:
//!   a fast reply can never outrun a listener that connected first).
//! * `GET /native/session/{id}/live/snapshot` is the authoritative part
//!   snapshot a client fetches after an explicit `resync` signal.
//!
//! ## Offset boundary rule (Rust byte offsets vs. JS UTF-16 indices)
//!
//! Every `offset` on the wire is a **UTF-8 byte offset** into the logical
//! part text (the concatenation of the durable `text`/`reasoning` parts of
//! one `(message, kind)`), and every `content` delta consists of whole code
//! points (it is produced as a Rust `String`, never a mid-char byte slice).
//! A JS client therefore never converts byte offsets to UTF-16 indices: it
//! appends `content` and advances a running byte counter by the UTF-8 byte
//! length of that content alone. For well-formed text a UTF-16 `startsWith`
//! test is equivalent to a UTF-8 prefix test (both cut at code-point
//! boundaries), which is exactly how snapshot reconciliation compares live
//! and saved text.
//!
//! ## Bounds
//!
//! * per-subscriber queue: a bounded broadcast ring
//!   ([`LIVE_BROADCAST_CAPACITY`] frames); a lagging receiver gets `Lagged`
//!   and the stream emits an explicit `resync` signal — never silent loss.
//! * part lineage slots: [`MAX_TRACKED_PARTS`] (LRU-evicted).
//! * live text memory: [`MAX_LIVE_TEXT_BYTES_TOTAL`] global /
//!   [`MAX_LIVE_TEXT_BYTES_PART`] per part; the durable path is never
//!   affected (the cache exists only to make live tails and resyncs fluid).

use super::*;
use crate::api::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_agent::ChunkEvent;
use faktor_core::id::SessionId;
use faktor_session::SessionHandle;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Per-subscriber bounded queue (frames). A receiver that falls further
/// behind than this ring is told to RESYNC instead of being silently
/// truncated: the lag IS the signal.
pub(crate) const LIVE_BROADCAST_CAPACITY: usize = 256;
/// Hard cap of tracked (session, message, kind) part lineages (counters).
/// Text memory is dropped first (see [`MAX_LIVE_TEXT_BYTES_TOTAL`]); a slot
/// eviction past this cap restarts a lineage and is recovered by the next
/// snapshot (attempt bump + durable authority).
pub(crate) const MAX_TRACKED_PARTS: usize = 4096;
/// Global cap of cached live part text (bytes). The durable message parts
/// are the authority; this cache only makes live tails and resyncs fluid.
pub(crate) const MAX_LIVE_TEXT_BYTES_TOTAL: usize = 8 * 1024 * 1024;
/// Per-part cap of cached live part text (bytes); past it the live tail is
/// dropped (the next durable flush covers it — snapshots fall back to the
/// durable authority until then).
pub(crate) const MAX_LIVE_TEXT_BYTES_PART: usize = 2 * 1024 * 1024;
/// Parts carried in one snapshot (bounded everything).
pub(crate) const MAX_SUBSCRIPTION_PARTS: usize = 32;
/// Turn-status fallback poll while a turn is active (the journal wake is
/// the primary signal; this is the bounded safety net).
pub(crate) const LIVE_TURN_POLL_MS: u64 = 500;

/// One part of the live lineage: `(session, message id, kind)`.
type PartKey = (SessionId, i64, &'static str);

/// A published live frame (content is shared across receivers: the
/// broadcast ring allocates the delta ONCE no matter the subscriber count).
#[derive(Clone, Debug)]
pub(crate) struct LiveFrame {
    pub instance: Arc<str>,
    pub session: SessionId,
    pub message_id: i64,
    pub kind: &'static str,
    /// Lineage epoch of this part in the hub (bumps on a rebase; a client
    /// that sees a different attempt than its snapshot must resync).
    pub attempt: u32,
    /// Hub-global monotonic frame sequence (the subscribe watermark).
    pub sequence: u64,
    /// Per-part publish counter (revision of the live lineage).
    pub revision: u64,
    /// UTF-8 byte offset where `content` begins in the logical part.
    pub offset: u64,
    pub content: Arc<str>,
}

/// Tracked live lineage of one logical part.
#[derive(Debug)]
struct PartEntry {
    /// Absolute UTF-8 bytes published so far (the next frame's offset).
    next_offset: u64,
    /// Cached live text covering `[text_base, next_offset)`; `None` after a
    /// memory-bound drop (snapshots then fall back to the durable authority
    /// and the next publish restarts the cache at the then-current offset).
    text: Option<String>,
    /// Absolute offset of `text[0]`.
    text_base: u64,
    /// Per-part publish counter.
    revision: u64,
    /// Lineage epoch (see [`LiveFrame::attempt`]).
    attempt: u32,
    /// Last hub sequence that touched this part (LRU order).
    last_sequence: u64,
}

impl PartEntry {
    fn fresh() -> Self {
        Self {
            next_offset: 0,
            text: None,
            text_base: 0,
            revision: 0,
            attempt: 0,
            last_sequence: 0,
        }
    }
}

#[derive(Debug)]
struct PartTables {
    map: HashMap<PartKey, PartEntry>,
    /// Sum of cached live text bytes across all entries.
    live_text_bytes: usize,
}

impl PartTables {
    /// Drop OTHER parts' cached live text (LRU) until `extra` more bytes
    /// fit. Takes the split fields so a live `entry` borrow on `map` never
    /// conflicts (a `MutexGuard` deref would borrow everything at once).
    fn evict_text_for(
        map: &mut HashMap<PartKey, PartEntry>,
        live_text_bytes: &mut usize,
        keep: &PartKey,
        extra: usize,
    ) -> bool {
        while *live_text_bytes + extra > MAX_LIVE_TEXT_BYTES_TOTAL {
            let victim = map
                .iter()
                .filter(|(key, entry)| *key != keep && entry.text.is_some())
                .min_by_key(|(_, entry)| entry.last_sequence)
                .map(|(key, _)| *key);
            match victim {
                Some(key) => {
                    if let Some(entry) = map.get_mut(&key) {
                        if let Some(old) = entry.text.take() {
                            *live_text_bytes = live_text_bytes.saturating_sub(old.len());
                        }
                    }
                }
                None => return false,
            }
        }
        true
    }

    /// Drop the least-recently-published LINEAGE slot (called only past
    /// [`MAX_TRACKED_PARTS`]).
    fn evict_slots_lru(map: &mut HashMap<PartKey, PartEntry>, live_text_bytes: &mut usize) {
        while map.len() > MAX_TRACKED_PARTS {
            let victim = map
                .iter()
                .min_by_key(|(_, entry)| entry.last_sequence)
                .map(|(key, _)| *key);
            match victim {
                Some(key) => {
                    if let Some(entry) = map.remove(&key) {
                        if let Some(old) = entry.text {
                            *live_text_bytes = live_text_bytes.saturating_sub(old.len());
                        }
                    }
                }
                None => break,
            }
        }
    }
}

/// The daemon-owned live-output broadcaster fed by the SAME chunk producer
/// the agent streams to. One instance per [`crate::api::ServerDeps`]; a
/// daemon restart constructs a new hub with a NEW server-instance id, so a
/// client can never mix two processes' sequence spaces.
#[derive(Debug)]
pub struct LiveOutputHub {
    /// Server-instance id: random per hub construction. Present on every
    /// snapshot and frame; a client that observes a changed id treats its
    /// sequences as a foreign lineage and resyncs.
    instance: Arc<str>,
    sequence: std::sync::atomic::AtomicU64,
    tx: broadcast::Sender<LiveFrame>,
    parts: Mutex<PartTables>,
}

impl Default for LiveOutputHub {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveOutputHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(LIVE_BROADCAST_CAPACITY);
        Self {
            instance: Arc::from(random_instance_id()),
            sequence: std::sync::atomic::AtomicU64::new(0),
            tx,
            parts: Mutex::new(PartTables {
                map: HashMap::new(),
                live_text_bytes: 0,
            }),
        }
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The exact sequence watermark: every frame published after this call
    /// carries a strictly greater sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Consume one agent chunk: assign offsets/sequence/revision, cache the
    /// live tail (bounded) and broadcast the frame. Frames without a message
    /// id cannot be attached to a transcript part and are dropped here (the
    /// durable path is unaffected — the agent journals and flushes its own
    /// text buffers regardless of any subscriber).
    pub fn publish(&self, event: &ChunkEvent) {
        if event.text.is_empty() {
            return;
        }
        let Some(message_id) = event.message_id else {
            return;
        };
        let accumulating = matches!(event.kind, "text" | "reasoning");
        let key: PartKey = (event.session_id, message_id, event.kind);
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let mut tables = match self.parts.lock() {
            Ok(t) => t,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Split the guard's fields so the `entry` borrow below and the live
        // byte accounting never conflict on one deref borrow.
        let PartTables {
            map,
            live_text_bytes,
        } = &mut *tables;
        if accumulating && *live_text_bytes + event.text.len() > MAX_LIVE_TEXT_BYTES_TOTAL {
            PartTables::evict_text_for(map, live_text_bytes, &key, event.text.len());
        }
        let entry = map.entry(key).or_insert_with(PartEntry::fresh);
        let offset = if accumulating { entry.next_offset } else { 0 };
        if accumulating {
            if entry.text.is_none() {
                // (Re)start the cache from the current offset: `text_base`
                // keeps every later snapshot composable with the durable
                // prefix (see `compose_live_text`).
                entry.text_base = entry.next_offset;
                entry.text = Some(String::new());
            }
            let fits = entry
                .text
                .as_ref()
                .is_some_and(|t| t.len() + event.text.len() <= MAX_LIVE_TEXT_BYTES_PART)
                && *live_text_bytes + event.text.len() <= MAX_LIVE_TEXT_BYTES_TOTAL;
            if fits {
                if let Some(text) = entry.text.as_mut() {
                    text.push_str(&event.text);
                }
                *live_text_bytes += event.text.len();
            } else if let Some(old) = entry.text.take() {
                // Per-part bound: free the tail cache; the offset lineage
                // (`next_offset`) is untouched, so live frames stay fluid
                // and byte-exact while snapshots fall back to durable.
                *live_text_bytes = live_text_bytes.saturating_sub(old.len());
            }
            entry.next_offset += event.text.len() as u64;
        }
        entry.revision += 1;
        entry.last_sequence = sequence;
        let (revision, attempt, next_offset) = (entry.revision, entry.attempt, entry.next_offset);
        debug_assert!(
            !accumulating || offset + event.text.len() as u64 == next_offset,
            "live offsets stay contiguous"
        );
        if map.len() > MAX_TRACKED_PARTS {
            PartTables::evict_slots_lru(map, live_text_bytes);
        }
        drop(tables);
        let frame = LiveFrame {
            instance: Arc::clone(&self.instance),
            session: event.session_id,
            message_id,
            kind: event.kind,
            attempt,
            sequence,
            revision,
            offset,
            content: Arc::from(event.text.as_str()),
        };
        // No subscriber: the bounded ring drops it (no unbounded growth).
        let _ = self.tx.send(frame);
    }

    /// Subscribe to one session's live frames. The receiver is created
    /// UNDER the parts lock, so every frame published BEFORE the call is
    /// reflected in the returned briefs/watermark and every frame AFTER is
    /// delivered to the receiver — a fast reply can never outrun a listener
    /// that subscribed first.
    pub(crate) fn subscribe(
        &self,
        session: SessionId,
    ) -> (broadcast::Receiver<LiveFrame>, Vec<PartBrief>, u64) {
        let rx = self.tx.subscribe();
        let watermark = self.sequence();
        let tables = match self.parts.lock() {
            Ok(t) => t,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut briefs: Vec<PartBrief> = tables
            .map
            .iter()
            .filter(|((sid, _, _), _)| *sid == session)
            .map(|((_, message_id, kind), entry)| PartBrief {
                message_id: *message_id,
                kind,
                next_offset: entry.next_offset,
            })
            .collect();
        briefs.sort_by_key(|brief| std::cmp::Reverse(brief.next_offset));
        briefs.truncate(MAX_SUBSCRIPTION_PARTS);
        (rx, briefs, watermark)
    }

    /// The cached live tail of one part, if the hub still holds it.
    /// Returns `(text, text_base, next_offset, revision, attempt)`.
    pub(crate) fn live_text(
        &self,
        session: SessionId,
        message_id: i64,
        kind: &'static str,
    ) -> Option<(String, u64, u64, u64, u32)> {
        let tables = match self.parts.lock() {
            Ok(t) => t,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = tables.map.get(&(session, message_id, kind))?;
        let text = entry.text.clone()?;
        Some((
            text,
            entry.text_base,
            entry.next_offset,
            entry.revision,
            entry.attempt,
        ))
    }

    /// Current lineage counters of one part without the text (bounded
    /// snapshot metadata for parts whose text cache was dropped).
    pub(crate) fn part_lineage(
        &self,
        session: SessionId,
        message_id: i64,
        kind: &'static str,
    ) -> Option<(u64, u64, u32)> {
        let tables = match self.parts.lock() {
            Ok(t) => t,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = tables.map.get(&(session, message_id, kind))?;
        Some((entry.next_offset, entry.revision, entry.attempt))
    }

    /// Rebase one part's live lineage onto the authoritative composed text
    /// a snapshot served. The NORMAL case (live text is a consistent
    /// extension of the durable prefix) is an exact-equality no-op. Any
    /// other case (memory-dropped tail, coalescing gaps) resets the offset
    /// lineage to the composed length and bumps `attempt`, so every
    /// subscriber resyncs EXACTLY ONCE and the next frame's offset matches
    /// the text it adopted — never a resync loop.
    pub(crate) fn rebase(
        &self,
        session: SessionId,
        message_id: i64,
        kind: &'static str,
        composed: &str,
    ) {
        let mut tables = match self.parts.lock() {
            Ok(t) => t,
            Err(poisoned) => poisoned.into_inner(),
        };
        let PartTables {
            map,
            live_text_bytes,
        } = &mut *tables;
        let Some(entry) = map.get_mut(&(session, message_id, kind)) else {
            return;
        };
        if entry.text.as_deref() == Some(composed) {
            return;
        }
        if let Some(old) = entry.text.take() {
            *live_text_bytes = live_text_bytes.saturating_sub(old.len());
        }
        entry.text = Some(composed.to_string());
        entry.text_base = 0;
        *live_text_bytes += composed.len();
        if entry.next_offset != composed.len() as u64 {
            entry.next_offset = composed.len() as u64;
            entry.attempt = entry.attempt.saturating_add(1);
        }
    }
}

/// Bounded lineage brief of one part (subscribe handshake input).
#[derive(Debug, Clone)]
pub(crate) struct PartBrief {
    pub message_id: i64,
    pub kind: &'static str,
    pub next_offset: u64,
}

/// A random server-instance id (non-secret identity; restarts differ).
fn random_instance_id() -> String {
    use rand::Rng;
    let bytes: [u8; 8] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compose the authoritative part text from the hub's live tail and the
/// durable parts. Returns `(composed, hub_was_consistent)`.
///
/// * live base 0 and live extends the durable prefix → the live text (the
///   normal case: durable flushes lag the stream by less than one flush).
/// * durable extends the live text (hub behind after a memory drop) →
///   the durable text.
/// * live base inside the durable range → durable prefix + the live suffix
///   beyond the durable end (exact byte composition; a non-boundary cut
///   falls back to the durable authority rather than slicing mid-char).
/// * anything else (divergence) → the durable text (the recovery
///   authority wins; `rebase` realigns the lineage once).
pub(crate) fn compose_live_text(live: Option<(&str, u64)>, durable: &str) -> (String, bool) {
    let Some((live_text, live_base)) = live else {
        return (durable.to_string(), false);
    };
    let d_len = durable.len() as u64;
    if live_base == 0 {
        if live_text.starts_with(durable) {
            return (live_text.to_string(), true);
        }
        if durable.starts_with(live_text) {
            return (durable.to_string(), false);
        }
        return (durable.to_string(), false);
    }
    if live_base > d_len {
        // The live tail starts beyond the durable end: the bytes between
        // are covered by neither side — the durable authority stands until
        // its next flush crosses the gap.
        return (durable.to_string(), false);
    }
    let skip = (d_len - live_base) as usize;
    match live_text.get(skip..) {
        Some(suffix) => {
            let mut out = String::with_capacity(durable.len() + suffix.len());
            out.push_str(durable);
            out.push_str(suffix);
            (out, true)
        }
        None => (durable.to_string(), false),
    }
}

/// Serialize `value` as one SSE frame into a fresh buffer.
fn sse_frame(event: &str, value: &serde_json::Value) -> bytes::Bytes {
    let data = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut buf = bytes::BytesMut::with_capacity(data.len() + event.len() + 32);
    buf.extend_from_slice(b"event: ");
    buf.extend_from_slice(event.as_bytes());
    buf.extend_from_slice(b"\ndata: ");
    buf.extend_from_slice(&data);
    buf.extend_from_slice(b"\n\n");
    buf.freeze()
}

/// One part in a snapshot payload.
fn part_snapshot_json(
    handle: &SessionHandle,
    hub: &LiveOutputHub,
    message_id: i64,
    kind: &'static str,
    turn: Option<String>,
) -> Result<serde_json::Value, Box<Response>> {
    // Durable authority: the persisted parts of this kind, concatenated in
    // row order (the message-part flush order IS the text order).
    let rows = handle
        .parts_of(message_id)
        .map_err(|e| Box::new(api_err(&e)))?;
    let mut durable = String::new();
    let mut durable_revision = 0i64;
    for row in &rows {
        if row.kind != kind {
            continue;
        }
        if row.id > durable_revision {
            durable_revision = row.id;
        }
        if let Some(text) = row.data.get("text").and_then(|v| v.as_str()) {
            durable.push_str(text);
        }
    }
    let live = hub.live_text(handle.id(), message_id, kind);
    let live_ref = live.as_ref().map(|(text, base, ..)| (text.as_str(), *base));
    let (composed, consistent) = compose_live_text(live_ref, &durable);
    if !consistent {
        // One realignment (an exact-equality no-op in the normal case).
        hub.rebase(handle.id(), message_id, kind, &composed);
    }
    let durable_bytes = durable.len() as u64;
    let (next_offset, revision, attempt) = match hub.part_lineage(handle.id(), message_id, kind) {
        Some((next_offset, revision, attempt)) => (next_offset, revision, attempt),
        None => (composed.len() as u64, 0, 0),
    };
    // After a rebase the lineage restarts AT the composed length; without a
    // lineage (never seen live) the composed (durable) length is exactly
    // the next expected frame offset.
    let offset = if consistent {
        next_offset.max(composed.len() as u64)
    } else {
        composed.len() as u64
    };
    Ok(serde_json::json!({
        "partId": format!("{message_id}:{kind}"),
        "messageId": message_id,
        "kind": kind,
        "turn": turn,
        "attempt": attempt,
        "revision": revision,
        "offset": offset,
        "text": composed,
        "durableBytes": durable_bytes,
        "durableRevision": durable_revision,
        "liveTail": composed.len() as u64 > durable_bytes,
    }))
}

/// The bounded active-turn projection every snapshot carries: the durable
/// turn record (never inferred from a quiet stream or an HTTP 2xx).
fn turn_projection(handle: &SessionHandle) -> Result<serde_json::Value, Box<Response>> {
    if let Some(active) = handle
        .active_turn_record()
        .map_err(|e| Box::new(api_err(&e)))?
    {
        return Ok(serde_json::json!({
            "op": active.turn_op_id.to_string(),
            "status": "active",
        }));
    }
    if let Some(latest) = handle
        .latest_turn_record()
        .map_err(|e| Box::new(api_err(&e)))?
    {
        return Ok(serde_json::json!({
            "op": latest.turn_op_id.to_string(),
            "status": latest.status,
        }));
    }
    Ok(serde_json::Value::Null)
}

/// Current turn projection as the stream's comparable `(op, status)` state
/// (`None` = no turn record at all). Bounded point reads (`LIMIT 1`).
fn read_turn_state(handle: &SessionHandle) -> Option<(String, String)> {
    match handle.active_turn_record() {
        Ok(Some(active)) => {
            return Some((active.turn_op_id.to_string(), "active".to_string()));
        }
        Ok(None) => {}
        Err(_) => return None,
    }
    match handle.latest_turn_record() {
        Ok(Some(latest)) => Some((latest.turn_op_id.to_string(), latest.status)),
        _ => None,
    }
}

/// Build one authoritative snapshot: instance, sequence watermark, durable
/// watermark, the turn projection and (optionally filtered) part snapshots.
pub(crate) fn build_live_snapshot(
    handle: &SessionHandle,
    hub: &LiveOutputHub,
    filter: Option<(i64, &'static str)>,
) -> Result<serde_json::Value, Box<Response>> {
    let turn = turn_projection(handle)?;
    let turn_op = turn.get("op").and_then(|v| v.as_str()).map(str::to_string);
    let watermark = handle
        .last_event_seq()
        .map_err(|e| Box::new(api_err(&e)))?
        .map(|seq| seq.raw())
        .unwrap_or(0);
    let mut parts: Vec<serde_json::Value> = Vec::new();
    match filter {
        Some((message_id, kind)) => {
            parts.push(part_snapshot_json(handle, hub, message_id, kind, turn_op)?);
        }
        None => {
            let (_, briefs, _) = hub.subscribe(handle.id());
            for brief in briefs.iter().take(MAX_SUBSCRIPTION_PARTS) {
                parts.push(part_snapshot_json(
                    handle,
                    hub,
                    brief.message_id,
                    brief.kind,
                    turn_op.clone(),
                )?);
            }
        }
    }
    Ok(serde_json::json!({
        "instance": hub.instance(),
        "session": handle.id().raw(),
        "sequence": hub.sequence(),
        "watermark": watermark,
        "turn": turn,
        "parts": parts,
    }))
}

/// Strict query of the snapshot route: an optional part filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeLiveSnapshotQuery {
    pub session: Option<String>,
    pub message: Option<i64>,
    pub kind: Option<String>,
}

/// `GET /native/session/{id}/live/snapshot[?message=&kind=]` — the
/// authoritative part snapshot a client fetches after an explicit `resync`
/// (or to prime a fresh view). Unknown sessions 404; hostile ids 400.
pub(crate) async fn native_live_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeLiveSnapshotQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    if let Some(session) = q.session.as_deref() {
        let body_sid = match parse_session_id(session) {
            Ok(s) => s,
            Err(e) => return wire_status(e),
        };
        if body_sid != handle.id() {
            return wire_status(malformed_body(&format!(
                "path session id {} does not match query session id {body_sid}",
                handle.id()
            )));
        }
    }
    let filter = match (q.message, q.kind) {
        (Some(message_id), Some(kind)) => {
            let kind: &'static str = match kind.as_str() {
                "text" => "text",
                "reasoning" => "reasoning",
                _ => return wire_status(malformed_body("kind must be text or reasoning")),
            };
            Some((message_id, kind))
        }
        (None, None) => None,
        _ => return wire_status(malformed_body("message and kind must be provided together")),
    };
    match build_live_snapshot(&handle, &state.deps.live_output, filter) {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(response) => *response,
    }
}

/// Strict query of the live-output stream: only `after` (informational
/// resume hint; every reconnect re-handshakes with a fresh snapshot, so a
/// missed range can never silently skip text).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeLiveOutputQuery {
    pub after: Option<u64>,
}

type LiveStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Send>,
>;

/// `GET /native/session/{id}/live/output` — the session-scoped live model
/// output SSE subscription.
///
/// Frames (all JSON `data:` payloads carry `instance` + `session`):
/// * `snapshot` — FIRST frame: exact starting watermark + turn projection +
///   authoritative part snapshots (the subscribe-before-submit handshake);
/// * `live` — one chunk delta: `messageId`, `kind`, `partId`, `turn`,
///   `attempt`, `sequence`, `revision`, UTF-8 byte `offset`, `content`;
/// * `resync` — explicit lag signal: the queue overran the bounded ring;
///   the client fetches `/live/snapshot` and reconciles;
/// * `turn` — the bounded active-turn projection changed (the durable turn
///   record is the ONLY completion authority);
/// * `heartbeat` / `error` — liveness / terminal read failure.
pub(crate) async fn native_live_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeLiveOutputQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let hub = Arc::clone(&state.deps.live_output);
    let ready = Arc::clone(&state.ready);
    // Atomic handshake: the receiver is created under the parts lock (see
    // `LiveOutputHub::subscribe`), so no frame can slip between the
    // snapshot state and the listener.
    let (rx, _briefs, _watermark) = hub.subscribe(handle.id());
    let first = match build_live_snapshot(&handle, &hub, None) {
        Ok(snapshot) => snapshot,
        Err(response) => return *response,
    };
    let stream = live_output_stream(handle, hub, rx, first, ready, q.after.unwrap_or(0));
    let mut response = Response::new(axum::body::Body::from_stream(stream));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    response
}

/// One step of the live-output loop: the state carried across iterations.
struct LiveStreamState {
    handle: SessionHandle,
    hub: Arc<LiveOutputHub>,
    rx: broadcast::Receiver<LiveFrame>,
    journal_rx: tokio::sync::watch::Receiver<u64>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    /// The handshake snapshot (emitted exactly once, first).
    first: Option<serde_json::Value>,
    /// Last emitted turn projection `(op, status)` (`None` = no record).
    turn_state: Option<(String, String)>,
    /// Informational `after` cursor: frames at/below it were already seen.
    skip_until: u64,
    last_emit: tokio::time::Instant,
    last_turn_check: tokio::time::Instant,
}

/// The bounded live-output loop: broadcast frames + turn-status wakes +
/// heartbeats, terminated by the shared readiness flag. Skipped frames
/// (foreign session / `after` cursor) re-enter the select instead of
/// emitting empty chunks.
fn live_output_stream(
    handle: SessionHandle,
    hub: Arc<LiveOutputHub>,
    rx: broadcast::Receiver<LiveFrame>,
    first: serde_json::Value,
    ready: Arc<std::sync::atomic::AtomicBool>,
    skip_until: u64,
) -> LiveStream {
    let journal_rx = handle.subscribe_journal();
    let initial_turn = first
        .get("turn")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let turn_state = initial_turn.as_object().and_then(|o| {
        Some((
            o.get("op")?.as_str()?.to_string(),
            o.get("status")?.as_str()?.to_string(),
        ))
    });
    let state = LiveStreamState {
        handle,
        hub,
        rx,
        journal_rx,
        ready,
        first: Some(first),
        turn_state,
        skip_until,
        last_emit: tokio::time::Instant::now(),
        last_turn_check: tokio::time::Instant::now(),
    };
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if !state.ready.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            if let Some(first) = state.first.take() {
                let frame = sse_frame("snapshot", &first);
                return Some((Ok::<bytes::Bytes, std::convert::Infallible>(frame), state));
            }
            let poll_active = state
                .turn_state
                .as_ref()
                .is_some_and(|(_, status)| status == "active");
            let heartbeat_due = state.last_emit.elapsed()
                >= std::time::Duration::from_millis(super::session::SSE_HEARTBEAT_MS);
            if heartbeat_due {
                state.last_emit = tokio::time::Instant::now();
                return Some((
                    Ok(bytes::Bytes::from_static(b"event: heartbeat\ndata: {}\n\n")),
                    state,
                ));
            }
            tokio::select! {
                biased;
                frame = state.rx.recv() => {
                    let event;
                    let data = match frame {
                        Ok(frame) => {
                            if frame.sequence <= state.skip_until
                                || frame.session != state.handle.id()
                            {
                                // Already-seen cursor or a FOREIGN
                                // session's frame: never cross streams.
                                continue;
                            }
                            event = "live";
                            serde_json::json!({
                                "instance": frame.instance.as_ref(),
                                "session": frame.session.raw(),
                                "messageId": frame.message_id,
                                "kind": frame.kind,
                                "partId": format!("{}:{}", frame.message_id, frame.kind),
                                "turn": state.turn_state.as_ref()
                                    .map(|(op, _)| serde_json::Value::String(op.clone()))
                                    .unwrap_or(serde_json::Value::Null),
                                "attempt": frame.attempt,
                                "sequence": frame.sequence,
                                "revision": frame.revision,
                                "offset": frame.offset,
                                "content": frame.content.as_ref(),
                            })
                        }
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            // The bounded queue overran: an EXPLICIT
                            // resync signal (never silent truncation).
                            event = "resync";
                            serde_json::json!({
                                "instance": state.hub.instance(),
                                "session": state.handle.id().raw(),
                                "reason": "lagged",
                                "dropped": dropped,
                            })
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    };
                    state.last_emit = tokio::time::Instant::now();
                    let frame = sse_frame(event, &data);
                    return Some((Ok(frame), state));
                }
                changed = state.journal_rx.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    state.last_turn_check = tokio::time::Instant::now();
                    let next = read_turn_state(&state.handle);
                    if next != state.turn_state {
                        state.turn_state = next;
                        let data = match &state.turn_state {
                            Some((op, status)) => serde_json::json!({
                                "instance": state.hub.instance(),
                                "session": state.handle.id().raw(),
                                "turn": op,
                                "status": status,
                            }),
                            None => serde_json::json!({
                                "instance": state.hub.instance(),
                                "session": state.handle.id().raw(),
                                "turn": serde_json::Value::Null,
                                "status": serde_json::Value::Null,
                            }),
                        };
                        state.last_emit = tokio::time::Instant::now();
                        let frame = sse_frame("turn", &data);
                        return Some((Ok(frame), state));
                    }
                }
                _ = tokio::time::sleep(
                    std::time::Duration::from_millis(LIVE_TURN_POLL_MS)
                        .saturating_sub(state.last_turn_check.elapsed()),
                ), if poll_active => {
                    state.last_turn_check = tokio::time::Instant::now();
                    let next = read_turn_state(&state.handle);
                    if next != state.turn_state {
                        state.turn_state = next;
                        let data = match &state.turn_state {
                            Some((op, status)) => serde_json::json!({
                                "instance": state.hub.instance(),
                                "session": state.handle.id().raw(),
                                "turn": op,
                                "status": status,
                            }),
                            None => serde_json::json!({
                                "instance": state.hub.instance(),
                                "session": state.handle.id().raw(),
                                "turn": serde_json::Value::Null,
                                "status": serde_json::Value::Null,
                            }),
                        };
                        state.last_emit = tokio::time::Instant::now();
                        let frame = sse_frame("turn", &data);
                        return Some((Ok(frame), state));
                    }
                }
            }
            // A journal wake or poll that changed nothing: loop again
            // (bounded heartbeat handling runs at the top).
        }
    })
    .boxed()
}

#[cfg(test)]
#[path = "live_output_tests.rs"]
mod live_output_tests;
