//! faktor-acp — ACP v1 (Agent Client Protocol, protocol version 1) agent
//! server.
//!
//! Purpose (audit note): expose an ACP agent server so ACP-capable editors
//! (Zed and others) can drive the Faktor core alongside the native API and
//! the legacy compat surface. This crate implements the WIRE + lifecycle
//! only; agent behavior is delegated to the injected backend seam, so the
//! daemon can attach the real runtime without this crate knowing it.
//!
//! # Wire surface (official Agent Client Protocol v1 subset)
//!
//! JSON-RPC 2.0 over the official newline-delimited JSON transport (the
//! official SDK's `ByteStreams` framing), with the legacy `Content-Length`
//! framing still detected and served for frozen pre-conformance peers (see
//! [`protocol`]). Request ids may be non-negative integers **or strings**
//! (the official SDK allocates UUID strings) and are echoed verbatim.
//! Notifications omit the `id` member entirely. Field names follow the
//! official ACP v1 schema; the crate's pre-1.0 deviations
//! (`protocolVersion: "0.1.0"`, `sessionID`, `text`, `"agent"`,
//! `session/abort`, result-passthrough prompt responses) are renamed —
//! deprecated wire aliases are still *accepted* on parse but never
//! produced.
//!
//! | method           | request params                             | response                              |
//! |------------------|--------------------------------------------|---------------------------------------|
//! | `initialize`     | `{protocolVersion: 1, extensions?, clientCapabilities?}` | `{protocolVersion: 1, agentCapabilities, authMethods, extensions?}` |
//! | `session/new`    | opaque backend params (+ `mcpServers`)     | `{sessionId}`                         |
//! | `session/load`   | `{sessionId, cwd?, mcpServers?}`           | `{}` (history replays as `session/update`) |
//! | `session/prompt` | `{sessionId, prompt:[{type:"text",text}]}` | `{stopReason}` (see below)            |
//! | `session/cancel` | `{sessionId}` (notification or request)    | none (notification) / `{}` (request)  |
//! | `$/cancel_request` | `{requestId}` (notification or request)  | none (notification) / `{}` (request)  |
//! | `session/update` | agent→client notification `{sessionId, update}` | —                               |
//! | `session/request_permission` | agent→client request `{sessionId, toolCall, options}` | `{outcome}` |
//! | `fs/read_text_file` / `fs/write_text_file` | agent→client request | `{content}` / `null` |
//! | `authenticate`   | `{methodId}`                               | refused `-32602` (`authMethods` empty) |
//!
//! * Version handshake: `initialize` accepts `protocolVersion: 1` (the
//!   official ACP v1 major version) and rejects anything else loudly with
//!   a typed error — never a silent fallback.
//! * Prompt outcome: a turn ends with the official `stopReason` response
//!   (`"end_turn"` on success, `"cancelled"` when cancelled). The sync
//!   backend's opaque result JSON — which official ACP v1 has no slot for —
//!   rides the official `_meta` extension member of the result and is
//!   omitted when null. Turn failures answer the prompt with the official
//!   error format (`-32603` internal error, backend message in `data`).
//! * A cancelled turn MUST answer the original `session/prompt` with
//!   `{stopReason: "cancelled"}` and MUST NOT emit further content frames
//!   after cancellation is observed.
//! * Error format (official): `{code, message}` with `data` omitted when
//!   absent. Codes: `-32700` parse, `-32600` invalid request, `-32601`
//!   method not found, `-32602` invalid params, `-32603` internal error,
//!   ACP-reserved-range `-32001` (session busy). `-32000` is not used
//!   (officially "Authentication required").
//! * Streaming: the streaming seam emits `session/update` notifications
//!   for the official kinds this surface can populate
//!   (`user_message_chunk`/`agent_message_chunk`/`agent_thought_chunk`
//!   text, `tool_call`, `tool_call_update`, `plan`) plus the crate's
//!   documented `agentStateChanged` status frames (`{kind:
//!   "agentStateChanged", agentState: {status: "idle"|"busy"|"error",
//!   message?}}`, see [`agent_state_changed_update`]) which are gated on
//!   the negotiated extension.
//! * Extensions kept from the pre-conformance surface: `agent_info`
//!   (agent metadata), `session/list` (`{sessions: [{sessionId}, ..]}`),
//!   `shutdown` (request answered `{"ok": true}`, then the loop ends), and
//!   the deprecated `session/abort` alias of `session/cancel`.
//!
//! # Runtime architecture (async dispatch + cancellation)
//!
//! One connection is served by four cooperating roles:
//!
//! 1. **Reader task** — pulls frames from the transport (autodetecting
//!    NDJSON vs legacy `Content-Length` from the peer's first bytes),
//!    decodes JSON-RPC, and routes. `session/cancel` (and the deprecated
//!    `session/abort`) and the request-level `$/cancel_request` extension
//!    short-circuit *here*: the matching session's cancellation token fires
//!    synchronously and, for sync backends, the legacy `abort` hook runs
//!    off-thread. A cancel never waits behind a full writer queue before it
//!    lands. Responses to server→client requests (permissions, client fs)
//!    are routed to the bounded per-connection waiter table; unknown
//!    response ids are logged and dropped, never answered.
//! 2. **Dispatcher task** — owns the per-session state machine and routes
//!    `session/prompt` to per-session operation tasks. At most one running
//!    turn plus one queued prompt per session (FIFO); deeper concurrency
//!    is refused with the typed busy error `-32001`.
//! 3. **Per-session operation tasks** — run one prompt turn against the
//!    backend, stream `session/update` frames, observe the cancellation
//!    token, and answer the original prompt id with the terminal
//!    `stopReason` response *before* promoting a queued prompt, so
//!    per-session wire order holds by construction.
//! 4. **Writer task** — owns the transport output. Every frame travels a
//!    bounded queue: the **main queue** (capacity
//!    [`AcpConfig::writer_queue_capacity`], default 64) carries responses
//!    and `session/update` notifications, and the **cancel lane**
//!    (fixed capacity [`CANCEL_LANE_CAPACITY`] = 16) carries cancel
//!    acknowledgements. The writer drains the lane strictly before the
//!    main queue, so a cancel answer never waits behind a full queue of
//!    prompt frames. Full queues backpressure senders — nothing buffers
//!    unboundedly.
//!
//! # Bounded everything
//!
//! - Declared frames are capped at [`protocol::MAX_FRAME_BYTES`] (16 MiB)
//!   by the parser; request params at [`MAX_PARAMS_BYTES`] (1 MiB);
//!   responses and notifications at [`MAX_RESPONSE_BYTES`] (8 MiB) — an
//!   oversize item is refused with `-32603`, never truncated or buffered.
//! - Session bookkeeping is capped ([`AcpConfig::max_sessions`]); idle
//!   entries are evicted first.
//! - Server→client requests are capped per connection
//!   ([`AcpConfig::max_client_requests`]), wait at most
//!   [`AcpConfig::client_request_timeout`], and are cancelled with the
//!   turn; outstanding waiters observe the connection close.
//! - Sync backend prompts run on their operation task (same contract as
//!   the pre-conformance single-task loop): the daemon must attach a
//!   backend that answers promptly or is internally time-boxed. Mid-run
//!   cancellation of a *sync* prompt is delivered through the legacy
//!   `abort` hook (the daemon maps it onto its own cancellation); the
//!   *streaming* seam observes the crate's cancellation token directly.
//!   Wire outcomes are identical: exactly one terminal response per turn,
//!   `"cancelled"` iff the cancel reached the turn before its terminal
//!   decision point.
//!
//! # Mapping layer over the native services
//!
//! This crate owns no agent logic: every ACP operation below is a mapping
//! over the injected backend seam, which the daemon attaches to its native
//! `TaskExecutor`/`AgentRuntime`/session/verification services.
//!
//! * **`session/load`** — the backend hook returns the frozen native
//!   `faktor_protocol::native::MessagesPage` for a session it owns. The crate
//!   maps the page (newest-first natively) into chronological official
//!   replay frames: `user_message_chunk`/`agent_message_chunk` text,
//!   `agent_thought_chunk` for reasoning/summary/system parts, `tool_call`
//!   from native `tool_call` parts, and `tool_call_update` from native
//!   `tool_result` parts. Load is bounded ([`MAX_LOAD_MESSAGES`],
//!   [`MAX_LOAD_FRAMES`]); an incomplete page (`has_more`) or an oversized
//!   history is refused, never silently truncated. `loadSession` is
//!   advertised only when the backend reports the capability *and* the hook
//!   exists (both are declared together in [`BackendCapabilities`]).
//! * **Permissions** — [`PromptCtx::request_permission`] (and the owned
//!   [`ClientHandle`]) sends the official `session/request_permission`
//!   request and awaits the client's `allow`/`deny` outcome. The native
//!   `ChannelPermissionRequester` flow plugs in at the adapter: the ACP
//!   outcome resolves the native pending request with the same
//!   first-decision-wins/timeout/cleanup semantics (adversarially tested
//!   against the real requester in `tests/acp.rs`).
//! * **Tool calls and plans** — official `tool_call`, `tool_call_update`
//!   and `plan` frames are emitted from the native turn/tool event shapes
//!   via [`tool_call_from_native`], [`tool_result_from_native`] and
//!   [`plan_from_native_steps`]. The native event surface lacks the ACP
//!   `kind` field (tool calls), and plan entries carry no priority/status:
//!   those fields are omitted or degraded to `medium`/`pending`, as
//!   documented on each builder — never invented.
//! * **Client filesystem** — when `initialize` negotiates
//!   `clientCapabilities.fs.readTextFile`/`writeTextFile`, the backend can
//!   call the official client methods through [`ClientHandle`]; without the
//!   negotiated capability every call is refused with a typed error and no
//!   frame is sent.
//! * **MCP** — `mcpCapabilities` mirrors [`BackendCapabilities::mcp_http`]/
//!   `mcp_sse`; non-empty `mcpServers` on `session/new`/`session/load` are
//!   refused with `-32602` unless the backend reports the capability.
//! * **`authenticate`** — no real auth flow exists, so `authMethods` stays
//!   empty and every `authenticate` call is an official `-32602` refusal.
//!
//! # Extension negotiation (Faktor frames only for declaring clients)
//!
//! `initialize` may declare extensions (`"extensions":
//! ["faktor.agentStateChanged"]`). The accepted subset is echoed back;
//! malformed declarations are refused loudly (`-32602`) and unknown names
//! are silently not accepted. Extension frames (updates carrying a `kind`
//! member, e.g. [`agent_state_changed_update`]) are suppressed for clients
//! that did not declare them: [`PromptCtx::emit_agent_state`] is a no-op in
//! that case and [`PromptCtx::emit`] returns [`EmitError::NotNegotiated`].
//! `clientCapabilities.fs` is negotiated per connection and gates every
//! [`ClientHandle`] filesystem call.
//!
//! # Terminal extension (`faktor.terminal`, negotiated)
//!
//! Interactive terminals are a negotiated Faktor extension: `initialize`
//! accepts `"extensions": ["faktor.terminal"]` exactly like
//! `faktor.agentStateChanged`, and the accepted name is echoed back only
//! when the server carries a [`TerminalAuthority`]. The daemon attaches its
//! existing session-owned terminal authority through
//! [`AcpServer::with_terminal_authority`]; without one the name is not
//! negotiated and nothing is claimed. Clients that did not negotiate keep
//! the official `-32601` for every `terminal/*` request.
//!
//! | method            | params                                                   | result |
//! |-------------------|----------------------------------------------------------|--------|
//! | `terminal/create` | `{sessionId, command, args?, cwd?, env?, rows?, cols?}`  | `{terminalId, pid, sessionId, ownershipId}` |
//! | `terminal/input`  | `{sessionId, terminalId, data}`                          | `{}`   |
//! | `terminal/resize` | `{sessionId, terminalId, rows, cols}`                    | `{}`   |
//! | `terminal/kill`   | `{sessionId, terminalId}`                                | `{}`   |
//! | `terminal/close`  | `{sessionId, terminalId}`                                | `{}`   |
//! | `terminal/list`   | `{sessionId}`                                            | `{sessionId, terminals: [...]}` |
//!
//! Params are strict (unknown fields answer `-32602`) and bounded: command
//! and args by the [`MAX_TERMINAL_COMMAND_BYTES`] family, `env` by
//! [`MAX_TERMINAL_ENV_NAMES`]/[`MAX_TERMINAL_ENV_NAME_BYTES`], input by
//! [`MAX_TERMINAL_INPUT_BYTES`], sizes to the `u16` geometry range
//! (`0`/oversize/NUL are typed refusals). `env` is an ALLOWLIST OF NAMES:
//! the attached authority resolves them against the daemon environment and
//! applies its deny-set, so daemon secrets never cross; this crate never
//! carries environment values.
//!
//! Every terminal is owned by the session that created it: the per-
//! connection terminal registry keys rows by `(sessionId, terminalId)`,
//! a foreign session or terminal id is a typed `-32602` denial (existence is
//! never leaked), and `terminal/list` projects only the requesting session's
//! rows with their ownership ids. A durable authority can opt in
//! ([`TerminalAuthority::exposes_durable_terminals`] /
//! [`TerminalAuthority::list_terminals`]) to project ITS durable rows for
//! the listing instead — including rows a restart recovered as lost — while
//! the per-connection table keeps owning output pumping. Output arrives as
//! bounded `session/update` extension frames (`kind: "terminalOutput"`,
//! [`terminal_output_update`]): coalesced per pump window, capped per frame
//! ([`AcpConfig::terminal_output_frame_max`]) and per window
//! ([`AcpConfig::terminal_output_window_max`]), ordered by a per-terminal
//! `seq`; when the writer queue is full or a window cap is hit, bytes are
//! dropped and the loss is recorded (`droppedBytes`/`backpressureEvents` on
//! the next frame and on the `terminal/list` row) — a pump never blocks the
//! writer, the PTY drain, or a prompt turn.
//!
//! Lifetime: terminals are cancellable (per-terminal `kill`/`close`, and the
//! connection's wind-down on EOF), bounded by [`AcpConfig::max_terminals`]
//! and the [`AcpConfig::max_sessions`]-capped session index, session-scoped,
//! and killed before the serve loop returns. The process is owned by the
//! attached authority (the daemon's durable terminal service owns the pty
//! and journals every transition), so dropping the connection table does not
//! orphan or silently kill a row the durable authority still governs. This
//! crate implements no PTY itself: everything goes through the injected
//! [`TerminalAuthority`] seam, which the daemon maps onto `faktor-pty` and
//! its existing session-owned terminal rows.
//!
//! # Out of official ACP v1 scope (honestly absent, never faked)
//!
//! Prompt content blocks other than text are refused with `-32602`; the
//! text-only path is the documented structured-output path — a text block
//! carries any structured payload (JSON/XML) verbatim and this crate never
//! inspects or rewrites it. The official client-side `terminal/*` methods
//! (`terminal/output`, `terminal/wait_for_exit`, `terminal/release`) are not
//! part of this extension's table and keep answering `-32601`.

use futures::future::BoxFuture;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::Duration;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faktor_protocol::native::{MessagesPage, Part as NativePart};

use crate::protocol::AcpMethod;

pub mod protocol;

/// Official ACP v1 protocol version accepted by the `initialize` handshake.
pub const PROTOCOL_VERSION: u64 = 1;

/// Fixed capacity of the high-priority cancel lane (see module docs).
pub const CANCEL_LANE_CAPACITY: usize = 16;

/// Cap on the serialized `params` of one incoming request (1 MiB). Client
/// responses (fs reads and permission outcomes) share the same bound.
pub const MAX_PARAMS_BYTES: usize = 1024 * 1024;

/// Cap on one serialized frame written to the wire (8 MiB).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Bound on the message history a single `session/load` may replay.
pub const MAX_LOAD_MESSAGES: usize = 4096;

/// Bound on the update frames a single `session/load` may replay.
pub const MAX_LOAD_FRAMES: usize = 16384;

/// Default cap on concurrent server→client requests (permission and client
/// filesystem calls) per connection; exceeding it is a typed refusal.
pub const DEFAULT_MAX_CLIENT_REQUESTS: usize = 32;

/// The one Faktor extension this crate negotiates: the
/// `agentStateChanged` status frame.
pub const EXTENSION_AGENT_STATE_CHANGED: &str = "faktor.agentStateChanged";

/// The negotiated terminal extension: session-owned PTY control
/// (`terminal/create|input|resize|kill|close|list`) plus bounded
/// `terminalOutput` update frames.
pub const EXTENSION_TERMINAL: &str = "faktor.terminal";

/// The extensions this server can accept, in canonical order.
pub const ACCEPTED_EXTENSIONS: [&str; 2] = [EXTENSION_AGENT_STATE_CHANGED, EXTENSION_TERMINAL];

/// JSON-RPC well-known error codes (official ACP v1 messages).
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// ACP-reserved-range error (official range -32000..=-32099): a prompt
/// turn is already running (or queued) for the session.
pub const SESSION_BUSY: i64 = -32001;

/// ACP-reserved-range error: the server's per-session prompt capacity is
/// exhausted (a bounded-resource refusal, never unbounded growth).
pub const SESSION_LIMIT: i64 = -32003;

/// ACP-reserved-range error: the per-connection terminal capacity is
/// exhausted ([`AcpConfig::max_terminals`]); new terminals are refused,
/// existing ones keep working.
pub const TERMINAL_LIMIT: i64 = -32004;

/// Cap on one terminal `command` (bytes).
pub const MAX_TERMINAL_COMMAND_BYTES: usize = 4096;

/// Cap on the `args` list of one `terminal/create`.
pub const MAX_TERMINAL_ARGS: usize = 256;

/// Cap on one `args` entry (bytes).
pub const MAX_TERMINAL_ARG_BYTES: usize = 4096;

/// Cap on a `terminal/create` `cwd` (bytes).
pub const MAX_TERMINAL_CWD_BYTES: usize = 4096;

/// Cap on the `env` allowlist of one `terminal/create` (names only).
pub const MAX_TERMINAL_ENV_NAMES: usize = 64;

/// Cap on one `env` allowlist entry (bytes).
pub const MAX_TERMINAL_ENV_NAME_BYTES: usize = 256;

/// Cap on one session/terminal id string in terminal params (bytes).
pub const MAX_TERMINAL_ID_BYTES: usize = 256;

/// Cap on one `terminal/input` payload (bytes).
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;

/// Default cap on the raw bytes of one `terminalOutput` frame.
pub const TERMINAL_OUTPUT_FRAME_MAX: usize = 16 * 1024;

/// Default cap on the raw bytes a terminal pump may emit per window (one
/// pump tick); the excess is dropped and recorded as backpressure.
pub const TERMINAL_OUTPUT_WINDOW_MAX: usize = 128 * 1024;

const MAX_METHOD_LEN: usize = 128;
const READ_CHUNK: usize = 64 * 1024;
const SHUTDOWN_METHOD: &str = "shutdown";
/// Request-level cancellation extension: `{requestId}` cancelled like
/// `session/cancel` cancels a session's active turn.
const CANCEL_REQUEST_METHOD: &str = "$/cancel_request";
const REQUEST_PERMISSION_METHOD: &str = "session/request_permission";
const FS_READ_METHOD: &str = "fs/read_text_file";
const FS_WRITE_METHOD: &str = "fs/write_text_file";

/// Official canonical error messages.
const MSG_PARSE_ERROR: &str = "Parse error";
const MSG_METHOD_NOT_FOUND: &str = "Method not found";
const MSG_INTERNAL_ERROR: &str = "Internal error";
const MSG_SESSION_BUSY: &str = "A prompt turn is already in progress for this session";
const MSG_SESSION_LIMIT: &str = "session capacity exhausted";
const MSG_TERMINAL_LIMIT: &str = "terminal capacity exhausted";
const MSG_LOAD_MCP_UNSUPPORTED: &str = "this agent does not support MCP servers";
const MSG_AUTH_UNAVAILABLE: &str = "no authentication methods are available";

/// A JSON-RPC request id: a non-negative integer or a string. The official
/// SDK allocates string/UUID ids, so both forms are accepted and echoed
/// verbatim in the response ([`RequestId::to_value`]). Fractional and
/// negative numbers are invalid requests (never truncated or coerced).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(u64),
    String(String),
}

impl RequestId {
    /// Parse a JSON id: integers must fit a non-negative `u64`; strings are
    /// accepted verbatim. Anything else (null, float, bool, object) is not
    /// a valid request id.
    pub fn from_value(value: &Value) -> Option<RequestId> {
        match value {
            Value::Number(number) => number.as_u64().map(RequestId::Number),
            Value::String(text) => Some(RequestId::String(text.clone())),
            _ => None,
        }
    }

    /// The exact JSON value to echo back in the response.
    pub fn to_value(&self) -> Value {
        match self {
            RequestId::Number(number) => json!(number),
            RequestId::String(text) => json!(text),
        }
    }

    /// The numeric form, when this id is an integer.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            RequestId::Number(number) => Some(*number),
            RequestId::String(_) => None,
        }
    }

    /// The string form, when this id is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            RequestId::Number(_) => None,
            RequestId::String(text) => Some(text),
        }
    }
}

impl From<u64> for RequestId {
    fn from(number: u64) -> Self {
        RequestId::Number(number)
    }
}

impl From<String> for RequestId {
    fn from(text: String) -> Self {
        RequestId::String(text)
    }
}

impl From<&str> for RequestId {
    fn from(text: &str) -> Self {
        RequestId::String(text.to_string())
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(number) => write!(formatter, "{number}"),
            RequestId::String(text) => write!(formatter, "{text}"),
        }
    }
}

/// Server-side sizing/behavior knobs. All queues are bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpConfig {
    /// Capacity of the main outgoing frame queue (responses + updates).
    pub writer_queue_capacity: usize,
    /// Capacity of the reader→dispatcher request queue.
    pub request_queue_capacity: usize,
    /// Upper bound on tracked sessions (idle entries are evicted first).
    pub max_sessions: usize,
    /// Bounded wait granted to a streaming backend after cancellation
    /// before the operation task answers `stopReason: cancelled` anyway.
    pub cancel_grace: Duration,
    /// Bounded wait for the dispatcher/writer tasks to wind down at EOF.
    pub shutdown_timeout: Duration,
    /// Bounded wait for one server→client request (permission request or
    /// client filesystem call) before it answers with a typed timeout.
    pub client_request_timeout: Duration,
    /// Cap on concurrent server→client requests per connection.
    pub max_client_requests: usize,
    /// Cap on live terminals per connection (new creates are refused with
    /// [`TERMINAL_LIMIT`] once reached; existing rows keep working).
    pub max_terminals: usize,
    /// Cap on the raw bytes of one `terminalOutput` frame.
    pub terminal_output_frame_max: usize,
    /// Cap on the raw bytes one terminal pump emits per window; the excess
    /// is dropped and recorded as backpressure.
    pub terminal_output_window_max: usize,
    /// Terminal pump tick: how often available output is coalesced and
    /// emitted. Bounded loss is only ever recorded, never silent.
    pub terminal_poll_interval: Duration,
    /// Bounded wait for one authority terminal spawn.
    pub terminal_spawn_timeout: Duration,
    /// Bounded wait for one terminal write/kill operation.
    pub terminal_io_timeout: Duration,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            writer_queue_capacity: 64,
            request_queue_capacity: 64,
            max_sessions: 1024,
            cancel_grace: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(5),
            client_request_timeout: Duration::from_secs(300),
            max_client_requests: DEFAULT_MAX_CLIENT_REQUESTS,
            max_terminals: 32,
            terminal_output_frame_max: TERMINAL_OUTPUT_FRAME_MAX,
            terminal_output_window_max: TERMINAL_OUTPUT_WINDOW_MAX,
            terminal_poll_interval: Duration::from_millis(20),
            terminal_spawn_timeout: Duration::from_secs(10),
            terminal_io_timeout: Duration::from_secs(2),
        }
    }
}

/// Status values of the crate's `agentStateChanged` status frames
/// (official ACP v1 enum: `idle`, `busy`, `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStateStatus {
    Busy,
    Idle,
    Error,
}

impl AgentStateStatus {
    /// Exact on-the-wire status string.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            AgentStateStatus::Busy => "busy",
            AgentStateStatus::Idle => "idle",
            AgentStateStatus::Error => "error",
        }
    }
}

/// `session/update` frame body: `agentStateChanged` status frame
/// (`{kind, agentState: {status, message?}}`). This is this crate's
/// documented status channel: the official ACP v1 schema has no state
/// frame kind, so status frames are emitted only by backends that opt in
/// (see module docs on out-of-scope surface).
pub fn agent_state_changed_update(status: AgentStateStatus, message: Option<&str>) -> Value {
    let mut state = Map::new();
    state.insert("status".into(), json!(status.as_wire_str()));
    if let Some(message) = message {
        state.insert("message".into(), json!(message));
    }
    let mut update = Map::new();
    update.insert("kind".into(), json!("agentStateChanged"));
    update.insert("agentState".into(), Value::Object(state));
    Value::Object(update)
}

/// `session/update` frame body: official `agent_message_chunk` content
/// frame (`{sessionUpdate, content: {type: "text", text}}`).
pub fn text_chunk_update(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// `session/update` frame body: official `user_message_chunk` (used when
/// replaying `session/load` history; live turns never echo user input).
pub fn user_message_chunk_update(text: &str) -> Value {
    json!({
        "sessionUpdate": "user_message_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// `session/update` frame body: official `agent_thought_chunk`. Reused for
/// native reasoning/summary/system parts (the official schema has no
/// separate kind for those; see [`history_updates`]).
pub fn agent_thought_chunk_update(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// Official ACP tool-call status enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolCallStatus {
    /// Exact on-the-wire status string.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            ToolCallStatus::Pending => "pending",
            ToolCallStatus::InProgress => "in_progress",
            ToolCallStatus::Completed => "completed",
            ToolCallStatus::Failed => "failed",
        }
    }

    /// Map the frozen native tool-run state vocabulary (`pending`,
    /// `running`, `completed`, `failed`) onto the ACP status enum. Unknown
    /// native states degrade to `None`: the caller omits the optional
    /// `status` member rather than inventing one.
    pub fn from_native_state(state: &str) -> Option<Self> {
        match state {
            "pending" => Some(ToolCallStatus::Pending),
            "running" => Some(ToolCallStatus::InProgress),
            "completed" => Some(ToolCallStatus::Completed),
            "failed" => Some(ToolCallStatus::Failed),
            _ => None,
        }
    }
}

/// Official ACP tool-kind enum. The native tool surface carries no `kind`,
/// so [`tool_call_from_native`] omits it; adapters with richer knowledge can
/// pass one explicitly to [`tool_call_update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

impl ToolKind {
    /// Exact on-the-wire kind string.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            ToolKind::Read => "read",
            ToolKind::Edit => "edit",
            ToolKind::Delete => "delete",
            ToolKind::Move => "move",
            ToolKind::Search => "search",
            ToolKind::Execute => "execute",
            ToolKind::Think => "think",
            ToolKind::Fetch => "fetch",
            ToolKind::SwitchMode => "switch_mode",
            ToolKind::Other => "other",
        }
    }
}

/// `session/update` frame body: official `tool_call` creation frame
/// (`{sessionUpdate, toolCallId, title, kind?, status?, content?, rawInput?}`).
/// `kind`, `status` and `rawInput` are omitted when `None` — the official
/// schema treats absent fields as unknown/pending, never as a claim.
pub fn tool_call_update(
    tool_call_id: &str,
    title: &str,
    kind: Option<ToolKind>,
    status: Option<ToolCallStatus>,
    raw_input: Option<&Value>,
) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("tool_call"));
    update.insert("toolCallId".into(), json!(tool_call_id));
    update.insert("title".into(), json!(title));
    if let Some(kind) = kind {
        update.insert("kind".into(), json!(kind.as_wire_str()));
    }
    if let Some(status) = status {
        update.insert("status".into(), json!(status.as_wire_str()));
    }
    if let Some(raw_input) = raw_input {
        update.insert("rawInput".into(), raw_input.clone());
    }
    Value::Object(update)
}

/// `session/update` frame body: official `tool_call_update` status frame
/// (`{sessionUpdate, toolCallId, status, content?, rawOutput?}`).
pub fn tool_call_status_update(
    tool_call_id: &str,
    status: ToolCallStatus,
    content: Option<Value>,
    raw_output: Option<&Value>,
) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("tool_call_update"));
    update.insert("toolCallId".into(), json!(tool_call_id));
    update.insert("status".into(), json!(status.as_wire_str()));
    if let Some(content) = content {
        update.insert("content".into(), content);
    }
    if let Some(raw_output) = raw_output {
        update.insert("rawOutput".into(), raw_output.clone());
    }
    Value::Object(update)
}

/// Map one frozen native tool-call part (`tool_call_id`, `name`, `input`,
/// `state`) to the official creation frame. Documented degradation: the
/// native shape has no tool `kind`, so `kind` is omitted; an unrecognized
/// native `state` omits the optional `status` instead of guessing.
pub fn tool_call_from_native(tool_call_id: &str, name: &str, input: &Value, state: &str) -> Value {
    tool_call_update(
        tool_call_id,
        name,
        None,
        ToolCallStatus::from_native_state(state),
        Some(input),
    )
}

/// Map one frozen native tool-result part to an official `tool_call_update`.
/// The bounded excerpt becomes a text content block; a non-zero exit code
/// maps to `failed`, zero/absent to `completed`. The durable artifact
/// reference (`artifact`/`slice_hint`) has no official field and rides the
/// official `_meta` extension member so the client can page the full output.
pub fn tool_result_from_native(
    tool_call_id: &str,
    excerpt: &str,
    exit_code: Option<i32>,
    artifact: Option<&str>,
    slice_hint: Option<&str>,
) -> Value {
    let status = match exit_code {
        Some(0) | None => ToolCallStatus::Completed,
        Some(_) => ToolCallStatus::Failed,
    };
    let content = json!([{
        "type": "content",
        "content": { "type": "text", "text": excerpt },
    }]);
    let mut update = tool_call_status_update(tool_call_id, status, Some(content), None);
    if artifact.is_some() || slice_hint.is_some() {
        let mut meta = Map::new();
        if let Some(artifact) = artifact {
            meta.insert("artifact".into(), json!(artifact));
        }
        if let Some(slice_hint) = slice_hint {
            meta.insert("sliceHint".into(), json!(slice_hint));
        }
        if let Value::Object(object) = &mut update {
            object.insert("_meta".into(), Value::Object(meta));
        }
    }
    update
}

/// Official ACP plan-entry priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

impl PlanPriority {
    /// Exact on-the-wire priority string.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            PlanPriority::High => "high",
            PlanPriority::Medium => "medium",
            PlanPriority::Low => "low",
        }
    }
}

/// Official ACP plan-entry status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    /// Exact on-the-wire status string.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Completed => "completed",
        }
    }
}

/// One official ACP plan entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanPriority,
    pub status: PlanStatus,
}

impl PlanEntry {
    /// The documented conservative entry: native ledger steps exist before
    /// they run and the native surface tracks no priority.
    pub fn pending(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            priority: PlanPriority::Medium,
            status: PlanStatus::Pending,
        }
    }
}

/// `session/update` frame body: official `plan` frame (`{sessionUpdate,
/// entries}`), in the caller's given (plan) order.
pub fn plan_update(entries: &[PlanEntry]) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "content": entry.content,
                "priority": entry.priority.as_wire_str(),
                "status": entry.status.as_wire_str(),
            })
        })
        .collect();
    json!({ "sessionUpdate": "plan", "entries": entries })
}

/// Map native ledger plan steps — `(text, parent_index)` in ascending
/// `step_index` order — to an official plan frame. Documented degradation:
/// the native ledger tracks neither priority nor per-step status, so every
/// entry is emitted as `medium`/`pending`; the flat ACP plan cannot carry
/// `parent_index`, which is dropped.
pub fn plan_from_native_steps(steps: &[(String, Option<u32>)]) -> Value {
    let entries: Vec<PlanEntry> = steps
        .iter()
        .map(|(text, _)| PlanEntry::pending(text))
        .collect();
    plan_update(&entries)
}

/// Official `session/update` notification params: `{sessionId, update}`.
pub fn session_update_params(session_id: &str, update: Value) -> Value {
    json!({ "sessionId": session_id, "update": update })
}

/// `session/update` frame body: the negotiated `terminalOutput` extension
/// frame (`{kind, terminalId, seq, data, bytes, droppedBytes?,
/// backpressureEvents?}`). `data` is the lossy-UTF-8 view of the raw chunk
/// (terminal streams are not guaranteed valid UTF-8), `bytes` is the exact
/// raw byte count of the chunk, `seq` is the per-terminal monotonic frame
/// index, and the optional counters report bytes/events dropped since the
/// previous frame (omitted when zero — never silently lost, always
/// recorded).
pub fn terminal_output_update(
    terminal_id: &str,
    seq: u64,
    data: &str,
    bytes: usize,
    dropped_bytes: u64,
    backpressure_events: u64,
) -> Value {
    let mut update = Map::new();
    update.insert("kind".into(), json!("terminalOutput"));
    update.insert("terminalId".into(), json!(terminal_id));
    update.insert("seq".into(), json!(seq));
    update.insert("data".into(), json!(data));
    update.insert("bytes".into(), json!(bytes));
    if dropped_bytes > 0 {
        update.insert("droppedBytes".into(), json!(dropped_bytes));
    }
    if backpressure_events > 0 {
        update.insert("backpressureEvents".into(), json!(backpressure_events));
    }
    Value::Object(update)
}

/// Cooperative cancellation token shared by a running turn, its streaming
/// backend, and the cancel path of the reader task.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    fired: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    fn new() -> Self {
        Self::default()
    }

    /// Fire the token. Idempotent: a fired token never unfires.
    pub fn cancel(&self) {
        if !self.inner.fired.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.fired.load(Ordering::Acquire)
    }

    /// Resolves once cancellation has been requested.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl PartialEq for CancelToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for CancelToken {}

/// Why an emission failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitError {
    /// The turn was cancelled; the emitter must stop producing frames.
    Cancelled,
    /// The outgoing connection closed (writer task ended).
    Closed,
    /// The frame exceeds [`MAX_RESPONSE_BYTES`].
    TooLarge,
    /// The update is an extension frame (`kind` member) and the client did
    /// not declare that extension during `initialize`; it was suppressed
    /// (see [`PromptCtx::emit_agent_state`], which treats this as success).
    NotNegotiated,
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmitError::Cancelled => write!(f, "turn cancelled"),
            EmitError::Closed => write!(f, "connection closed"),
            EmitError::TooLarge => write!(f, "frame exceeds the response bound"),
            EmitError::NotNegotiated => write!(f, "extension frame not negotiated by the client"),
        }
    }
}

/// Official ACP permission-option kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionOptionKind {
    /// Exact on-the-wire kind string.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            PermissionOptionKind::AllowOnce => "allow_once",
            PermissionOptionKind::AllowAlways => "allow_always",
            PermissionOptionKind::RejectOnce => "reject_once",
            PermissionOptionKind::RejectAlways => "reject_always",
        }
    }
}

/// One official `session/request_permission` option. The canonical
/// constructors use the kind string as `optionId` so adapters can map the
/// client's selection back onto the native allow/deny decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    option_id: String,
    name: String,
    kind: PermissionOptionKind,
}

impl PermissionOption {
    pub fn new(
        option_id: impl Into<String>,
        name: impl Into<String>,
        kind: PermissionOptionKind,
    ) -> Self {
        Self {
            option_id: option_id.into(),
            name: name.into(),
            kind,
        }
    }

    pub fn allow_once() -> Self {
        Self::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce)
    }

    pub fn allow_always() -> Self {
        Self::new(
            "allow_always",
            "Allow always",
            PermissionOptionKind::AllowAlways,
        )
    }

    pub fn reject_once() -> Self {
        Self::new(
            "reject_once",
            "Reject once",
            PermissionOptionKind::RejectOnce,
        )
    }

    pub fn reject_always() -> Self {
        Self::new(
            "reject_always",
            "Reject always",
            PermissionOptionKind::RejectAlways,
        )
    }

    pub fn option_id(&self) -> &str {
        &self.option_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> PermissionOptionKind {
        self.kind
    }

    fn to_wire(&self) -> Value {
        json!({
            "optionId": self.option_id,
            "name": self.name,
            "kind": self.kind.as_wire_str(),
        })
    }
}

/// The client's answer to `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOutcome {
    selected_option_id: Option<String>,
}

impl PermissionOutcome {
    pub fn selected(option_id: impl Into<String>) -> Self {
        Self {
            selected_option_id: Some(option_id.into()),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            selected_option_id: None,
        }
    }

    /// The selected option id, or `None` for the official cancelled outcome.
    pub fn selected_option_id(&self) -> Option<&str> {
        self.selected_option_id.as_deref()
    }

    pub fn is_selected(&self) -> bool {
        self.selected_option_id.is_some()
    }
}

/// Why a server→client request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequestError {
    /// The negotiated client capability does not include this method (or
    /// the backend does not know it): nothing was sent, never silent.
    UnsupportedMethod(String),
    /// The client did not answer within [`AcpConfig::client_request_timeout`].
    Timeout,
    /// The turn was cancelled while waiting.
    Cancelled,
    /// The connection closed while waiting.
    Closed,
    /// The request params exceed [`MAX_PARAMS_BYTES`].
    TooLarge,
    /// The per-connection outstanding-request bound was reached.
    TooMany,
    /// The client answered with a malformed or out-of-contract payload.
    Malformed(String),
    /// The client answered with a JSON-RPC error.
    Rpc { code: i64, message: String },
}

impl std::fmt::Display for ClientRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientRequestError::UnsupportedMethod(method) => {
                write!(f, "client method {method} is not negotiated")
            }
            ClientRequestError::Timeout => write!(f, "client request timed out"),
            ClientRequestError::Cancelled => write!(f, "turn cancelled while awaiting the client"),
            ClientRequestError::Closed => write!(f, "connection closed while awaiting the client"),
            ClientRequestError::TooLarge => write!(f, "client request params exceed the bound"),
            ClientRequestError::TooMany => write!(f, "too many outstanding client requests"),
            ClientRequestError::Malformed(message) => {
                write!(f, "malformed client response: {message}")
            }
            ClientRequestError::Rpc { code, message } => {
                write!(f, "client answered error {code}: {message}")
            }
        }
    }
}

impl std::error::Error for ClientRequestError {}

/// The client-side surface negotiated for this connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientCapabilities {
    /// Accepted Faktor extension names (subset of [`ACCEPTED_EXTENSIONS`]).
    pub extensions: Vec<String>,
    /// `clientCapabilities.fs.readTextFile`.
    pub fs_read_text_file: bool,
    /// `clientCapabilities.fs.writeTextFile`.
    pub fs_write_text_file: bool,
}

/// Per-connection negotiation state (shared by initialize, turns and the
/// reader task).
#[derive(Clone, Debug, Default)]
struct Negotiation {
    inner: Arc<Mutex<Negotiated>>,
}

#[derive(Clone, Debug, Default)]
struct Negotiated {
    extensions: Vec<String>,
    fs_read_text_file: bool,
    fs_write_text_file: bool,
}

impl Negotiation {
    fn replace(&self, next: Negotiated) {
        *self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }

    fn snapshot(&self) -> Negotiated {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn has_extension(&self, name: &str) -> bool {
        self.snapshot().extensions.iter().any(|e| e == name)
    }
}

/// Outstanding server→client requests, keyed by server-allocated id.
#[derive(Clone, Debug)]
struct Outstanding {
    inner: Arc<Mutex<OutstandingInner>>,
}

#[derive(Debug)]
struct OutstandingInner {
    waiters: HashMap<u64, oneshot::Sender<Value>>,
    next_id: u64,
    max: usize,
}

impl Outstanding {
    fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutstandingInner {
                waiters: HashMap::new(),
                next_id: 1,
                max,
            })),
        }
    }

    fn register(&self) -> Result<(u64, oneshot::Receiver<Value>), ClientRequestError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.waiters.len() >= inner.max {
            return Err(ClientRequestError::TooMany);
        }
        // Skip ids still outstanding (possible only after a wrap, which is
        // itself bounded by the waiter cap).
        let mut id = inner.next_id.max(1);
        let mut scanned = 0usize;
        while inner.waiters.contains_key(&id) && scanned <= inner.waiters.len() {
            id = id.wrapping_add(1).max(1);
            scanned += 1;
        }
        inner.next_id = id.wrapping_add(1).max(1);
        let (tx, rx) = oneshot::channel();
        inner.waiters.insert(id, tx);
        Ok((id, rx))
    }

    /// Deliver a client response to the waiting request. Returns false when
    /// no request with that id is outstanding.
    fn resolve(&self, id: u64, value: Value) -> bool {
        let sender = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .waiters
            .remove(&id);
        match sender {
            Some(sender) => sender.send(value).is_ok(),
            None => false,
        }
    }

    fn remove(&self, id: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .waiters
            .remove(&id);
    }

    /// Drop every waiter (connection winding down); each waiter observes
    /// [`ClientRequestError::Closed`].
    fn drain(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .waiters
            .clear();
    }
}

/// An owned handle for server→client requests (permissions, client
/// filesystem), safe to move into a `'static` task. The native daemon
/// adapter wraps it to answer its `PermissionRequester` flow.
#[derive(Clone)]
pub struct ClientHandle {
    main_tx: mpsc::Sender<Vec<u8>>,
    session_id: Arc<str>,
    token: CancelToken,
    negotiation: Negotiation,
    outstanding: Outstanding,
    timeout: Duration,
}

impl ClientHandle {
    /// The session this request is scoped to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The negotiated client capabilities for this connection.
    pub fn capabilities(&self) -> ClientCapabilities {
        let negotiated = self.negotiation.snapshot();
        ClientCapabilities {
            extensions: negotiated.extensions,
            fs_read_text_file: negotiated.fs_read_text_file,
            fs_write_text_file: negotiated.fs_write_text_file,
        }
    }

    /// Official `session/request_permission`: one bounded round trip whose
    /// outcome is `selected(optionId)` or `cancelled`. The client must
    /// select one of the offered options; anything else is malformed.
    pub async fn request_permission(
        &self,
        tool_call: &Value,
        options: &[PermissionOption],
    ) -> Result<PermissionOutcome, ClientRequestError> {
        if options.is_empty() {
            return Err(ClientRequestError::Malformed(
                "permission options must not be empty".into(),
            ));
        }
        let object = tool_call
            .as_object()
            .ok_or_else(|| ClientRequestError::Malformed("toolCall must be an object".into()))?;
        for field in ["toolCallId", "title"] {
            if object.get(field).and_then(Value::as_str).is_none() {
                return Err(ClientRequestError::Malformed(format!(
                    "toolCall is missing string field \"{field}\""
                )));
            }
        }
        let params = json!({
            "sessionId": &*self.session_id,
            "toolCall": tool_call,
            "options": options.iter().map(PermissionOption::to_wire).collect::<Vec<_>>(),
        });
        let result = self.request(REQUEST_PERMISSION_METHOD, params).await?;
        parse_permission_outcome(&result, options)
    }

    /// Official `fs/read_text_file`; refused with a typed error unless the
    /// client negotiated `fs.readTextFile`.
    pub async fn read_text_file(
        &self,
        path: &str,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, ClientRequestError> {
        if !self.negotiation.snapshot().fs_read_text_file {
            return Err(ClientRequestError::UnsupportedMethod(
                FS_READ_METHOD.to_string(),
            ));
        }
        let mut params = Map::new();
        params.insert("sessionId".into(), json!(&*self.session_id));
        params.insert("path".into(), json!(path));
        if let Some(line) = line {
            params.insert("line".into(), json!(line));
        }
        if let Some(limit) = limit {
            params.insert("limit".into(), json!(limit));
        }
        let result = self.request(FS_READ_METHOD, Value::Object(params)).await?;
        result
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                ClientRequestError::Malformed(
                    "fs/read_text_file result is missing string field \"content\"".into(),
                )
            })
    }

    /// Official `fs/write_text_file`; refused with a typed error unless the
    /// client negotiated `fs.writeTextFile`.
    pub async fn write_text_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), ClientRequestError> {
        if !self.negotiation.snapshot().fs_write_text_file {
            return Err(ClientRequestError::UnsupportedMethod(
                FS_WRITE_METHOD.to_string(),
            ));
        }
        let params = json!({
            "sessionId": &*self.session_id,
            "path": path,
            "content": content,
        });
        self.request(FS_WRITE_METHOD, params).await.map(|_| ())
    }

    /// One bounded JSON-RPC request to the client. Params are capped at
    /// [`MAX_PARAMS_BYTES`], concurrent requests at
    /// [`AcpConfig::max_client_requests`], and the wait at
    /// [`AcpConfig::client_request_timeout`] or the turn's cancellation.
    async fn request(&self, method: &str, params: Value) -> Result<Value, ClientRequestError> {
        if self.token.is_cancelled() {
            return Err(ClientRequestError::Cancelled);
        }
        let body_len = serde_json::to_vec(&params)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if body_len > MAX_PARAMS_BYTES {
            return Err(ClientRequestError::TooLarge);
        }
        let (id, rx) = self.outstanding.register()?;
        let frame = body_or_internal(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let sent = tokio::select! {
            biased;
            _ = self.token.cancelled() => {
                self.outstanding.remove(id);
                return Err(ClientRequestError::Cancelled);
            }
            sent = self.main_tx.send(frame) => sent,
        };
        if sent.is_err() {
            self.outstanding.remove(id);
            return Err(ClientRequestError::Closed);
        }
        let response = tokio::select! {
            biased;
            _ = self.token.cancelled() => Err(ClientRequestError::Cancelled),
            response = tokio::time::timeout(self.timeout, rx) => match response {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(_recv)) => Err(ClientRequestError::Closed),
                Err(_elapsed) => Err(ClientRequestError::Timeout),
            },
        };
        self.outstanding.remove(id);
        parse_client_response(response?)
    }

    /// Convenience: map the ACP outcome onto the native allow/deny decision
    /// vocabulary used by `faktor-agent`'s `PermissionRequester` adapter:
    /// allow options are allowed, reject/cancelled/timed-out outcomes are
    /// denied. Returns `None` when the client failed to answer in contract.
    pub fn permission_allows(outcome: &Result<PermissionOutcome, ClientRequestError>) -> bool {
        matches!(outcome, Ok(outcome) if outcome
            .selected_option_id()
            .is_some_and(|id| id.starts_with("allow")))
    }
}

/// Parse a client response value into its `result`, or a typed RPC error.
fn parse_client_response(value: Value) -> Result<Value, ClientRequestError> {
    if let Some(result) = value.get("result") {
        return Ok(result.clone());
    }
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64).ok_or_else(|| {
            ClientRequestError::Malformed("error response without a numeric code".into())
        })?;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Err(ClientRequestError::Rpc { code, message });
    }
    Err(ClientRequestError::Malformed(
        "response has neither \"result\" nor \"error\"".into(),
    ))
}

/// Validate the official permission outcome against the offered options.
fn parse_permission_outcome(
    result: &Value,
    options: &[PermissionOption],
) -> Result<PermissionOutcome, ClientRequestError> {
    let outcome = result
        .get("outcome")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ClientRequestError::Malformed(
                "permission result is missing object field \"outcome\"".into(),
            )
        })?;
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("cancelled") => Ok(PermissionOutcome::cancelled()),
        Some("selected") => {
            let option_id = outcome
                .get("optionId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ClientRequestError::Malformed(
                        "selected permission outcome is missing string field \"optionId\"".into(),
                    )
                })?;
            if !options.iter().any(|option| option.option_id == option_id) {
                return Err(ClientRequestError::Malformed(format!(
                    "selected option {option_id:?} was not offered"
                )));
            }
            Ok(PermissionOutcome::selected(option_id))
        }
        _ => Err(ClientRequestError::Malformed(
            "permission outcome must be \"selected\" or \"cancelled\"".into(),
        )),
    }
}

/// Streaming context handed to [`AcpStreamBackend::prompt`]: the only way
/// a running turn puts frames on the wire, plus cancellation observation,
/// the negotiated [`ClientCapabilities`], and the owned [`ClientHandle`]
/// for permission/client-filesystem round trips.
#[derive(Clone)]
pub struct PromptCtx {
    main_tx: mpsc::Sender<Vec<u8>>,
    session_id: Arc<str>,
    token: CancelToken,
    negotiation: Negotiation,
    outstanding: Outstanding,
    client_timeout: Duration,
}

impl PromptCtx {
    fn new(
        main_tx: mpsc::Sender<Vec<u8>>,
        session_id: Arc<str>,
        token: CancelToken,
        negotiation: Negotiation,
        outstanding: Outstanding,
        client_timeout: Duration,
    ) -> Self {
        Self {
            main_tx,
            session_id,
            token,
            negotiation,
            outstanding,
            client_timeout,
        }
    }

    /// Emit one `session/update` notification for the current session.
    /// Resolves once the frame is queued on the bounded main queue;
    /// resolves with `Cancelled` as soon as the turn's token fires — a
    /// mid-frame cancel stops further frames without waiting for space.
    /// Extension frames (updates carrying a `kind` member) require the
    /// matching `faktor.<kind>` declaration and return
    /// [`EmitError::NotNegotiated`] otherwise.
    pub async fn emit(&self, update: Value) -> Result<(), EmitError> {
        if let Some(kind) = update.get("kind").and_then(Value::as_str) {
            let extension = format!("faktor.{kind}");
            if !self.negotiation.has_extension(&extension) {
                return Err(EmitError::NotNegotiated);
            }
        }
        if self.token.is_cancelled() {
            return Err(EmitError::Cancelled);
        }
        let params = session_update_params(&self.session_id, update);
        let body_len = serde_json::to_vec(&params)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if body_len > MAX_RESPONSE_BYTES {
            return Err(EmitError::TooLarge);
        }
        let frame = notification_body_bytes("session/update", &params);
        tokio::select! {
            biased;
            _ = self.token.cancelled() => Err(EmitError::Cancelled),
            sent = self.main_tx.send(frame) => {
                sent.map_err(|_| EmitError::Closed)
            }
        }
    }

    /// Convenience: emit a [`text_chunk_update`] frame.
    pub async fn emit_text(&self, text: &str) -> Result<(), EmitError> {
        self.emit(text_chunk_update(text)).await
    }

    /// Convenience: emit an [`agent_state_changed_update`] status frame.
    /// This is an extension frame: for clients that did not declare
    /// `faktor.agentStateChanged` it is suppressed and reported as success
    /// (the turn is not disturbed by an unnegotiated optional frame).
    pub async fn emit_agent_state(
        &self,
        status: AgentStateStatus,
        message: Option<&str>,
    ) -> Result<(), EmitError> {
        match self.emit(agent_state_changed_update(status, message)).await {
            Err(EmitError::NotNegotiated) => Ok(()),
            other => other,
        }
    }

    /// An owned handle for server→client requests scoped to this turn.
    pub fn client(&self) -> ClientHandle {
        ClientHandle {
            main_tx: self.main_tx.clone(),
            session_id: self.session_id.clone(),
            token: self.token.clone(),
            negotiation: self.negotiation.clone(),
            outstanding: self.outstanding.clone(),
            timeout: self.client_timeout,
        }
    }

    /// The client capabilities negotiated for this connection.
    pub fn client_capabilities(&self) -> ClientCapabilities {
        self.client().capabilities()
    }

    /// Convenience: official `session/request_permission` for this session.
    pub async fn request_permission(
        &self,
        tool_call: &Value,
        options: &[PermissionOption],
    ) -> Result<PermissionOutcome, ClientRequestError> {
        self.client().request_permission(tool_call, options).await
    }

    /// Resolves when the turn is cancelled (same as the token).
    pub async fn cancelled(&self) {
        self.token.cancelled().await
    }

    /// Whether the turn is cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

/// What a backend honestly reports it can do. The all-false default is the
/// conservative minimum: `initialize` advertises only what is set here, so
/// a backend that does not override this never claims a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackendCapabilities {
    /// The backend implements [`AcpBackend::load_session`] and can produce a
    /// bounded native message history for every session it owns.
    pub load_session: bool,
    /// The backend can serve MCP servers over HTTP.
    pub mcp_http: bool,
    /// The backend can serve MCP servers over SSE.
    pub mcp_sse: bool,
}

/// Spawn specification of one `terminal/create`: bounded command/args/cwd,
/// an environment ALLOWLIST OF NAMES (never values, never the daemon
/// environment), and the initial window geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Environment names the authority may copy from the daemon (after its
    /// own deny-set); names, never values.
    pub env: Vec<String>,
    pub rows: u16,
    pub cols: u16,
}

/// Why one terminal authority operation failed. [`TerminalError::Invalid`]
/// maps to the official `-32602`; the other variants map to `-32603` with
/// the message in `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalError {
    /// The request is invalid (bad geometry, NUL, unknown terminal, ...).
    Invalid(String),
    /// The authority refused the operation (spawn failed, process gone).
    Refused(String),
    /// The authority is unavailable for this operation.
    Unavailable(String),
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TerminalError::Invalid(message) => write!(f, "{message}"),
            TerminalError::Refused(message) => write!(f, "terminal refused: {message}"),
            TerminalError::Unavailable(message) => write!(f, "terminal unavailable: {message}"),
        }
    }
}

impl std::error::Error for TerminalError {}

/// One live terminal handed back by a [`TerminalAuthority`]. Implementations
/// must be fast and internally bounded: `drain_output` never blocks, `kill`
/// is idempotent and terminates the whole process tree, and dropping the
/// last handle kills the tree as the final failsafe.
pub trait TerminalHandle: Send + Sync {
    /// The authority-unique terminal id (the ownership row's key).
    fn terminal_id(&self) -> &str;
    /// The child pid at spawn.
    fn pid(&self) -> u32;
    /// Whether the child process tree is still alive.
    fn is_alive(&self) -> bool;
    /// The authority's ownership id for this row (daemon op/pty identity).
    fn ownership_id(&self) -> &str;
    /// Write raw bytes to the terminal.
    fn write(&self, bytes: &[u8]) -> Result<(), TerminalError>;
    /// Resize the terminal window.
    fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalError>;
    /// Drain all currently available output (bounded by the authority's
    /// ring; never blocking).
    fn drain_output(&self) -> Vec<u8>;
    /// Kill the whole process tree (idempotent).
    fn kill(&self) -> Result<(), TerminalError>;
}

/// One durable session-owned terminal row as reported by a
/// [`TerminalAuthority`] that exposes its durable store. This is the ACP
/// projection of the authority's own rows — the authority remains the
/// single source of truth; ACP never invents a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAuthorityRow {
    /// The authority's terminal UUID (the durable row key).
    pub terminal_id: String,
    /// The owning session.
    pub session_id: String,
    /// The owning session's durable task identity.
    pub task_id: String,
    /// The orchestrator child agent that spawned the terminal, when one did.
    pub agent_id: Option<String>,
    /// The durable operation id of the spawn.
    pub operation_id: String,
    /// The child pid at spawn (0 = never observed).
    pub pid: u32,
    /// The OS process start-time marker (0 = unverified identity).
    pub start_time_ms: i64,
    /// The durable lifecycle state (`created|running|exited|killed|lost|
    /// reconciled`).
    pub state: String,
    /// Whether a live pty authority of this daemon boot owns the row.
    pub alive: bool,
}

/// The terminal seam: the daemon attaches its existing session-owned
/// terminal authority (faktor-pty + durable rows) here. This crate owns no
/// PTY of its own; absent an authority, the `faktor.terminal` extension is
/// not negotiated and every `terminal/*` request answers `-32601`.
pub trait TerminalAuthority: Send + Sync {
    /// Create a session-owned terminal. `session_id` is the session the
    /// caller proved ownership of; the authority must enforce its own
    /// environment deny-set regardless of `spec.env`.
    fn create(
        &self,
        session_id: &str,
        spec: &TerminalSpec,
    ) -> Result<Arc<dyn TerminalHandle>, TerminalError>;

    /// Whether this authority exposes its durable terminal rows
    /// ([`Self::list_terminals`]). The default is `false`: a create-only
    /// authority keeps the ACP connection table as its listing surface.
    fn exposes_durable_terminals(&self) -> bool {
        false
    }

    /// The session's durable terminal rows, scope-enforced by the
    /// authority. Only called when [`Self::exposes_durable_terminals`] is
    /// true; the default reports the seam does not expose a listing.
    fn list_terminals(
        &self,
        _session_id: &str,
    ) -> Result<Vec<TerminalAuthorityRow>, TerminalError> {
        Err(TerminalError::Unavailable(
            "this terminal authority does not expose its durable rows".into(),
        ))
    }
}

impl<T: TerminalAuthority + ?Sized> TerminalAuthority for Arc<T> {
    fn create(
        &self,
        session_id: &str,
        spec: &TerminalSpec,
    ) -> Result<Arc<dyn TerminalHandle>, TerminalError> {
        (**self).create(session_id, spec)
    }

    fn exposes_durable_terminals(&self) -> bool {
        (**self).exposes_durable_terminals()
    }

    fn list_terminals(&self, session_id: &str) -> Result<Vec<TerminalAuthorityRow>, TerminalError> {
        (**self).list_terminals(session_id)
    }
}

/// Per-connection terminal registry: session-scoped ownership rows, bounded
/// by [`AcpConfig::max_terminals`], each with a coalescing output pump. Owned
/// by `serve_connection`; [`TerminalRegistry::shutdown`] runs on every
/// connection wind-down so no terminal outlives its connection.
#[derive(Clone)]
struct TerminalRegistry {
    authority: Option<Arc<dyn TerminalAuthority>>,
    sessions: Arc<Mutex<HashSet<String>>>,
    table: Arc<Mutex<TerminalTable>>,
    config: AcpConfig,
}

#[derive(Default)]
struct TerminalTable {
    entries: HashMap<String, LiveTerminal>,
}

struct LiveTerminal {
    session_id: String,
    handle: Arc<dyn TerminalHandle>,
    stats: Arc<TerminalStats>,
    kill_tx: Option<oneshot::Sender<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
}

/// Bounded per-terminal output accounting (never unbounded).
#[derive(Default)]
struct TerminalStats {
    emitted_frames: AtomicU64,
    dropped_bytes: AtomicU64,
    backpressure_events: AtomicU64,
}

impl TerminalRegistry {
    fn new(authority: Option<Arc<dyn TerminalAuthority>>, config: AcpConfig) -> Self {
        Self {
            authority,
            sessions: Arc::new(Mutex::new(HashSet::new())),
            table: Arc::new(Mutex::new(TerminalTable::default())),
            config,
        }
    }

    fn has_authority(&self) -> bool {
        self.authority.is_some()
    }

    /// Record a session created through this connection. The index is
    /// capped like the session registry; beyond the cap new sessions stay
    /// untracked and their terminal requests answer the typed unknown-
    /// session denial (bounded, loud, never silent).
    fn note_session(&self, session_id: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if sessions.len() < self.config.max_sessions {
            sessions.insert(session_id.to_string());
        }
    }

    fn knows_session(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(session_id)
    }

    /// Register a freshly created handle, spawn its output pump, and return
    /// the `terminal/create` result. A hostile authority id (empty, NUL,
    /// oversized, duplicate) is refused and the handle is killed — never
    /// leaked.
    fn register(
        &self,
        session_id: &str,
        handle: Arc<dyn TerminalHandle>,
        main_tx: mpsc::Sender<Vec<u8>>,
        negotiation: Negotiation,
    ) -> Result<Value, ServerError> {
        let terminal_id = handle.terminal_id().to_string();
        let invalid_id = terminal_id.is_empty()
            || terminal_id.len() > MAX_TERMINAL_ID_BYTES
            || terminal_id.contains('\0');
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if invalid_id {
            drop(table);
            let _ = handle.kill();
            return Err(ServerError::internal(
                "terminal authority returned an invalid terminal id".to_string(),
            ));
        }
        if table.entries.len() >= self.config.max_terminals {
            drop(table);
            let _ = handle.kill();
            return Err(ServerError {
                code: TERMINAL_LIMIT,
                message: MSG_TERMINAL_LIMIT.to_string(),
                data: None,
            });
        }
        if table.entries.contains_key(&terminal_id) {
            drop(table);
            let _ = handle.kill();
            return Err(ServerError::internal(format!(
                "terminal authority returned duplicate terminal id {terminal_id:?}"
            )));
        }
        let stats = Arc::new(TerminalStats::default());
        let (kill_tx, kill_rx) = oneshot::channel();
        let pump = spawn_terminal_pump(
            Arc::clone(&handle),
            Arc::clone(&stats),
            kill_rx,
            session_id.to_string(),
            terminal_id.clone(),
            main_tx,
            negotiation,
            self.config,
        );
        table.entries.insert(
            terminal_id.clone(),
            LiveTerminal {
                session_id: session_id.to_string(),
                handle: Arc::clone(&handle),
                stats,
                kill_tx: Some(kill_tx),
                pump: Some(pump),
            },
        );
        Ok(json!({
            "terminalId": terminal_id,
            "pid": handle.pid(),
            "sessionId": session_id,
            "ownershipId": handle.ownership_id(),
        }))
    }

    /// Resolve `(sessionId, terminalId)` to its live handle. A terminal of
    /// another session is reported exactly like an unknown terminal: typed
    /// invalid params, never a silent success and never an existence leak.
    fn handle_for(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<Arc<dyn TerminalHandle>, ServerError> {
        if !self.knows_session(session_id) {
            return Err(unknown_session(session_id));
        }
        let table = self
            .table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match table.entries.get(terminal_id) {
            Some(entry) if entry.session_id == session_id => Ok(Arc::clone(&entry.handle)),
            _ => Err(unknown_terminal(session_id, terminal_id)),
        }
    }

    /// Remove one terminal row, returning its handle, pump cancellation and
    /// stats (the caller kills the handle). The row is gone even if the kill
    /// is best-effort, so a foreign caller can never reach it again.
    fn take(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<(Arc<dyn TerminalHandle>, Arc<TerminalStats>), ServerError> {
        if !self.knows_session(session_id) {
            return Err(unknown_session(session_id));
        }
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owned = matches!(
            table.entries.get(terminal_id),
            Some(entry) if entry.session_id == session_id
        );
        if !owned {
            return Err(unknown_terminal(session_id, terminal_id));
        }
        let mut entry = table
            .entries
            .remove(terminal_id)
            .expect("entry was just observed");
        if let Some(kill_tx) = entry.kill_tx.take() {
            let _ = kill_tx.send(());
        }
        if let Some(pump) = entry.pump.take() {
            pump.abort();
        }
        Ok((entry.handle, entry.stats))
    }

    /// The requesting session's OWN rows, sorted by terminal id for a stable
    /// projection. Foreign rows are never included.
    fn list(&self, session_id: &str) -> Result<Vec<Value>, ServerError> {
        if !self.knows_session(session_id) {
            return Err(unknown_session(session_id));
        }
        let table = self
            .table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut ids: Vec<&String> = table
            .entries
            .iter()
            .filter(|(_, entry)| entry.session_id == session_id)
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        Ok(ids
            .into_iter()
            .map(|id| {
                let entry = table.entries.get(id).expect("id came from the table");
                json!({
                    "terminalId": id,
                    "pid": entry.handle.pid(),
                    "alive": entry.handle.is_alive(),
                    "sessionId": entry.session_id,
                    "ownershipId": entry.handle.ownership_id(),
                    "emittedFrames": entry.stats.emitted_frames.load(Ordering::Relaxed),
                    "droppedBytes": entry.stats.dropped_bytes.load(Ordering::Relaxed),
                    "backpressureEvents": entry.stats.backpressure_events.load(Ordering::Relaxed),
                })
            })
            .collect())
    }

    /// Cancel every pump and kill every handle. Runs on every connection
    /// wind-down (EOF/shutdown/writer death) before the serve loop returns,
    /// so a PTY child never outlives its ACP connection; dropping the last
    /// handle stays the emergency failsafe.
    async fn shutdown(&self) {
        let entries: Vec<LiveTerminal> = {
            let mut table = self
                .table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            table.entries.drain().map(|(_, entry)| entry).collect()
        };
        let mut handles: Vec<Arc<dyn TerminalHandle>> = Vec::with_capacity(entries.len());
        for mut entry in entries {
            if let Some(kill_tx) = entry.kill_tx.take() {
                let _ = kill_tx.send(());
            }
            if let Some(pump) = entry.pump.take() {
                pump.abort();
            }
            handles.push(entry.handle);
        }
        if !handles.is_empty() {
            let _ = tokio::task::spawn_blocking(move || {
                for handle in &handles {
                    let _ = handle.kill();
                }
            })
            .await;
        }
    }
}

fn unknown_session(session_id: &str) -> ServerError {
    ServerError::invalid_params(format!("unknown session {session_id:?}"))
}

fn unknown_terminal(session_id: &str, terminal_id: &str) -> ServerError {
    ServerError::invalid_params(format!(
        "unknown terminal {terminal_id:?} for session {session_id:?}"
    ))
}

/// One terminal's coalescing output pump. Every tick it drains the
/// authority's bounded ring, emits `terminalOutput` frames of at most
/// `terminal_output_frame_max` raw bytes and at most
/// `terminal_output_window_max` raw bytes per window, and records every
/// dropped byte/event (frames are `try_send`-ed: the pump NEVER blocks the
/// writer queue, so a slow client can only cost recorded terminal loss, not
/// a deadlock). Per-terminal frame order is preserved by construction.
#[allow(clippy::too_many_arguments)]
fn spawn_terminal_pump(
    handle: Arc<dyn TerminalHandle>,
    stats: Arc<TerminalStats>,
    mut kill_rx: oneshot::Receiver<()>,
    session_id: String,
    terminal_id: String,
    main_tx: mpsc::Sender<Vec<u8>>,
    negotiation: Negotiation,
    config: AcpConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = config.terminal_poll_interval.max(Duration::from_millis(1));
        let frame_max = config.terminal_output_frame_max.max(1);
        let window_max = config.terminal_output_window_max.max(frame_max);
        let mut seq: u64 = 0;
        let mut pending_dropped: u64 = 0;
        let mut pending_backpressure: u64 = 0;
        loop {
            tokio::select! {
                _ = &mut kill_rx => break,
                _ = tokio::time::sleep(interval) => {}
            }
            // A re-initialize can un-negotiate the extension; the pump then
            // holds the bounded ring (never unbounded buffering) instead of
            // emitting unnegotiated extension frames.
            if !negotiation.has_extension(EXTENSION_TERMINAL) {
                continue;
            }
            let raw = handle.drain_output();
            if raw.is_empty() {
                continue;
            }
            let mut offset = 0usize;
            let mut window_used = 0usize;
            while offset < raw.len() {
                let window_left = window_max.saturating_sub(window_used);
                if window_left == 0 {
                    let dropped = (raw.len() - offset) as u64;
                    stats.dropped_bytes.fetch_add(dropped, Ordering::Relaxed);
                    stats.backpressure_events.fetch_add(1, Ordering::Relaxed);
                    pending_dropped += dropped;
                    pending_backpressure += 1;
                    break;
                }
                let take = (raw.len() - offset).min(frame_max).min(window_left);
                let chunk = &raw[offset..offset + take];
                offset += take;
                window_used += take;
                let data = String::from_utf8_lossy(chunk);
                let update = terminal_output_update(
                    &terminal_id,
                    seq,
                    &data,
                    take,
                    pending_dropped,
                    pending_backpressure,
                );
                let params = session_update_params(&session_id, update);
                let frame = notification_body_bytes("session/update", &params);
                match main_tx.try_send(frame) {
                    Ok(()) => {
                        seq += 1;
                        stats.emitted_frames.fetch_add(1, Ordering::Relaxed);
                        pending_dropped = 0;
                        pending_backpressure = 0;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        stats
                            .dropped_bytes
                            .fetch_add(take as u64, Ordering::Relaxed);
                        stats.backpressure_events.fetch_add(1, Ordering::Relaxed);
                        pending_dropped += take as u64;
                        pending_backpressure += 1;
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        }
    })
}

/// Why `session/load` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadSessionError {
    /// The backend does not implement `session/load`.
    Unsupported,
    /// The session is unknown or not owned by this server (official
    /// `-32602`, never a silent empty replay).
    NotFound,
    /// The session exists but bounded history could not be produced
    /// (official `-32603`).
    Unavailable(String),
}

/// The seam: deterministic, injectable agent behavior. The daemon attaches
/// the real Faktor runtime here; this crate only wires JSON-RPC to it.
///
/// The synchronous surface is the pre-conformance contract: `prompt` runs
/// as one blocking turn on the session's operation task (implementations
/// must not block indefinitely and should be internally cancellable via
/// [`AcpBackend::abort`]). Prefer the additive async surface
/// [`AcpStreamBackend`] for cancellable, streaming turns.
pub trait AcpBackend: Send + Sync {
    /// Agent metadata surfaced by the `agent_info` extension
    /// (e.g. `{"name": .., "version": ..}`).
    fn agent_info(&self) -> Value;
    /// Create a session; `Err` surfaces as `-32603` (internal error).
    fn create_session(&self, params: &Value) -> Result<String, String>;
    /// Run one prompt turn against `session_id`; `Err` surfaces as
    /// `-32603`.
    fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String>;
    /// Best-effort abort of the active turn of `session_id`; invoked from
    /// the cancel path when a cancel lands on a running sync turn.
    fn abort(&self, session_id: &str) -> Result<(), String>;
    /// Current session ids (e.g. for the `session/list` extension).
    fn list_sessions(&self) -> Vec<String>;
    /// Honest capability profile (see [`BackendCapabilities`]).
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }
    /// The bounded native message history of a session this backend owns,
    /// as the frozen `faktor_protocol::native::MessagesPage`. Implementations
    /// must refuse ([`LoadSessionError::NotFound`]) for sessions they do not
    /// own and must only report [`BackendCapabilities::load_session`] when
    /// this hook can produce a complete bounded page.
    fn load_session(&self, session_id: &str) -> Result<MessagesPage, LoadSessionError> {
        let _ = session_id;
        Err(LoadSessionError::Unsupported)
    }
}

impl<T: AcpBackend + ?Sized> AcpBackend for Arc<T> {
    fn agent_info(&self) -> Value {
        (**self).agent_info()
    }
    fn create_session(&self, params: &Value) -> Result<String, String> {
        (**self).create_session(params)
    }
    fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
        (**self).prompt(session_id, text)
    }
    fn abort(&self, session_id: &str) -> Result<(), String> {
        (**self).abort(session_id)
    }
    fn list_sessions(&self) -> Vec<String> {
        (**self).list_sessions()
    }
    fn capabilities(&self) -> BackendCapabilities {
        (**self).capabilities()
    }
    fn load_session(&self, session_id: &str) -> Result<MessagesPage, LoadSessionError> {
        (**self).load_session(session_id)
    }
}

/// Additive async seam: a backend whose prompt runs are streams of frames
/// that observe the crate's cancellation token. This is the surface on
/// which mid-frame cancellation is guaranteed: once the token fires,
/// [`PromptCtx::emit`] stops resolving successfully, the backend returns,
/// and the operation task answers the original prompt id with
/// `{stopReason: "cancelled"}` after at most [`AcpConfig::cancel_grace`].
pub trait AcpStreamBackend: Send + Sync {
    /// Agent metadata (see [`AcpBackend::agent_info`]).
    fn agent_info(&self) -> Value;
    /// Create a session (see [`AcpBackend::create_session`]).
    fn create_session(&self, params: &Value) -> Result<String, String>;
    /// Current session ids.
    fn list_sessions(&self) -> Vec<String>;
    /// Honest capability profile (see [`BackendCapabilities`]).
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }
    /// The bounded native message history of a session this backend owns
    /// (see [`AcpBackend::load_session`]).
    fn load_session(&self, session_id: &str) -> Result<MessagesPage, LoadSessionError> {
        let _ = session_id;
        Err(LoadSessionError::Unsupported)
    }
    /// Run one prompt turn. `ctx` is the only way to stream frames and the
    /// only cancellation observation point. Returning before cancellation
    /// yields the `end_turn`/error outcome; returning after cancellation
    /// still yields `stopReason: cancelled` (cancellation always wins the
    /// race at the turn's terminal decision point).
    fn prompt<'a>(
        &'a self,
        session_id: &'a str,
        ctx: &'a PromptCtx,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>>;
}

impl<T: AcpStreamBackend + ?Sized> AcpStreamBackend for Arc<T> {
    fn agent_info(&self) -> Value {
        (**self).agent_info()
    }
    fn create_session(&self, params: &Value) -> Result<String, String> {
        (**self).create_session(params)
    }
    fn list_sessions(&self) -> Vec<String> {
        (**self).list_sessions()
    }
    fn capabilities(&self) -> BackendCapabilities {
        (**self).capabilities()
    }
    fn load_session(&self, session_id: &str) -> Result<MessagesPage, LoadSessionError> {
        (**self).load_session(session_id)
    }
    fn prompt<'a>(
        &'a self,
        session_id: &'a str,
        ctx: &'a PromptCtx,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        (**self).prompt(session_id, ctx, text)
    }
}

/// One session's prompt state: at most one running turn plus one queued
/// prompt (FIFO); everything else is refused with the typed busy error.
#[derive(Debug, Default)]
struct SessionState {
    active: Option<ActiveTurn>,
}

#[derive(Debug)]
struct ActiveTurn {
    /// The id of the running prompt, so `$/cancel_request` can match it
    /// (the session-scoped `session/cancel` does not need it).
    request_id: RequestId,
    token: CancelToken,
    queued: Option<PromptJob>,
}

#[derive(Debug)]
struct PromptJob {
    id: RequestId,
    text: String,
}

/// Outcome of one prompt turn at its terminal decision point.
#[derive(Debug)]
enum TurnOutcome {
    Completed(Value),
    Failed(String),
    Cancelled,
}

#[derive(Clone)]
enum Engine {
    Sync(Arc<dyn AcpBackend>),
    Stream(Arc<dyn AcpStreamBackend>),
}

impl Engine {
    fn agent_info(&self) -> Value {
        match self {
            Engine::Sync(b) => b.agent_info(),
            Engine::Stream(b) => b.agent_info(),
        }
    }

    fn create_session(&self, params: &Value) -> Result<String, String> {
        match self {
            Engine::Sync(b) => b.create_session(params),
            Engine::Stream(b) => b.create_session(params),
        }
    }

    fn list_sessions(&self) -> Vec<String> {
        match self {
            Engine::Sync(b) => b.list_sessions(),
            Engine::Stream(b) => b.list_sessions(),
        }
    }

    fn capabilities(&self) -> BackendCapabilities {
        match self {
            Engine::Sync(b) => b.capabilities(),
            Engine::Stream(b) => b.capabilities(),
        }
    }

    fn load_session(&self, session_id: &str) -> Result<MessagesPage, LoadSessionError> {
        match self {
            Engine::Sync(b) => b.load_session(session_id),
            Engine::Stream(b) => b.load_session(session_id),
        }
    }

    fn is_sync(&self) -> bool {
        matches!(self, Engine::Sync(_))
    }

    /// Run one full turn. Stream backends run as a cancellable future;
    /// sync backends run their blocking call on this task exactly like the
    /// pre-conformance loop (their `abort` hook carries cancellation).
    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        session_id: &str,
        job: &PromptJob,
        main_tx: mpsc::Sender<Vec<u8>>,
        token: CancelToken,
        negotiation: Negotiation,
        outstanding: Outstanding,
        config: AcpConfig,
    ) -> TurnOutcome {
        match self {
            Engine::Sync(backend) => {
                // The blocking call runs on the blocking pool, NOT on the
                // async worker, and the caller races it against the session
                // token: a cancel returns `cancelled` at once while the
                // backend's own `abort` hook (fired by the cancel path) is
                // what actually stops the work. A backend that ignores both
                // is still bounded by its contract (must not block
                // indefinitely).
                let backend = backend.clone();
                let session = session_id.to_string();
                let text = job.text.clone();
                let blocking = tokio::task::spawn_blocking(move || backend.prompt(&session, &text));
                tokio::select! {
                    biased;
                    _ = token.cancelled() => TurnOutcome::Cancelled,
                    result = blocking => match result {
                        Ok(Ok(value)) => {
                            if token.is_cancelled() {
                                TurnOutcome::Cancelled
                            } else {
                                TurnOutcome::Completed(value)
                            }
                        }
                        Ok(Err(message)) => {
                            if token.is_cancelled() {
                                TurnOutcome::Cancelled
                            } else {
                                TurnOutcome::Failed(message)
                            }
                        }
                        Err(join) => TurnOutcome::Failed(format!("sync prompt task failed: {join}")),
                    },
                }
            }
            Engine::Stream(backend) => {
                let ctx = PromptCtx::new(
                    main_tx,
                    Arc::from(session_id),
                    token.clone(),
                    negotiation,
                    outstanding,
                    config.client_request_timeout,
                );
                let future = backend.prompt(session_id, &ctx, &job.text);
                let mut future = std::pin::pin!(future);
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        // Bounded wind-down: grant the backend a grace
                        // period to observe cancellation and return, then
                        // answer cancelled regardless.
                        match tokio::time::timeout(config.cancel_grace, &mut future).await {
                            Ok(_) => TurnOutcome::Cancelled,
                            Err(_elapsed) => {
                                tracing::debug!(
                                    session_id,
                                    grace_ms = config.cancel_grace.as_millis(),
                                    "acp: streaming backend did not stop within cancel grace"
                                );
                                TurnOutcome::Cancelled
                            }
                        }
                    }
                    result = &mut future => {
                        if token.is_cancelled() {
                            return TurnOutcome::Cancelled;
                        }
                        match result {
                            Ok(value) => TurnOutcome::Completed(value),
                            Err(message) => TurnOutcome::Failed(message),
                        }
                    }
                }
            }
        }
    }
}

/// One JSON-RPC message decoded from the transport.
#[derive(Debug)]
enum Incoming {
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// A response to a server→client request (permission, client fs). The
    /// id is `None` for a malformed response, which is logged and dropped
    /// (JSON-RPC forbids answering a response).
    Response {
        id: Option<u64>,
        value: Value,
    },
    Invalid {
        id: Value,
        code: i64,
        message: String,
    },
}

/// Decode one parsed message into a request, a notification, a response, or
/// a well-formed error response to send back (JSON-RPC 2.0 §5.1 semantics).
fn classify(value: Value) -> Incoming {
    fn invalid(id: Value, code: i64, message: impl Into<String>) -> Incoming {
        Incoming::Invalid {
            id,
            code,
            message: message.into(),
        }
    }

    let Some(obj) = value.as_object() else {
        return invalid(Value::Null, INVALID_REQUEST, "message is not a JSON object");
    };
    match obj.get("jsonrpc") {
        Some(Value::String(s)) if s != "2.0" => {
            return invalid(
                Value::Null,
                INVALID_REQUEST,
                "jsonrpc version must be \"2.0\"",
            )
        }
        _ => {}
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // No method: a response to one of our server→client requests, or
        // invalid JSON-RPC. Responses are never answered.
        let is_response = obj.contains_key("result") || obj.contains_key("error");
        let id = obj.get("id").and_then(Value::as_u64);
        if is_response {
            return Incoming::Response { id, value };
        }
        return invalid(
            Value::Null,
            INVALID_REQUEST,
            "missing string field \"method\"",
        );
    };
    if method.len() > MAX_METHOD_LEN {
        return invalid(
            Value::Null,
            INVALID_REQUEST,
            format!("method exceeds {MAX_METHOD_LEN}-byte bound"),
        );
    }
    let method = method.to_string();

    let id_field = obj.get("id");
    if id_field.is_none() || id_field.is_some_and(Value::is_null) {
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        return Incoming::Notification { method, params };
    }

    // A request: id must be a non-negative integer or a string (the
    // official SDK sends UUID strings). Echo the original id when it was
    // numeric but not representable; null otherwise.
    let id_echo = match id_field {
        Some(v @ Value::Number(_)) => v.clone(),
        _ => Value::Null,
    };
    let Some(id) = id_field.and_then(RequestId::from_value) else {
        return invalid(
            id_echo,
            INVALID_REQUEST,
            "request id must be a non-negative integer or a string",
        );
    };

    let params = match obj.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(p) if p.is_object() => p.clone(),
        Some(_) => {
            return invalid(
                id.to_value(),
                INVALID_PARAMS,
                "params must be a JSON object",
            )
        }
    };
    if serde_json::to_vec(&params)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
        > MAX_PARAMS_BYTES
    {
        return invalid(
            id.to_value(),
            INVALID_REQUEST,
            "request params exceed the 1 MiB params bound",
        );
    }
    Incoming::Request { id, method, params }
}

/// ACP agent server. Cheap to build; `serve_connection`/`run_stdio` own
/// the connection until EOF or `shutdown`.
#[derive(Clone)]
pub struct AcpServer {
    engine: Engine,
    config: AcpConfig,
    terminal_authority: Option<Arc<dyn TerminalAuthority>>,
}

impl AcpServer {
    /// Serve with the synchronous backend seam (pre-conformance contract,
    /// kept source-compatible).
    pub fn new<B: AcpBackend + 'static>(backend: B) -> Self {
        Self {
            engine: Engine::Sync(Arc::new(backend)),
            config: AcpConfig::default(),
            terminal_authority: None,
        }
    }

    /// Serve with the additive streaming backend seam.
    pub fn new_streaming<B: AcpStreamBackend + 'static>(backend: B) -> Self {
        Self {
            engine: Engine::Stream(Arc::new(backend)),
            config: AcpConfig::default(),
            terminal_authority: None,
        }
    }

    /// Attach the daemon's session-owned terminal authority. Only then can
    /// `faktor.terminal` be negotiated: a client that declares the extension
    /// gets it echoed and the `terminal/*` table; without an authority the
    /// extension is not granted and every `terminal/*` request answers the
    /// official `-32601`. The crate implements no PTY itself — the authority
    /// is where `faktor-pty` and the daemon's ownership rows live.
    pub fn with_terminal_authority(mut self, authority: Arc<dyn TerminalAuthority>) -> Self {
        self.terminal_authority = Some(authority);
        self
    }

    /// Override the sizing/behavior knobs.
    pub fn with_config(mut self, config: AcpConfig) -> Self {
        self.config = config;
        self
    }

    /// Serve ACP over an arbitrary async reader/writer pair (in-memory
    /// pipes for tests, stdio for production). Reader, dispatcher,
    /// per-session operation tasks and the writer task run concurrently
    /// (module docs). Returns `Ok(())` on EOF or `shutdown`.
    pub async fn serve_connection<R, W>(&self, mut reader: R, writer: W) -> Result<(), String>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let config = self.config;
        let (main_tx, main_rx) = mpsc::channel(config.writer_queue_capacity);
        let (lane_tx, lane_rx) = mpsc::channel(CANCEL_LANE_CAPACITY);
        let (request_tx, request_rx) = mpsc::channel(config.request_queue_capacity);
        let registry = Registry::new(config.max_sessions);
        let terminals = TerminalRegistry::new(self.terminal_authority.clone(), config);
        let negotiation = Negotiation::default();
        let outstanding = Outstanding::new(config.max_client_requests);

        // The writer frames every body in the mode detected from the peer's
        // first bytes. It waits for that decision before its first write;
        // until the peer sends something there is nothing to write.
        let (framing_tx, framing_rx) = tokio::sync::watch::channel(None);
        let writer_handle = tokio::spawn(writer_task(writer, framing_rx, main_rx, lane_rx));
        let dispatcher_handle = tokio::spawn(dispatcher_task(
            self.engine.clone(),
            registry.clone(),
            terminals.clone(),
            main_tx.clone(),
            negotiation.clone(),
            outstanding.clone(),
            config,
            request_rx,
        ));

        // Reader loop (this task): autodetect framing, parse, short-circuit
        // cancels, route responses to outstanding server→client requests,
        // forward everything else to the dispatcher.
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        let mut reader_error: Option<String> = None;
        let mut framing: Option<protocol::Framing> = None;
        'read: loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) => {
                    tracing::debug!("acp: input closed; ending serve loop");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    reader_error = Some(format!("read error: {e}"));
                    break;
                }
            };
            buf.extend_from_slice(&chunk[..n]);
            if framing.is_none() {
                if let Some(detected) = protocol::detect_framing(&buf) {
                    framing = Some(detected);
                    let _ = framing_tx.send(Some(detected));
                    tracing::debug!(framing = ?detected, "acp: connection framing detected");
                }
            }
            let mode = framing.unwrap_or(protocol::Framing::ContentLength);
            loop {
                let parsed = match mode {
                    protocol::Framing::Ndjson => match protocol::parse_ndjson_detailed(&buf) {
                        Ok(Some((consumed, Some(value)))) => Ok(Some((consumed, value))),
                        Ok(Some((consumed, None))) => {
                            // Blank keep-alive line: discard, keep parsing.
                            buf.drain(..consumed);
                            continue;
                        }
                        Ok(None) => Ok(None),
                        Err(err) => Err(err),
                    },
                    protocol::Framing::ContentLength => protocol::parse_frame_detailed(&buf),
                };
                match parsed {
                    Ok(Some((consumed, value))) => {
                        buf.drain(..consumed);
                        match classify(value) {
                            Incoming::Invalid { id, code, message } => {
                                let frame = error_frame_value(&id, code, &message, None);
                                if send_checked(&main_tx, frame).await.is_err() {
                                    break 'read;
                                }
                            }
                            Incoming::Response { id, value } => {
                                // A response is never answered. Deliver it
                                // to the matching waiter (bounded by the
                                // per-connection outstanding table); an
                                // unknown or malformed id is logged, never
                                // guessed.
                                match id {
                                    Some(id) if outstanding.resolve(id, value) => {}
                                    Some(id) => {
                                        tracing::debug!(id, "acp: response for unknown request id")
                                    }
                                    None => {
                                        tracing::warn!("acp: dropping response without a usable id")
                                    }
                                }
                            }
                            Incoming::Notification { method, params } => {
                                if method == SHUTDOWN_METHOD {
                                    break 'read;
                                }
                                if is_cancel_method(&method) {
                                    // Notification form: nothing to answer.
                                    self.handle_cancel(&params, None, &registry, &lane_tx).await;
                                } else if method == CANCEL_REQUEST_METHOD {
                                    // Request-level cancel (official SDK
                                    // drop semantics): notification form,
                                    // nothing to answer.
                                    self.handle_cancel_request(&params, None, &registry, &lane_tx)
                                        .await;
                                } else {
                                    tracing::info!(
                                        method = %method,
                                        params = %params,
                                        "acp: ignoring notification"
                                    );
                                }
                            }
                            Incoming::Request { id, method, params } => {
                                if method == SHUTDOWN_METHOD {
                                    let frame = result_frame(id, &json!({ "ok": true }));
                                    let _ = send_checked(&main_tx, frame).await;
                                    break 'read;
                                }
                                if is_cancel_method(&method) {
                                    // Short-circuit: cancel never waits
                                    // behind the dispatcher or the main
                                    // writer queue; acknowledgements use
                                    // the high-priority cancel lane.
                                    self.handle_cancel(&params, Some(id), &registry, &lane_tx)
                                        .await;
                                } else if method == CANCEL_REQUEST_METHOD {
                                    self.handle_cancel_request(
                                        &params,
                                        Some(id),
                                        &registry,
                                        &lane_tx,
                                    )
                                    .await;
                                } else if request_tx
                                    .send(Incoming::Request { id, method, params })
                                    .await
                                    .is_err()
                                {
                                    // Dispatcher gone (writer failed or
                                    // connection ending).
                                    break 'read;
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!(
                            error = %err.message,
                            fatal = err.fatal,
                            "acp: frame-level parse error"
                        );
                        // Official parse-error frame (null id), then either
                        // recover (byte boundary known) or end the stream.
                        let frame =
                            error_frame_value(&Value::Null, PARSE_ERROR, MSG_PARSE_ERROR, None);
                        if send_checked(&main_tx, frame).await.is_err() {
                            break 'read;
                        }
                        if err.fatal {
                            break 'read;
                        }
                        buf.drain(..err.consumed.min(buf.len()));
                    }
                }
            }
        }

        // Wind-down: close the registry, cancel every running turn, fail
        // every outstanding server→client request with Closed, kill every
        // session-owned terminal (pumps cancelled, process trees killed)
        // under an explicit bound, drop our queue handles, then ABORT+join
        // every outstanding task (operation turns, dispatcher, writer) so
        // nothing can write after `serve_connection` returns.
        let mut result = match reader_error {
            Some(e) => Err(e),
            None => Ok(()),
        };
        let timeout = config.shutdown_timeout;
        registry.cancel_all();
        outstanding.drain();
        match tokio::time::timeout(timeout, terminals.shutdown()).await {
            Ok(()) => {}
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis(),
                    "acp: terminal shutdown exceeded its bound"
                );
                if result.is_ok() {
                    result = Err(format!(
                        "terminal shutdown exceeded its {}ms bound",
                        timeout.as_millis()
                    ));
                }
            }
        }
        drop(request_tx);
        drop(main_tx);
        drop(lane_tx);
        drop(framing_tx);

        // Operation tasks: the registry knows every spawned turn. Aborting
        // before joining guarantees no post-shutdown write, even for a
        // backend that ignored cancellation.
        if !registry.join_turns(timeout).await && result.is_ok() {
            result = Err(format!(
                "prompt tasks did not stop within {}ms and were aborted",
                timeout.as_millis()
            ));
        }

        let mut dispatcher_handle = dispatcher_handle;
        match tokio::time::timeout(timeout, &mut dispatcher_handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                if result.is_ok() {
                    result = Err(e);
                }
            }
            Ok(Err(join)) => {
                if result.is_ok() {
                    result = Err(format!("dispatcher task failed: {join}"));
                }
            }
            Err(_elapsed) => {
                dispatcher_handle.abort();
                let _ = dispatcher_handle.await;
                tracing::warn!("acp: dispatcher task did not stop within shutdown timeout");
                if result.is_ok() {
                    result = Err(format!(
                        "dispatcher task was aborted after {}ms",
                        timeout.as_millis()
                    ));
                }
            }
        }
        let mut writer_handle = writer_handle;
        match tokio::time::timeout(timeout, &mut writer_handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                // A writer IO failure is the connection's real error.
                result = Err(e);
            }
            Ok(Err(join)) => {
                if result.is_ok() {
                    result = Err(format!("writer task failed: {join}"));
                }
            }
            Err(_elapsed) => {
                writer_handle.abort();
                let _ = writer_handle.await;
                tracing::warn!("acp: writer task did not stop within shutdown timeout");
                if result.is_ok() {
                    result = Err(format!(
                        "writer task was aborted after {}ms",
                        timeout.as_millis()
                    ));
                }
            }
        }
        result
    }

    /// Serve ACP over stdin/stdout. Callers must route their logging to
    /// stderr or elsewhere: stdout carries ONLY framed protocol bytes.
    pub async fn run_stdio(self) -> Result<(), String> {
        self.serve_connection(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Cancel path (runs on the reader task): fire the session's token
    /// immediately, poke sync backends through their legacy abort hook on
    /// another thread, and acknowledge id-bearing cancels through the
    /// high-priority cancel lane.
    async fn handle_cancel(
        &self,
        params: &Value,
        id: Option<RequestId>,
        registry: &Registry,
        lane_tx: &mpsc::Sender<Vec<u8>>,
    ) {
        match require_session_id(params) {
            Ok(session_id) => {
                let fired = registry.cancel(&session_id);
                tracing::debug!(session_id = %session_id, active = fired, "acp: session cancel");
                if fired && self.engine.is_sync() {
                    self.fire_sync_abort(session_id);
                }
            }
            Err(e) => {
                // Malformed cancel: notifications are dropped (nothing can
                // answer them), requests get the typed error via the lane.
                if let Some(id) = id {
                    let frame = error_frame(id, e.code, &e.message, e.data);
                    let _ = lane_tx.send(frame).await;
                } else {
                    tracing::warn!(
                        params = %params,
                        "acp: dropping malformed cancel notification"
                    );
                }
                return;
            }
        }
        // Uniform acknowledgement: session/cancel is a notification in
        // official ACP v1, so the ack carries no outcome; the turn's
        // terminal `stopReason` response is the outcome signal.
        if let Some(id) = id {
            let frame = result_frame(id, &json!({}));
            let _ = lane_tx.send(frame).await;
        }
    }

    /// Request-level cancel path (`$/cancel_request`, the official SDK's
    /// cancellation for a dropped request): fires the token of the running
    /// turn whose request id matches, with the same sync-backend abort hook
    /// as `session/cancel`. A non-matching or already-finished id is a
    /// no-op (bounded scan, never guessed); malformed params are dropped as
    /// a notification or answered with the typed `-32602` error when the
    /// extension was sent in request form.
    async fn handle_cancel_request(
        &self,
        params: &Value,
        id: Option<RequestId>,
        registry: &Registry,
        lane_tx: &mpsc::Sender<Vec<u8>>,
    ) {
        let request_id = params.get("requestId").and_then(RequestId::from_value);
        match request_id {
            Some(request_id) => {
                let fired = registry.cancel_request(&request_id);
                tracing::debug!(
                    request_id = %request_id,
                    session = ?fired,
                    "acp: request-level cancel"
                );
                if let Some(session_id) = fired {
                    if self.engine.is_sync() {
                        self.fire_sync_abort(session_id);
                    }
                }
            }
            None => {
                let message = "missing request id field \"requestId\"";
                if let Some(id) = id {
                    let frame = error_frame(id, INVALID_PARAMS, message, None);
                    let _ = lane_tx.send(frame).await;
                } else {
                    tracing::warn!(
                        params = %params,
                        "acp: dropping malformed $/cancel_request notification"
                    );
                }
                return;
            }
        }
        // Uniform acknowledgement for the request form; the turn's terminal
        // `stopReason` response is the outcome signal.
        if let Some(id) = id {
            let frame = result_frame(id, &json!({}));
            let _ = lane_tx.send(frame).await;
        }
    }

    /// Best-effort legacy abort for sync backends. Runs off-thread when the
    /// runtime supports it; never blocks the reader on a hostile abort.
    fn fire_sync_abort(&self, session_id: String) {
        let Engine::Sync(backend) = &self.engine else {
            return;
        };
        let backend = backend.clone();
        match tokio::runtime::Handle::current().runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = backend.abort(&session_id) {
                        tracing::debug!(session_id = %session_id, error = %e, "acp: abort hook");
                    }
                });
            }
            _ => {
                // Single-thread runtime (tests): run inline; the abort
                // contract says implementations return quickly.
                if let Err(e) = backend.abort(&session_id) {
                    tracing::debug!(session_id = %session_id, error = %e, "acp: abort hook");
                }
            }
        }
    }
}

fn is_cancel_method(method: &str) -> bool {
    method == AcpMethod::SessionCancel.as_str() || method == AcpMethod::SessionAbort.as_str()
}

/// Writer task: drains the cancel lane strictly before the main queue, so
/// cancel acknowledgements never wait behind a full queue of prompt
/// frames. Every queued item is one bare JSON body; the detected connection
/// framing is applied here (NDJSON line or legacy `Content-Length`
/// header). Ends when both channels are closed and drained.
async fn writer_task<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut framing_rx: tokio::sync::watch::Receiver<Option<protocol::Framing>>,
    mut main_rx: mpsc::Receiver<Vec<u8>>,
    mut lane_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    let mut main_open = true;
    let mut lane_open = true;
    while main_open || lane_open {
        // Strict lane priority: drain every queued cancel acknowledgement
        // before touching the main queue.
        if lane_open {
            loop {
                match lane_rx.try_recv() {
                    Ok(frame) => write_frame(&mut writer, &mut framing_rx, &frame).await?,
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        lane_open = false;
                        break;
                    }
                }
            }
        }
        if main_open {
            match main_rx.try_recv() {
                Ok(frame) => {
                    write_frame(&mut writer, &mut framing_rx, &frame).await?;
                    continue;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => main_open = false,
            }
        }
        if !main_open && !lane_open {
            break;
        }
        // Nothing immediately available: block; lane stays preferred.
        tokio::select! {
            biased;
            lane = lane_rx.recv(), if lane_open => {
                match lane {
                    Some(frame) => write_frame(&mut writer, &mut framing_rx, &frame).await?,
                    None => lane_open = false,
                }
            }
            main = main_rx.recv(), if main_open => {
                match main {
                    Some(frame) => write_frame(&mut writer, &mut framing_rx, &frame).await?,
                    None => main_open = false,
                }
            }
        }
    }
    writer
        .flush()
        .await
        .map_err(|e| format!("flush error: {e}"))
}

/// Wait until the reader has decided the connection framing (or the
/// connection ended before any framing could be observed).
async fn wait_for_framing(
    framing_rx: &mut tokio::sync::watch::Receiver<Option<protocol::Framing>>,
) -> Result<protocol::Framing, String> {
    loop {
        if let Some(framing) = *framing_rx.borrow_and_update() {
            return Ok(framing);
        }
        if framing_rx.changed().await.is_err() {
            // Reader gone before any framing was seen: no frame can belong
            // to this connection.
            return Err("connection ended before framing was established".to_string());
        }
    }
}

/// Frame one bare JSON body in the connection's negotiated framing and write
/// it. The writer blocks (bounded by the connection lifetime) until the
/// reader has decided the framing; every frame-producing path is reachable
/// only after at least one peer byte, so the wait is never unbounded.
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    framing_rx: &mut tokio::sync::watch::Receiver<Option<protocol::Framing>>,
    body: &[u8],
) -> Result<(), String> {
    let framing = wait_for_framing(framing_rx).await?;
    match framing {
        protocol::Framing::Ndjson => {
            writer
                .write_all(body)
                .await
                .map_err(|e| format!("write error: {e}"))?;
            writer
                .write_all(b"\n")
                .await
                .map_err(|e| format!("write error: {e}"))?;
        }
        protocol::Framing::ContentLength => {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            writer
                .write_all(header.as_bytes())
                .await
                .map_err(|e| format!("write error: {e}"))?;
            writer
                .write_all(body)
                .await
                .map_err(|e| format!("write error: {e}"))?;
        }
    }
    writer
        .flush()
        .await
        .map_err(|e| format!("flush error: {e}"))
}

/// Dispatcher task: owns the per-session state machine; routes prompts to
/// per-session operation tasks; answers everything else directly.
#[allow(clippy::too_many_arguments)]
async fn dispatcher_task(
    engine: Engine,
    registry: Registry,
    terminals: TerminalRegistry,
    main_tx: mpsc::Sender<Vec<u8>>,
    negotiation: Negotiation,
    outstanding: Outstanding,
    config: AcpConfig,
    mut request_rx: mpsc::Receiver<Incoming>,
) -> Result<(), String> {
    while let Some(Incoming::Request { id, method, params }) = request_rx.recv().await {
        dispatch_request(
            &engine,
            &registry,
            &terminals,
            &main_tx,
            &negotiation,
            &outstanding,
            config,
            id,
            &method,
            &params,
        )
        .await?;
    }
    Ok(())
}

/// Dispatch one request on the dispatcher task.
#[allow(clippy::too_many_arguments)]
async fn dispatch_request(
    engine: &Engine,
    registry: &Registry,
    terminals: &TerminalRegistry,
    main_tx: &mpsc::Sender<Vec<u8>>,
    negotiation: &Negotiation,
    outstanding: &Outstanding,
    config: AcpConfig,
    id: RequestId,
    method: &str,
    params: &Value,
) -> Result<(), String> {
    match method {
        "initialize" => {
            let frame = initialize_response(
                id,
                params,
                engine.capabilities(),
                negotiation,
                terminals.has_authority(),
            );
            send_checked(main_tx, frame).await
        }
        "agent_info" => {
            let frame = result_frame(id, &engine.agent_info());
            send_checked(main_tx, frame).await
        }
        "session/new" => {
            if let Err(e) = require_supported_mcp(params, engine.capabilities()) {
                return respond_error(main_tx, id, e).await;
            }
            match engine.create_session(params) {
                Ok(session_id) => {
                    // Only sessions created through this connection may own
                    // terminals here; the index is bounded like the session
                    // registry.
                    terminals.note_session(&session_id);
                    let frame = result_frame(id, &json!({ "sessionId": session_id }));
                    send_checked(main_tx, frame).await
                }
                Err(message) => {
                    let frame = internal_error_frame(id, message);
                    send_checked(main_tx, frame).await
                }
            }
        }
        "session/load" => dispatch_load(engine, main_tx, id, params).await,
        "session/prompt" => {
            dispatch_prompt(
                engine,
                registry,
                main_tx,
                negotiation,
                outstanding,
                config,
                id,
                params,
            )
            .await
        }
        "session/list" => {
            let sessions = engine
                .list_sessions()
                .into_iter()
                .map(|session_id| json!({ "sessionId": session_id }))
                .collect::<Vec<_>>();
            let frame = result_frame(id, &json!({ "sessions": sessions }));
            send_checked(main_tx, frame).await
        }
        "authenticate" => {
            // No real auth flow exists: `authMethods` is always empty, so
            // every authenticate call is an official typed refusal.
            let frame = authenticate_response(id, params);
            send_checked(main_tx, frame).await
        }
        "terminal/create" | "terminal/input" | "terminal/resize" | "terminal/kill"
        | "terminal/close" | "terminal/list" => {
            dispatch_terminal(terminals, main_tx, negotiation, id, method, params).await
        }
        other => {
            let frame = error_frame(id, METHOD_NOT_FOUND, MSG_METHOD_NOT_FOUND, None);
            tracing::debug!(method = %other, "acp: unknown method");
            send_checked(main_tx, frame).await
        }
    }
}

/// `session/load`: capability-gated, bounded native history replay. The
/// backend owns session ownership; a foreign session is an official
/// invalid-params error, never an empty replay.
async fn dispatch_load(
    engine: &Engine,
    main_tx: &mpsc::Sender<Vec<u8>>,
    id: RequestId,
    params: &Value,
) -> Result<(), String> {
    if !engine.capabilities().load_session {
        let frame = error_frame(id, METHOD_NOT_FOUND, MSG_METHOD_NOT_FOUND, None);
        return send_checked(main_tx, frame).await;
    }
    let session_id = match require_session_id(params) {
        Ok(session_id) => session_id,
        Err(e) => return respond_error(main_tx, id, e).await,
    };
    if let Err(e) = require_supported_mcp(params, engine.capabilities()) {
        return respond_error(main_tx, id, e).await;
    }
    match engine.load_session(&session_id) {
        Ok(page) => {
            if page.session_id != session_id {
                // A backend that answers for a session it was not asked
                // about is refused loudly; never replay foreign history.
                return respond_error(
                    main_tx,
                    id,
                    ServerError::internal(format!(
                        "load_session answered for session {:?}, not the requested {:?}",
                        page.session_id, session_id
                    )),
                )
                .await;
            }
            let updates = match history_updates(&page) {
                Ok(updates) => updates,
                Err(e) => return respond_error(main_tx, id, e).await,
            };
            for update in updates {
                let update_params = session_update_params(&session_id, update);
                let frame = notification_body_bytes("session/update", &update_params);
                if send_checked(main_tx, frame).await.is_err() {
                    return Err("writer queue closed".to_string());
                }
            }
            let frame = result_frame(id, &json!({}));
            send_checked(main_tx, frame).await
        }
        Err(LoadSessionError::NotFound) => {
            respond_error(
                main_tx,
                id,
                ServerError::invalid_params(format!("unknown session {session_id:?}")),
            )
            .await
        }
        Err(LoadSessionError::Unsupported) => {
            let frame = error_frame(id, METHOD_NOT_FOUND, MSG_METHOD_NOT_FOUND, None);
            send_checked(main_tx, frame).await
        }
        Err(LoadSessionError::Unavailable(message)) => {
            let frame = internal_error_frame(id, message);
            send_checked(main_tx, frame).await
        }
    }
}

/// Dispatcher entry for the six `faktor.terminal` methods. The gate is the
/// negotiated extension AND an attached authority: clients that did not
/// declare `faktor.terminal` (or a server without a terminal authority) keep
/// the official `-32601`, exactly like any unknown method.
async fn dispatch_terminal(
    terminals: &TerminalRegistry,
    main_tx: &mpsc::Sender<Vec<u8>>,
    negotiation: &Negotiation,
    id: RequestId,
    method: &str,
    params: &Value,
) -> Result<(), String> {
    if !terminals.has_authority() || !negotiation.has_extension(EXTENSION_TERMINAL) {
        let frame = error_frame(id, METHOD_NOT_FOUND, MSG_METHOD_NOT_FOUND, None);
        return send_checked(main_tx, frame).await;
    }
    let outcome = match method {
        "terminal/create" => terminal_create(terminals, main_tx, negotiation, params).await,
        "terminal/input" => terminal_input(terminals, params).await,
        "terminal/resize" => terminal_resize(terminals, params).await,
        "terminal/kill" => terminal_kill(terminals, params).await,
        "terminal/close" => terminal_close(terminals, params).await,
        "terminal/list" => terminal_list(terminals, params),
        _ => Err(ServerError::internal("unrouted terminal method")),
    };
    match outcome {
        Ok(value) => {
            let frame = result_frame(id, &value);
            send_checked(main_tx, frame).await
        }
        Err(e) => respond_error(main_tx, id, e).await,
    }
}

/// Strict param object: every unknown member is a typed refusal (a typo can
/// never silently degrade into a default).
fn strict_terminal_params<'a>(
    params: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, ServerError> {
    let object = params
        .as_object()
        .ok_or_else(|| ServerError::invalid_params("params must be a JSON object"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ServerError::invalid_params(format!(
                "unknown terminal param {key:?}"
            )));
        }
    }
    Ok(object)
}

fn required_terminal_string(
    object: &Map<String, Value>,
    field: &str,
    max: usize,
) -> Result<String, ServerError> {
    match object.get(field) {
        Some(Value::String(text)) if text.is_empty() => Err(ServerError::invalid_params(format!(
            "terminal param \"{field}\" must be non-empty"
        ))),
        Some(Value::String(text)) if text.len() > max => Err(ServerError::invalid_params(format!(
            "terminal param \"{field}\" exceeds {max} bytes"
        ))),
        Some(Value::String(text)) if text.contains('\0') => Err(ServerError::invalid_params(
            format!("terminal param \"{field}\" contains a NUL byte"),
        )),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(ServerError::invalid_params(format!(
            "terminal param \"{field}\" must be a string"
        ))),
        None => Err(ServerError::invalid_params(format!(
            "missing terminal param \"{field}\""
        ))),
    }
}

fn optional_terminal_string(
    object: &Map<String, Value>,
    field: &str,
    max: usize,
) -> Result<Option<String>, ServerError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            if text.is_empty() {
                return Err(ServerError::invalid_params(format!(
                    "terminal param \"{field}\" must be non-empty"
                )));
            }
            if text.len() > max {
                return Err(ServerError::invalid_params(format!(
                    "terminal param \"{field}\" exceeds {max} bytes"
                )));
            }
            if text.contains('\0') {
                return Err(ServerError::invalid_params(format!(
                    "terminal param \"{field}\" contains a NUL byte"
                )));
            }
            Ok(Some(text.clone()))
        }
        Some(_) => Err(ServerError::invalid_params(format!(
            "terminal param \"{field}\" must be a string"
        ))),
    }
}

fn terminal_args(object: &Map<String, Value>) -> Result<Vec<String>, ServerError> {
    match object.get("args") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            if items.len() > MAX_TERMINAL_ARGS {
                return Err(ServerError::invalid_params(format!(
                    "terminal args exceed the {MAX_TERMINAL_ARGS}-entry bound"
                )));
            }
            let mut args = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text)
                        if text.len() <= MAX_TERMINAL_ARG_BYTES && !text.contains('\0') =>
                    {
                        args.push(text.clone());
                    }
                    Value::String(text) if text.len() > MAX_TERMINAL_ARG_BYTES => {
                        return Err(ServerError::invalid_params(format!(
                            "terminal arg exceeds {MAX_TERMINAL_ARG_BYTES} bytes"
                        )));
                    }
                    Value::String(_) => {
                        return Err(ServerError::invalid_params(
                            "terminal arg contains a NUL byte",
                        ));
                    }
                    _ => {
                        return Err(ServerError::invalid_params("terminal args must be strings"));
                    }
                }
            }
            Ok(args)
        }
        Some(_) => Err(ServerError::invalid_params(
            "terminal param \"args\" must be an array of strings",
        )),
    }
}

/// `env` is an allowlist of NAMES (never `KEY=VALUE` pairs): the attached
/// authority resolves the names against the daemon environment and applies
/// its own deny-set, so secrets can never cross the ACP wire from either
/// direction.
fn terminal_env(object: &Map<String, Value>) -> Result<Vec<String>, ServerError> {
    match object.get("env") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            if items.len() > MAX_TERMINAL_ENV_NAMES {
                return Err(ServerError::invalid_params(format!(
                    "terminal env allowlist exceeds the {MAX_TERMINAL_ENV_NAMES}-name bound"
                )));
            }
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(ServerError::invalid_params(
                        "terminal env entries must be strings",
                    ));
                };
                if name.len() > MAX_TERMINAL_ENV_NAME_BYTES {
                    return Err(ServerError::invalid_params(format!(
                        "terminal env name exceeds {MAX_TERMINAL_ENV_NAME_BYTES} bytes"
                    )));
                }
                let mut bytes = name.bytes().enumerate();
                let valid = match bytes.next() {
                    Some((_, b)) if b.is_ascii_alphabetic() || b == b'_' => {
                        bytes.all(|(_, b)| b.is_ascii_alphanumeric() || b == b'_')
                    }
                    _ => false,
                };
                if !valid {
                    return Err(ServerError::invalid_params(format!(
                        "invalid terminal env name {name:?} (expected [A-Za-z_][A-Za-z0-9_]*)"
                    )));
                }
                names.push(name.to_string());
            }
            Ok(names)
        }
        Some(_) => Err(ServerError::invalid_params(
            "terminal param \"env\" must be an array of names",
        )),
    }
}

fn terminal_size_field(
    object: &Map<String, Value>,
    field: &str,
    default: Option<u16>,
) -> Result<u16, ServerError> {
    match object.get(field) {
        None | Some(Value::Null) => default.ok_or_else(|| {
            ServerError::invalid_params(format!("missing terminal param \"{field}\""))
        }),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) if (1..=65535).contains(&value) => Ok(value as u16),
            _ => Err(ServerError::invalid_params(format!(
                "terminal param \"{field}\" must be an integer in 1..=65535"
            ))),
        },
        Some(_) => Err(ServerError::invalid_params(format!(
            "terminal param \"{field}\" must be an integer"
        ))),
    }
}

/// A session is terminal-writable only when it was created through THIS
/// connection; anything else is the same typed denial, so foreign sessions
/// and nonexistent ones are indistinguishable (no existence leak).
fn require_terminal_session(
    terminals: &TerminalRegistry,
    object: &Map<String, Value>,
) -> Result<String, ServerError> {
    let session_id = required_terminal_string(object, "sessionId", MAX_TERMINAL_ID_BYTES)?;
    if !terminals.knows_session(&session_id) {
        return Err(unknown_session(&session_id));
    }
    Ok(session_id)
}

fn terminal_error_to_server(error: TerminalError) -> ServerError {
    match error {
        TerminalError::Invalid(message) => ServerError::invalid_params(message),
        other => ServerError::internal(other.to_string()),
    }
}

/// Run one bounded blocking authority operation off the dispatcher task.
async fn run_terminal_op<F>(timeout: Duration, op: F) -> Result<(), ServerError>
where
    F: FnOnce() -> Result<(), TerminalError> + Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(op)).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(terminal_error_to_server(error)),
        Ok(Err(_join)) => Err(ServerError::internal("terminal io task failed")),
        Err(_elapsed) => Err(ServerError::internal("terminal io timed out")),
    }
}

async fn terminal_create(
    terminals: &TerminalRegistry,
    main_tx: &mpsc::Sender<Vec<u8>>,
    negotiation: &Negotiation,
    params: &Value,
) -> Result<Value, ServerError> {
    let object = strict_terminal_params(
        params,
        &["sessionId", "command", "args", "cwd", "env", "rows", "cols"],
    )?;
    let session_id = require_terminal_session(terminals, object)?;
    let command = required_terminal_string(object, "command", MAX_TERMINAL_COMMAND_BYTES)?;
    let args = terminal_args(object)?;
    let cwd = optional_terminal_string(object, "cwd", MAX_TERMINAL_CWD_BYTES)?;
    let env = terminal_env(object)?;
    let rows = terminal_size_field(object, "rows", Some(24))?;
    let cols = terminal_size_field(object, "cols", Some(80))?;
    let spec = TerminalSpec {
        command,
        args,
        cwd,
        env,
        rows,
        cols,
    };
    let authority = terminals
        .authority
        .clone()
        .ok_or_else(|| ServerError::internal("no terminal authority attached"))?;
    let session_for_authority = session_id.clone();
    let created = tokio::time::timeout(
        terminals.config.terminal_spawn_timeout,
        tokio::task::spawn_blocking(move || authority.create(&session_for_authority, &spec)),
    )
    .await;
    match created {
        Ok(Ok(Ok(handle))) => {
            terminals.register(&session_id, handle, main_tx.clone(), negotiation.clone())
        }
        Ok(Ok(Err(error))) => Err(terminal_error_to_server(error)),
        Ok(Err(_join)) => Err(ServerError::internal("terminal spawn task failed")),
        // The blocking spawn was abandoned at the bound; if it ever
        // completes, the drawn handle is dropped, which kills the child.
        Err(_elapsed) => Err(ServerError::internal("terminal spawn timed out")),
    }
}

async fn terminal_input(
    terminals: &TerminalRegistry,
    params: &Value,
) -> Result<Value, ServerError> {
    let object = strict_terminal_params(params, &["sessionId", "terminalId", "data"])?;
    let session_id = require_terminal_session(terminals, object)?;
    let terminal_id = required_terminal_string(object, "terminalId", MAX_TERMINAL_ID_BYTES)?;
    let data = match object.get("data") {
        Some(Value::String(text)) => text.clone(),
        Some(_) => {
            return Err(ServerError::invalid_params(
                "terminal param \"data\" must be a string",
            ))
        }
        None => {
            return Err(ServerError::invalid_params(
                "missing terminal param \"data\"",
            ))
        }
    };
    if data.len() > MAX_TERMINAL_INPUT_BYTES {
        return Err(ServerError::invalid_params(format!(
            "terminal input of {} bytes exceeds the {MAX_TERMINAL_INPUT_BYTES}-byte bound",
            data.len()
        )));
    }
    if data.contains('\0') {
        return Err(ServerError::invalid_params(
            "terminal input contains a NUL byte",
        ));
    }
    let handle = terminals.handle_for(&session_id, &terminal_id)?;
    let bytes = data.into_bytes();
    run_terminal_op(terminals.config.terminal_io_timeout, move || {
        handle.write(&bytes)
    })
    .await?;
    Ok(json!({}))
}

async fn terminal_resize(
    terminals: &TerminalRegistry,
    params: &Value,
) -> Result<Value, ServerError> {
    let object = strict_terminal_params(params, &["sessionId", "terminalId", "rows", "cols"])?;
    let session_id = require_terminal_session(terminals, object)?;
    let terminal_id = required_terminal_string(object, "terminalId", MAX_TERMINAL_ID_BYTES)?;
    let rows = terminal_size_field(object, "rows", None)?;
    let cols = terminal_size_field(object, "cols", None)?;
    let handle = terminals.handle_for(&session_id, &terminal_id)?;
    run_terminal_op(terminals.config.terminal_io_timeout, move || {
        handle.resize(rows, cols)
    })
    .await?;
    Ok(json!({}))
}

async fn terminal_kill(terminals: &TerminalRegistry, params: &Value) -> Result<Value, ServerError> {
    let object = strict_terminal_params(params, &["sessionId", "terminalId"])?;
    let session_id = require_terminal_session(terminals, object)?;
    let terminal_id = required_terminal_string(object, "terminalId", MAX_TERMINAL_ID_BYTES)?;
    let handle = terminals.handle_for(&session_id, &terminal_id)?;
    run_terminal_op(terminals.config.terminal_io_timeout, move || handle.kill()).await?;
    Ok(json!({}))
}

async fn terminal_close(
    terminals: &TerminalRegistry,
    params: &Value,
) -> Result<Value, ServerError> {
    let object = strict_terminal_params(params, &["sessionId", "terminalId"])?;
    let session_id = require_terminal_session(terminals, object)?;
    let terminal_id = required_terminal_string(object, "terminalId", MAX_TERMINAL_ID_BYTES)?;
    // The row is removed (pump cancelled) before the kill; the row can never
    // be reached again even if the best-effort kill fails.
    let (handle, _stats) = terminals.take(&session_id, &terminal_id)?;
    run_terminal_op(terminals.config.terminal_io_timeout, move || handle.kill()).await?;
    Ok(json!({}))
}

fn terminal_list(terminals: &TerminalRegistry, params: &Value) -> Result<Value, ServerError> {
    let object = strict_terminal_params(params, &["sessionId"])?;
    let session_id = require_terminal_session(terminals, object)?;
    // When the attached authority exposes its durable rows, the listing IS
    // the authority's projection (including Lost rows a restart recovered);
    // otherwise the per-connection table remains the create-only surface.
    if let Some(authority) = &terminals.authority {
        if authority.exposes_durable_terminals() {
            let rows = authority
                .list_terminals(&session_id)
                .map_err(terminal_error_to_server)?;
            let rows: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "terminalId": row.terminal_id,
                        "sessionId": row.session_id,
                        "taskId": row.task_id,
                        "agentId": row.agent_id,
                        "operationId": row.operation_id,
                        "pid": row.pid,
                        "startTimeMs": row.start_time_ms,
                        "state": row.state,
                        "alive": row.alive,
                    })
                })
                .collect();
            return Ok(json!({ "sessionId": session_id, "terminals": rows }));
        }
    }
    let rows = terminals.list(&session_id)?;
    Ok(json!({ "sessionId": session_id, "terminals": rows }))
}

/// Map the frozen native message page (newest-first) into chronological
/// official replay frames. The native page carries one bounded window: a
/// `has_more` page would replay only a suffix, so it is refused instead of
/// pretending the conversation is complete.
fn history_updates(page: &MessagesPage) -> Result<Vec<Value>, ServerError> {
    if page.has_more {
        return Err(ServerError::internal(
            "session history exceeds the bounded load window (older messages exist)".to_string(),
        ));
    }
    if page.messages.len() > MAX_LOAD_MESSAGES {
        return Err(ServerError::internal(format!(
            "session history of {} messages exceeds the {MAX_LOAD_MESSAGES}-message load bound",
            page.messages.len()
        )));
    }
    let mut updates = Vec::new();
    // Native pages are newest-first; ACP replays chronologically.
    for message in page.messages.iter().rev() {
        for part in &message.parts {
            let update = match part {
                NativePart::Text { text } => match message.role.as_str() {
                    "user" => user_message_chunk_update(text),
                    "assistant" => text_chunk_update(text),
                    // System scaffolding has no official chunk kind: it is
                    // replayed as an agent thought (documented degradation).
                    _ => agent_thought_chunk_update(text),
                },
                NativePart::Reasoning { text } | NativePart::Summary { text } => {
                    agent_thought_chunk_update(text)
                }
                NativePart::ToolCall {
                    tool_call_id,
                    name,
                    input,
                    state,
                } => tool_call_from_native(tool_call_id, name, input, state),
                NativePart::ToolResult {
                    tool_call_id,
                    result,
                } => tool_result_from_native(
                    tool_call_id,
                    &result.excerpt,
                    result.exit_code,
                    result.artifact.as_deref(),
                    result.slice_hint.as_deref(),
                ),
            };
            updates.push(update);
            if updates.len() > MAX_LOAD_FRAMES {
                return Err(ServerError::internal(format!(
                    "session history exceeds the {MAX_LOAD_FRAMES}-frame load bound"
                )));
            }
        }
    }
    Ok(updates)
}

/// MCP servers have no honest implementation in the default backend: a
/// non-empty `mcpServers` list is refused with `-32602` unless the backend
/// declares the MCP capability. Never silently dropped.
fn require_supported_mcp(
    params: &Value,
    capabilities: BackendCapabilities,
) -> Result<(), ServerError> {
    match params.get("mcpServers") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(servers)) => {
            if servers.is_empty() || capabilities.mcp_http || capabilities.mcp_sse {
                Ok(())
            } else {
                Err(ServerError::invalid_params(MSG_LOAD_MCP_UNSUPPORTED))
            }
        }
        Some(_) => Err(ServerError::invalid_params(
            "\"mcpServers\" must be an array",
        )),
    }
}

/// `authenticate` with an empty `authMethods` list: a real auth flow does
/// not exist, so the request is refused with the official invalid-params
/// error carrying the method id (never silent, never faked).
fn authenticate_response(id: RequestId, params: &Value) -> Vec<u8> {
    match params.get("methodId").and_then(Value::as_str) {
        Some(method_id) if !method_id.is_empty() => error_frame(
            id,
            INVALID_PARAMS,
            MSG_AUTH_UNAVAILABLE,
            Some(json!({ "methodId": method_id })),
        ),
        _ => error_frame(
            id,
            INVALID_PARAMS,
            "missing string field \"methodId\"",
            None,
        ),
    }
}

/// Per-session bookkeeping: one active turn (plus one queued prompt) per
/// session; idle entries are evicted first once `max_sessions` is hit.
/// `turns` tracks every spawned operation task so the connection wind-down
/// can abort/join them (no task outlives `serve_connection`).
#[derive(Clone)]
struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
    turns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

struct RegistryInner {
    sessions: HashMap<String, SessionState>,
    order: VecDeque<String>,
    max_sessions: usize,
    active_turns: usize,
    /// Set by `cancel_all` (connection wind-down): no NEW turn may be
    /// admitted and a finished turn never promotes its queued prompt.
    closed: bool,
}

impl Registry {
    fn new(max_sessions: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                sessions: HashMap::new(),
                order: VecDeque::new(),
                max_sessions,
                active_turns: 0,
                closed: false,
            })),
            turns: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Track one spawned turn task; finished handles are pruned first so the
    /// list stays bounded by the genuinely live tasks.
    fn track_turn(&self, handle: tokio::task::JoinHandle<()>) {
        let mut turns = self
            .turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        turns.retain(|h| !h.is_finished());
        turns.push(handle);
    }

    /// Abort and join every tracked turn task with an overall bound. Returns
    /// false when at least one task was still running at the deadline (it was
    /// aborted first, so it can never write again).
    async fn join_turns(&self, bound: Duration) -> bool {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut turns = self
                .turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            turns.drain(..).collect()
        };
        let deadline = tokio::time::Instant::now() + bound;
        let mut all_stopped = true;
        for handle in handles {
            handle.abort();
            if tokio::time::timeout_at(deadline, handle).await.is_err() {
                all_stopped = false;
            }
        }
        all_stopped
    }

    fn cancel(&self, session_id: &str) -> bool {
        let mut inner = self.lock();
        match inner
            .sessions
            .get_mut(session_id)
            .and_then(|s| s.active.as_mut())
        {
            Some(active) => {
                active.token.cancel();
                true
            }
            None => false,
        }
    }

    /// Wind-down: close the registry (no new turns, no queued-prompt
    /// promotion) and fire every active token. Tracked tasks are then
    /// aborted/joined by [`Registry::join_turns`].
    fn cancel_all(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        for state in inner.sessions.values() {
            if let Some(active) = &state.active {
                active.token.cancel();
            }
        }
    }

    /// Admit a prompt turn for a session.
    /// `Start(job, token)`: the caller must run the turn on a fresh
    /// operation task; `Queued`: runs after the active turn's terminal
    /// response; `Busy`: queue depth already full; `Full`: the server's
    /// per-session capacity is exhausted (no idle entries left to evict);
    /// `Closed`: the connection is winding down.
    fn admit(&self, session_id: &str, job: PromptJob) -> Admit {
        let mut inner = self.lock();
        if inner.closed {
            return Admit::Closed;
        }
        // Make room for a new entry first (never evicts active turns).
        if !inner.sessions.contains_key(session_id) {
            inner.order.push_back(session_id.to_string());
            let target = inner.max_sessions.saturating_sub(1);
            self.evict_idle_to(&mut inner, target);
        }
        let max_sessions = inner.max_sessions;
        let active_turns = inner.active_turns;
        let state = inner.sessions.entry(session_id.to_string()).or_default();
        if let Some(active) = &mut state.active {
            if active.queued.is_none() {
                active.queued = Some(job);
                return Admit::Queued;
            }
            return Admit::Busy;
        }
        if active_turns >= max_sessions && max_sessions > 0 {
            return Admit::Full;
        }
        let request_id = job.id.clone();
        let token = CancelToken::new();
        state.active = Some(ActiveTurn {
            request_id,
            token: token.clone(),
            queued: None,
        });
        inner.active_turns += 1;
        Admit::Start(job, token)
    }

    /// Cancel the running turn whose request id matches (the
    /// `$/cancel_request` extension). Returns the owning session so a sync
    /// backend's legacy `abort` hook can fire, or `None` when no running
    /// turn matches (queued prompts and unknown ids are left untouched,
    /// matching `session/cancel` semantics). The scan is bounded by
    /// `max_sessions`.
    fn cancel_request(&self, request_id: &RequestId) -> Option<String> {
        let mut inner = self.lock();
        for (session_id, state) in inner.sessions.iter_mut() {
            let Some(active) = state.active.as_mut() else {
                continue;
            };
            if &active.request_id == request_id {
                active.token.cancel();
                return Some(session_id.clone());
            }
        }
        None
    }

    /// Evict idle entries until at most `target` entries remain. Active
    /// entries are skipped (bounded scan); nothing is ever evicted while
    /// its turn is running.
    fn evict_idle_to(&self, inner: &mut RegistryInner, target: usize) {
        let mut scanned = 0usize;
        while inner.sessions.len() > target && scanned < inner.sessions.len() {
            scanned += 1;
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            let Some(state) = inner.sessions.get(&oldest) else {
                continue;
            };
            if state.active.is_none() {
                inner.sessions.remove(&oldest);
            } else {
                // Active entries are not evictable; revisit later.
                inner.order.push_back(oldest);
            }
        }
    }

    /// Terminal handoff after the operation task enqueued the turn's
    /// terminal response: promote the queued prompt (fresh token) or park
    /// the session as idle. Returns the job to run next, if any. A closed
    /// registry never promotes: the wind-down drops every queued prompt.
    fn finish(&self, session_id: &str, token: &CancelToken) -> Option<(PromptJob, CancelToken)> {
        let mut inner = self.lock();
        if inner.closed {
            if let Some(state) = inner.sessions.get_mut(session_id) {
                state.active = None;
            }
            inner.active_turns = inner.active_turns.saturating_sub(1);
            return None;
        }
        let state = inner.sessions.get_mut(session_id)?;
        let active = state.active.as_mut()?;
        if active.token != *token {
            // A newer turn already owns the session (cannot normally
            // happen: promotions happen under the same lock).
            return None;
        }
        match active.queued.take() {
            Some(job) => {
                let next_token = CancelToken::new();
                active.token = next_token.clone();
                Some((job, next_token))
            }
            None => {
                state.active = None;
                inner.active_turns = inner.active_turns.saturating_sub(1);
                None
            }
        }
    }
}

enum Admit {
    Start(PromptJob, CancelToken),
    Queued,
    Busy,
    /// Per-session capacity exhausted (bounded resource refusal).
    Full,
    /// The connection is winding down: no new work is admitted.
    Closed,
}

fn require_session_id(params: &Value) -> Result<String, ServerError> {
    let id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| params.get("sessionID").and_then(Value::as_str)); // deprecated alias
    match id {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(ServerError::invalid_params(
            "missing string field \"sessionId\"",
        )),
    }
}

/// Extract the plain-text content of an official prompt message (array of
/// `{"type":"text","text":...}` content blocks). The deprecated `text`
/// string alias is accepted. Anything the seam cannot honestly represent
/// is refused with `-32602`.
fn require_prompt_text(params: &Value) -> Result<String, ServerError> {
    if let Some(prompt) = params.get("prompt") {
        let blocks = prompt.as_array().ok_or_else(|| {
            ServerError::invalid_params("\"prompt\" must be an array of content blocks")
        })?;
        if blocks.is_empty() {
            return Err(ServerError::invalid_params(
                "\"prompt\" must contain at least one content block",
            ));
        }
        let mut parts = Vec::new();
        for block in blocks {
            let kind = block.get("type").and_then(Value::as_str);
            match (kind, block.get("text").and_then(Value::as_str)) {
                (Some("text"), Some(text)) => parts.push(text.to_string()),
                (Some(other), _) => {
                    return Err(ServerError::invalid_params(format!(
                        "unsupported content block type \"{other}\": this agent accepts text blocks only"
                    )))
                }
                _ => {
                    return Err(ServerError::invalid_params(
                        "content block must be {\"type\":\"text\",\"text\":\"...\"}",
                    ))
                }
            }
        }
        Ok(parts.join("\n"))
    } else if let Some(text) = params.get("text").and_then(Value::as_str) {
        // Deprecated pre-conformance alias.
        Ok(text.to_string())
    } else {
        Err(ServerError::invalid_params(
            "missing \"prompt\" (array of text content blocks)",
        ))
    }
}

/// Admit a `session/prompt` into the per-session state machine. Immediate
/// parameter errors answer right away; a Start/Queued admission answers
/// asynchronously with the turn's terminal `stopReason` response.
#[allow(clippy::too_many_arguments)]
async fn dispatch_prompt(
    engine: &Engine,
    registry: &Registry,
    main_tx: &mpsc::Sender<Vec<u8>>,
    negotiation: &Negotiation,
    outstanding: &Outstanding,
    config: AcpConfig,
    id: RequestId,
    params: &Value,
) -> Result<(), String> {
    let session_id = match require_session_id(params) {
        Ok(s) => s,
        Err(e) => return respond_error(main_tx, id, e).await,
    };
    let text = match require_prompt_text(params) {
        Ok(t) => t,
        Err(e) => return respond_error(main_tx, id, e).await,
    };
    let job = PromptJob {
        id: id.clone(),
        text,
    };
    match registry.admit(&session_id, job) {
        Admit::Start(job, token) => {
            spawn_turn(
                engine.clone(),
                registry.clone(),
                main_tx.clone(),
                negotiation.clone(),
                outstanding.clone(),
                config,
                session_id,
                job,
                token,
            );
            Ok(())
        }
        Admit::Queued => Ok(()),
        Admit::Busy => {
            let frame = error_frame(id, SESSION_BUSY, MSG_SESSION_BUSY, None);
            send_checked(main_tx, frame).await
        }
        Admit::Full => {
            let frame = error_frame(id, SESSION_LIMIT, MSG_SESSION_LIMIT, None);
            send_checked(main_tx, frame).await
        }
        Admit::Closed => {
            let frame = internal_error_frame(id, "the connection is shutting down".to_string());
            send_checked(main_tx, frame).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_turn(
    engine: Engine,
    registry: Registry,
    main_tx: mpsc::Sender<Vec<u8>>,
    negotiation: Negotiation,
    outstanding: Outstanding,
    config: AcpConfig,
    session_id: String,
    job: PromptJob,
    token: CancelToken,
) {
    let main_tx_2 = main_tx.clone();
    let tracked = registry.clone();
    let handle = tokio::spawn(async move {
        // The only per-session work in flight: run the turn, enqueue its
        // terminal response, then promote the queued prompt (if any).
        // A panicking backend must not leave the prompt unanswered.
        let run = engine.run_turn(
            &session_id,
            &job,
            main_tx,
            token.clone(),
            negotiation.clone(),
            outstanding.clone(),
            config,
        );
        let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(run))
            .await
            .unwrap_or_else(|_panic| {
                tracing::error!(session_id = %session_id, "acp: backend panicked during prompt");
                TurnOutcome::Failed("backend panicked during prompt".to_string())
            });
        let frame = terminal_frame(job.id, outcome);
        if send_checked(&main_tx_2, frame).await.is_err() {
            // Writer is gone; the connection is ending.
            return;
        }
        if let Some((next_job, next_token)) = registry.finish(&session_id, &token) {
            spawn_turn(
                engine,
                registry,
                main_tx_2,
                negotiation,
                outstanding,
                config,
                session_id,
                next_job,
                next_token,
            );
        }
    });
    // Tracked so the connection wind-down can abort/join every operation
    // task: no task outlives `serve_connection`.
    tracked.track_turn(handle);
}

/// Terminal frame for one prompt turn: the official `stopReason` result,
/// or the official internal-error frame on backend failure.
fn terminal_frame(id: RequestId, outcome: TurnOutcome) -> Vec<u8> {
    match outcome {
        TurnOutcome::Completed(value) => {
            let mut result = Map::new();
            result.insert("stopReason".into(), json!("end_turn"));
            if !value.is_null() {
                // Official `_meta` extension member: the backend seam's
                // opaque report has no other official slot.
                result.insert("_meta".into(), value);
            }
            result_frame(id, &Value::Object(result))
        }
        TurnOutcome::Cancelled => {
            let mut result = Map::new();
            result.insert("stopReason".into(), json!("cancelled"));
            result_frame(id, &Value::Object(result))
        }
        TurnOutcome::Failed(message) => internal_error_frame(id, message),
    }
}

/// The official `initialize` response, or a typed error for anything that
/// is not protocol version 1 / well-formed negotiation (no silent
/// fallback, no silent acceptance). The negotiated state is replaced only
/// after the whole request validates.
fn initialize_response(
    id: RequestId,
    params: &Value,
    capabilities: BackendCapabilities,
    negotiation: &Negotiation,
    terminal_available: bool,
) -> Vec<u8> {
    let version = params.get("protocolVersion");
    let version_ok = match version {
        Some(Value::Number(n)) => n.as_u64() == Some(PROTOCOL_VERSION),
        Some(Value::String(s)) => s.as_str() == PROTOCOL_VERSION.to_string(),
        _ => false,
    };
    if !version_ok {
        let raw = version.cloned().unwrap_or(Value::Null);
        let mut data = Map::new();
        data.insert("protocolVersion".into(), raw);
        data.insert("supportedProtocolVersion".into(), json!(PROTOCOL_VERSION));
        let message = format!(
            "unsupported protocol version; this agent supports protocol version {PROTOCOL_VERSION}"
        );
        return error_frame(id, INVALID_PARAMS, &message, Some(Value::Object(data)));
    }
    let mut negotiated = match parse_initialize(params) {
        Ok(negotiated) => negotiated,
        Err(e) => return error_frame(id, e.code, &e.message, e.data),
    };
    // `faktor.terminal` is granted only when the server carries a terminal
    // authority; declaring it without one is not negotiated (and every
    // terminal method keeps the official -32601).
    if !terminal_available {
        negotiated
            .extensions
            .retain(|name| name != EXTENSION_TERMINAL);
    }
    negotiation.replace(negotiated.clone());
    let mut agent_capabilities = Map::new();
    agent_capabilities.insert(
        "promptCapabilities".into(),
        json!({
            "audio": false,
            "embeddedContext": false,
            "image": false,
        }),
    );
    agent_capabilities.insert("loadSession".into(), json!(capabilities.load_session));
    if capabilities.mcp_http || capabilities.mcp_sse {
        agent_capabilities.insert(
            "mcpCapabilities".into(),
            json!({ "http": capabilities.mcp_http, "sse": capabilities.mcp_sse }),
        );
    }
    let mut result = Map::new();
    result.insert("protocolVersion".into(), json!(PROTOCOL_VERSION));
    result.insert(
        "agentCapabilities".into(),
        Value::Object(agent_capabilities),
    );
    result.insert("authMethods".into(), json!([]));
    if !negotiated.extensions.is_empty() {
        // Echo the accepted subset only; unknown names are not accepted.
        result.insert("extensions".into(), json!(negotiated.extensions));
    }
    result_frame(id, &Value::Object(result))
}

/// Parse the tolerated client negotiation members of `initialize`.
/// Malformed declarations are loud (`-32602`); unknown extension names are
/// silently not accepted (never echoed).
fn parse_initialize(params: &Value) -> Result<Negotiated, ServerError> {
    let mut negotiated = Negotiated::default();
    match params.get("extensions") {
        None | Some(Value::Null) => {}
        Some(Value::Array(extensions)) => {
            for extension in extensions {
                let name = extension.as_str().ok_or_else(|| {
                    ServerError::invalid_params("\"extensions\" entries must be strings")
                })?;
                if ACCEPTED_EXTENSIONS.contains(&name)
                    && !negotiated.extensions.iter().any(|e| e == name)
                {
                    negotiated.extensions.push(name.to_string());
                }
            }
        }
        Some(_) => {
            return Err(ServerError::invalid_params(
                "\"extensions\" must be an array of strings",
            ))
        }
    }
    match params.get("clientCapabilities") {
        None | Some(Value::Null) => {}
        Some(Value::Object(client)) => {
            if let Some(fs) = client.get("fs") {
                let fs = fs.as_object().ok_or_else(|| {
                    ServerError::invalid_params("\"clientCapabilities.fs\" must be an object")
                })?;
                negotiated.fs_read_text_file = bool_field(fs, "readTextFile")?;
                negotiated.fs_write_text_file = bool_field(fs, "writeTextFile")?;
            }
            if let Some(terminal) = client.get("terminal") {
                if !terminal.is_null() && !terminal.is_boolean() {
                    return Err(ServerError::invalid_params(
                        "\"clientCapabilities.terminal\" must be a boolean",
                    ));
                }
            }
        }
        Some(_) => {
            return Err(ServerError::invalid_params(
                "\"clientCapabilities\" must be an object",
            ))
        }
    }
    Ok(negotiated)
}

fn bool_field(object: &Map<String, Value>, field: &str) -> Result<bool, ServerError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ServerError::invalid_params(format!(
            "\"clientCapabilities.fs.{field}\" must be a boolean"
        ))),
    }
}

#[derive(Debug)]
struct ServerError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl ServerError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
            data: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: MSG_INTERNAL_ERROR.to_string(),
            data: Some(json!(message.into())),
        }
    }
}

async fn respond_error(
    main_tx: &mpsc::Sender<Vec<u8>>,
    id: RequestId,
    error: ServerError,
) -> Result<(), String> {
    let frame = error_frame(id, error.code, &error.message, error.data);
    send_checked(main_tx, frame).await
}

/// Official error object: `{code, message}` with `data` omitted when absent.
fn error_object(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut object = Map::new();
    object.insert("code".into(), json!(code));
    object.insert("message".into(), json!(message));
    if let Some(data) = data {
        object.insert("data".into(), data);
    }
    Value::Object(object)
}

fn error_frame_value(id: &Value, code: i64, message: &str, data: Option<Value>) -> Vec<u8> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error_object(code, message, data),
    });
    body_or_internal(&body)
}

fn error_frame(id: RequestId, code: i64, message: &str, data: Option<Value>) -> Vec<u8> {
    error_frame_value(&id.to_value(), code, message, data)
}

/// `-32603` internal error with the backend message in `data` (the
/// official `into_internal_error` convention).
fn internal_error_frame(id: RequestId, message: String) -> Vec<u8> {
    let frame = error_frame(
        id.clone(),
        INTERNAL_ERROR,
        MSG_INTERNAL_ERROR,
        Some(json!(message)),
    );
    if frame.len() > MAX_RESPONSE_BYTES {
        return error_frame(id, INTERNAL_ERROR, MSG_INTERNAL_ERROR, None);
    }
    frame
}

fn result_frame(id: RequestId, result: &Value) -> Vec<u8> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "result": result,
    });
    let frame = body_or_internal(&body);
    if frame.len() > MAX_RESPONSE_BYTES {
        // Refuse, never truncate: an oversized backend result must not be
        // silently cut.
        return internal_error_frame(id, "backend result exceeds the 8 MiB response bound".into());
    }
    frame
}

/// One `session/update` notification body. The `id` member is omitted
/// entirely (the official SDK treats an id-bearing frame as a request and
/// would drop every update).
fn notification_body_bytes(method: &str, params: &Value) -> Vec<u8> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    body_or_internal(&body)
}

/// Serialize one message body without framing; the writer task applies the
/// detected connection framing.
fn body_or_internal(body: &Value) -> Vec<u8> {
    protocol::encode_body(body).expect("protocol body encodes")
}

async fn send_checked(main_tx: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>) -> Result<(), String> {
    main_tx
        .send(frame)
        .await
        .map_err(|_| "writer queue closed".to_string())
}

#[cfg(test)]
mod golden;

#[cfg(test)]
mod tests;
