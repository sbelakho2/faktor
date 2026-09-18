# Faktor architecture (frozen specification)

This document is the normative spec the implementation must satisfy.
It is stored verbatim; the code is the derived artifact.

Section numbers cited in crate comments (e.g. "spec §9") refer to the
sections below.

---

## 1. Product definition

Faktor is a native Rust engine with **Faktor-owned IDE UIs**: every
rendering surface is authored in this repository, and the daemon speaks
its own **Faktor Native Protocol v1** (§16, `docs/native-protocol.md`).
There is no foreign wire-compatibility surface and no vendored UI source
dependency; the retired compatibility corpora and bundles were removed by
explicit owner decision (only the historical attribution under
`ui/LICENSES/` remains). Each client feature below carries an explicit
status label (IMPLEMENTED / PARTIAL / BLOCKED_EXTERNAL / UNIMPLEMENTED /
CERTIFIED). The engine is a durable, journaled, bounded runtime. The LLM
is used only where reasoning is actually needed — indexing, retrieval,
compaction of historical turns, and deterministic bookkeeping are local.

**Faktor-owned UI surfaces:**

- **VS Code UI:** the extension ships its own hand-written panel —
  `apps/vscode/media/chat.js` + `chat.css` + `composer-state.js` behind the
  strict provider in `apps/vscode/src/webview.ts` (local-resource-only CSP,
  per-load nonce, text-only rendering). The panel is IMPLEMENTED and
  CI-tested against the daemon; `apps/vscode/scripts/verify-vsix.mjs`
  asserts the VSIX carries exactly the panel surface and nothing else.
- **JetBrains shell:** native Kotlin bridge in `apps/jetbrains/`
  (`:shared` + `:backend` + `:frontend` compile and smoke-test against the
  real daemon). The frontend is a native Swing tool-window panel
  (`FaktorChatPanel`) speaking Faktor Native Protocol v1 through
  `NativeClient`/`NativeEventStream`: daemon lifecycle, bearer auth over
  the protected env channel, chat/task-run/agent/usage/verification/
  evidence routing and SSE journal streaming with cursor resume.
  **Status: IMPLEMENTED (native bridge + Faktor frontend).** No vendored
  upstream IDE source exists. The executable JetBrains behavioral/visual
  matrices exist (`target/certification/jetbrains-parity.json`, HEAD-bound:
  11 behavioral rows + 8 panels rendered against pinned baselines), so the
  derived `jetbrains_behavioral_parity`/`jetbrains_visual_parity` labels
  are **IMPLEMENTED**; the kotlinc/daemon smokes are regression tests, not
  matrix results. The bridge is UI-framework independent and the same
  panel is the single rendering implementation.
- **Protocol:** the daemon's native surface is Faktor Native Protocol v1
  (`docs/native-protocol.md`): durable, cursor-paged HTTP endpoints owned
  by this runtime. The pre-cutover compatibility subsystem (frozen routes,
  DTO mirror, fixture corpus, certification replay) was **retired by
  explicit owner decision**; no foreign wire/behavior parity is claimed or
  measured.

**Non-goals:** reimplementing any foreign engine or merging foreign UI
releases wholesale. New server-side behavior ships through the native
protocol instead.

---

## 2. High-level architecture

```
┌────────────────────────── IDE clients (UI targets) ────────────────────────┐
│  VS Code Faktor panel (apps/vscode)   JetBrains Faktor bridge (apps/jetbrains)│
└────────────────────────────────────┬─────────────────────────────────────┘
                                     │ HTTP (Faktor Native Protocol v1)
                                     ▼
                    ┌───────────────────────────────────┐
                    │  faktor-server  (HTTP surface)     │  auth: FAKTOR_SERVER_PASSWORD
                    │  startup line, /native/* routes   │  (Bearer | x-faktor-server-password)
                    └────────────────┬──────────────────┘
                                     │ commands (faktor-session API, synchronous)
                                     ▼
        ┌────────────────────────────────────────────────────────────┐
        │  Agent runtime  (faktor-agent: the durable turn loop)       │
        │  drives session state, streams providers, schedules tools, │
        │  keeps context bounded (faktor-context)                     │
        └─────────┬──────────────────────────────┬───────────────────┘
                  ▼                              ▼
     ┌───────────────────────┐      ┌──────────────────────────────────────┐
     │  Provider layer       │      │  Tool runtime                         │
     │  faktor-provider       │      │  faktor-scheduler  (DAG + budgets)     │
     │  ollama openai        │      │  faktor-terminal  (supervised child)   │
     │  anthropic google     │      │  faktor-edit      (transactional)      │
     │  deepseek gateway     │      │  faktor-snapshot  (CAS checkpoints)    │
     │  (normalized chunks)  │      │  faktor-fs / git / mcp / lsp / sandbox │
     └───────────────────────┘      │  faktor-index / search / memory        │
                                    └──────────────────────────────────────┘
                                     all state exits through commands/events
                                     ▼
                    ┌──────────────────────────────────────┐
                    │  faktor-session (durable runtime)     │
                    │  journal, state machines, tool-run   │
                    │  ledger, permissions, recovery       │
                    └──────────────┬───────────────────────┘
                                   ▼
              ┌───────────────────────────────┐
              │  faktor-store (SQLite, WAL)    │
              │  faktor-cas (BLAKE3+Zstd blobs)│
              └───────────────────────────────┘
```

Rules that shape the diagram (Commandments):

1. **Dependencies point inward toward pure core types.** `faktor-core` has
   no workspace dependencies and no I/O. Provider code never touches
   session persistence; tools never mutate session state directly.
   Everything enters the runtime through commands/events.
2. **Every async operation is explicit.** No bare `tokio::spawn` defines
   application state. Operations carry `operation_id, session_id, state,
   start_time, deadline, retry_policy, cancellation_token,
   recovery_strategy` (see §7).
3. **No `if provider == "..."` in the agent.** Provider behavior is decided
   by `ModelCapabilities` and provider profiles; quirks stay inside
   adapters (see §9).
4. **Bounded everything.** No unlimited command output in RAM or context
   (ring buffer + compressed artifact, §10), no unbounded transcripts
   (paging is fundamental), no unbounded resource lifetimes (explicit
   scopes, idle unload).
5. **Zero orphans.** Every child process has a runtime owner; if the
   session dies, ownership transfers deliberately or the process dies (§10).
6. **The protocol is owned, and so is the UI.** Faktor Native Protocol v1
   (§16, `docs/native-protocol.md`) is the daemon's own HTTP contract and
   evolves with the runtime; the Faktor-owned panels in `apps/` are the
   only rendering implementations. No foreign wire surface or vendored UI
   source constrains the runtime.

---

## 3. Workspace layout

```
apps/        Faktor-owned IDE clients
  vscode/       extension host + hand-written chat panel (Faktor-owned; selftest + VSIX surface verify)
  jetbrains/    native Kotlin bridge + Swing panel (Faktor-owned; behavioral+visual matrices IMPLEMENTED, HEAD-bound)
fixtures/    protocol/, providers/, screenshots/, repositories/ test data
tests/       integration/, soak/, performance/, fault/, visual/ (adversarial-only)
crates/      the Rust engine workspace
docs/        architecture.md (this spec), native-protocol.md (Faktor Native
             Protocol v1), api-contracts.md, specs/*.md (frozen sub-specs)
ui/          historical UI attribution only (LICENSES/ for removed code)
```

Crate responsibilities (each crate's module doc is authoritative):

| Crate | Responsibility |
|---|---|
| `core` | Pure types, std-only, no workspace deps: IDs, `Error`/`ErrorKind`, `AgentState` + `SessionLifecycle` machines, `EventKind`/`Event`, `OpMeta`/`RecoveryStrategy`/`EffectStatus`, `CancellationToken`, `Clock`/`Deadline`, `RetryPolicy`, `ModelCapabilities`, `ResourceClass`/`ResourceLimits`, `Capability`/`PermissionDecision`/`NetworkPolicy`, `FileHash`, `WorkspaceIdentity`. |
| `cas` | Content-addressed blob storage: BLAKE3 identity + Zstd compression, sharded layout (`ab/cdef…`), atomic writes (temp + fsync + rename), reads verify the hash. |
| `store` | SQLite persistence: WAL, single logical writer + bounded reader pool, busy timeout, explicit transactional migrations, integrity checks, automatic backups. Large blobs live in the CAS; SQLite stores hashes. Message/part rows store JSON so the store stays protocol-agnostic. |
| `protocol` | Faktor-owned protocol types (`native` shapes for the conversation view/paging/state projection, `ApiError` mapping). The daemon's own contract lives in `faktor-server` + `faktor-core` and is specified in `docs/native-protocol.md`. |
| `session` | The durable half of a session: journaled state machine (commands append events through `StateMachine`-validated transitions), conversation view, tool-run ledger, permission requests, checkpoints, memory facts, compaction records, crash recovery, owned processes. Synchronous `Send + Sync` API on `faktor-store` + `faktor-cas`. |
| `agent` | The durable agent reasoning loop: drives the session with commands, streams providers, schedules tools through `faktor-scheduler`, keeps context bounded via `faktor-context`. No provider-name conditionals; state-aware continuation; repair once, never five times; loop detection. |
| `context` | Bounded context construction (five memory classes, §8), durable task ledger, compaction engine that cannot death-spiral, artifact writer, budget, estimator. |
| `provider` | The common provider interface hub: `Provider` trait, `GenericAgentRequest` pipeline (capability validation → normalization → adapter wire serializer → adapter transport), `ProviderRegistry`, `FakeProvider` scripted test harness, `testing` mock HTTP server. |
| `ollama` | Native Ollama adapter: discovery via `GET /api/tags`, capability probing via `GET /api/show`, native `/api/chat` streaming with tools/thinking/keep_alive, `/api/embed` embeddings; OpenAI-compatible mode is a fallback only. |
| `openai` | OpenAI Chat Completions, Responses, and OpenAI-compatible endpoints. Wire serializer produces exactly the frozen OpenAI shapes; internal option names can never leak onto the wire (locked by tests). |
| `anthropic` | Anthropic Messages adapter (stream events, `tool_use` accumulation, `input_json_delta`). |
| `google` | Gemini streaming adapter (candidates → parts → functionCall). |
| `deepseek` | First-class DeepSeek profiles (direct, OpenRouter, aggregator gateway, arbitrary compatible, local derivatives); capability normalization after discovery. |
| `gateway` | OpenRouter-style gateway adapters: OpenAI-compatible endpoint with model routing and extra headers; BYOK preserved, gateway key never persisted. |
| `scheduler` | Tool/subagent concurrency as a dependency DAG with resource-class budgets, state-aware retries with jitter, circuit breakers. Independent reads/subagents run concurrently; edits touching overlapping ownership sets do not. |
| `terminal` | Process supervision: no orphans, process groups (Unix); Windows assigns every supervised child to a `faktor-winjob` Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) with `taskkill /T` as the escalation path, 200-line ring buffer live with CAS-artifact spill, dedicated reader threads. |
| `pty` | Session-owned pseudo-terminal authority (unix PTY + Windows ConPTY): bounded drop-oldest output ring, pre-spawn config validation, and Windows children spawned `CREATE_SUSPENDED`, assigned to a `KILL_ON_JOB_CLOSE` job, then resumed before exposure — no taskkill-based guarantee. |
| `edit` | Transactional patch engine: `expected_hash` versioning, validate-against-copy then ONE atomic write, parse-before-accept via tree-sitter for supported languages. |
| `snapshot` | Native content-addressed checkpoints: before-content stored once in CAS (dedup free), rollback verifies current == recorded after-hash then atomically writes before; an independently changed file is a `Conflict`, never silently overwritten. |
| `fs` | File service and watcher: explicit workspace identity on every call, traversal/symlink-safe path resolution, bounded reads, atomic writes, idle unload of heavyweight resources. |
| `git` | Worktree management and per-repository mutation locks. Every invocation is a supervised child with an explicit owner; reads run concurrently, mutations serialize per repository (never a global Git lock). |
| `mcp` | JSON-RPC Model Context Protocol client: supervised like terminals, every invocation has a deadline, responses bounded, Content-Length framing. |
| `lsp` | Workspace-scoped language server integration: one daemon shares servers across sessions on a workspace, per-request id multiplexing, heavy servers unloaded after workspace inactivity. |
| `sandbox` | Capability-based permission enforcement: canonicalization-safe path checks (symlink escapes and `..` rejected), three-mode network policy. |
| `index` | Hybrid repository index: lexical inverted index plus tree-sitter symbol index (Rust/Python), incrementally updated, bounded by caps, workspace-isolated. |
| `search` | Hybrid retrieval with rank fusion: exact + lexical + symbol (+ optional semantic) fused by reciprocal rank weighted by symbol relevance, lexical score, semantic score, file recency, task affinity; evidence packages retrieved before serious reasoning turns. |
| `memory` | Long-term structured session memory: durable task state and structured facts (the transcript is *not* memory), compact context render for the semi-stable memory class. |
| `server` | The HTTP surface of the daemon: Faktor Native Protocol v1 (`docs/native-protocol.md`). The UI connection is disposable: turns run detached from any connection and resume from the journal. |
| `cli` | `serve` (prints the frozen startup line), `run` (headless one-prompt), `doctor` (self-check: store, CAS, integrity, permissions, providers), `sessions` (list), `acp` (ACP stdio agent server over the real daemon graph), `updater` (signed updater lifecycle, local parity), `enterprise` (retention/audit/admin plane, local parity), `worker` (remote worker-node loop), `bootstrap` (verifies the activated release and execs it), `build` (build report: version, executable sha256, release id/digest). Logging goes to stderr; stdout is the startup-line contract. |

---

## 4. Durable state machine

There is **no generic `await Promise` that determines application state**.
Every session is an explicit state machine; every transition is validated
before any write, and an illegal transition leaves **no trace** in the
journal or the session row.

### 4.1 The turn machine (`AgentState`, 17 states)

`faktor-core` defines `AgentState` (wire tags are `snake_case`):

```
Idle, Preparing, BuildingContext, WaitingForModel, Streaming,
ToolRequested, WaitingForPermission, ExecutingTool, Validating,
UpdatingMemory, ReadyForNextTurn, Completed, Cancelled,
FailedRecoverable, FailedPermanent, NeedsUserInput, Suspended
```

**Legal transitions** (`AgentState::allowed_transitions`, verified
exhaustively by tests):

| From | May transition to |
|---|---|
| `Idle` | `Preparing`, `Suspended`, `Completed`, `Cancelled` |
| `Preparing` | `BuildingContext`, `FailedRecoverable`, `Cancelled`, `Suspended` |
| `BuildingContext` | `WaitingForModel`, `FailedRecoverable`, `Cancelled`, `Suspended` |
| `WaitingForModel` | `Streaming`, `FailedRecoverable`, `Cancelled`, `Suspended`, `NeedsUserInput` |
| `Streaming` | `ToolRequested`, `Validating`, `WaitingForModel`, `FailedRecoverable`, `Cancelled`, `Suspended` |
| `ToolRequested` | `WaitingForPermission`, `ExecutingTool`, `Validating`, `Cancelled`, `Suspended` |
| `WaitingForPermission` | `ExecutingTool`, `ReadyForNextTurn`, `Cancelled`, `Suspended`, `NeedsUserInput` |
| `ExecutingTool` | `Validating`, `ToolRequested`, `FailedRecoverable`, `Cancelled`, `Suspended` |
| `Validating` | `UpdatingMemory`, `ToolRequested`, `FailedRecoverable`, `Completed`, `Cancelled`, `Suspended` |
| `UpdatingMemory` | `ReadyForNextTurn`, `WaitingForModel`, `Completed`, `FailedRecoverable`, `Cancelled`, `Suspended` |
| `ReadyForNextTurn` | `Preparing`, `Completed`, `Cancelled`, `Suspended`, `NeedsUserInput` |
| `Completed` | *(none — terminal)* |
| `Cancelled` | `Preparing`, `ReadyForNextTurn`, `Suspended` |
| `FailedPermanent` | *(none — terminal)* |
| `FailedRecoverable` | `Preparing`, `Idle`, `Cancelled`, `Suspended`, `NeedsUserInput` |
| `NeedsUserInput` | `ReadyForNextTurn`, `Preparing`, `Cancelled`, `Suspended` |
| `Suspended` | `Idle`, `Preparing`, `Cancelled`, `Completed` |

Contract rules:

- **Self-transitions are legal and idempotent** — replay of re-emitted
  events must not fail.
- **Skipping states is rejected** (e.g. `Streaming → UpdatingMemory`
  without `Validating` is illegal).
- **`Cancelled` is a turn outcome, not a session outcome.** Stop in the IDE
  cancels the turn; the chat stays promptable (`Cancelled → Preparing` and
  `Cancelled → ReadyForNextTurn` are legal). `is_terminal()` is true only
  for `Completed` and `FailedPermanent`.
- `is_active()` = not terminal and not `Idle`/`Suspended`.
- `StateMachine::force` exists for recovery/replay only, never normal
  flow; setting a terminal state that has recorded later events is a
  corruption sign — callers must validate against the journal before
  forcing.

### 4.2 The session LIFETIME machine (`SessionLifecycle`, 5 states)

The per-turn machine alone cannot express session lifetime — a session is
Open for days across many turns. `SessionLifecycle` is **orthogonal** to
`AgentState` and persists on the session row:

```
Open, Suspended, Closing, Closed, FailedPermanent
```

**Legal transitions** (`SessionLifecycle::allowed_transitions`):

| From | May transition to |
|---|---|
| `Open` | `Suspended`, `Closing`, `Closed`, `FailedPermanent` |
| `Suspended` | `Open`, `Closing`, `Closed`, `FailedPermanent` |
| `Closing` | `Closed`, `FailedPermanent`, `Open` |
| `Closed` | *(none — terminal)* |
| `FailedPermanent` | *(none — terminal)* |

Contract rules:

- **Prompts are gated on `Open`** (`can_accept_prompts()`). A prompt on a
  `Suspended` session auto-resumes it (`Suspended → Open`); prompts on
  `Closing`/`Closed`/`FailedPermanent` are rejected with `Conflict`.
- **`end_session()` is the only normal terminal route.** It journals
  `SessionEnded` and moves the lifecycle `→ Closed`. Registered child
  processes block `end_session` until released or deliberately
  transferred (zero orphans).
- **`abort` cancels the turn, never the session.** It cancels every
  tracked operation's token, journals `ToolCancelled` (tool ops) or
  `Failed {"error":"aborted"}` (turn ops) landing state `Cancelled`, then
  journals `TurnCompleted` landing **`ReadyForNextTurn`**. The session
  stays promptable.
- `FailedPermanent` is reachable only as a documented escalation from
  `FailedRecoverable` (journal validation requires the two-step hop).

---

## 5. The append-only event journal

Every significant transition becomes a journal event. The session database
therefore knows exactly what happened; the rendered conversation is a
*view derived from the journal*, never the source of truth.

### 5.1 Event kinds (27, frozen — the 24 original + PromptAdmitted, PhaseChanged and ReplayStarted; exactly one TurnCompleted per logical turn)

```
SessionCreated, PromptReceived, ContextPrepared, ModelStarted,
ModelChunkReceived, ToolRequested, ToolStarted, FileChanged,
ToolCompleted, ToolCancelled, CheckpointCreated, ContextCompacted,
CompactRejected, SubagentStarted, SubagentCompleted, TurnCompleted,
PermissionGranted, PermissionDenied, PromptAdmitted, PhaseChanged,
ReplayStarted, CrashDetected, RecoveryApplied, SessionEnded, Suspended,
Resumed, Failed
```

### 5.2 Sequencing and structure

- Each session has a **gapless per-session sequence** (`EventSeq`, starts
  at 1 with `SessionCreated`); the sequence is the SSE resume cursor.
- `Event { seq, session_id, op_id: Option<OpId>, kind, state, ts_ms,
  payload: Option<Value> }`. The `state` column is the machine state the
  event *lands on*; every event's state must be a legal transition from
  the previous state (`validate_transition` in the session journal).
- Two kinds carry documented sub-chains: `ToolRequested` events are
  recorded with state `WaitingForPermission` (hop `ToolRequested` then
  `WaitingForPermission`, both must be legal); `Failed` recorded with
  state `FailedPermanent` is legal only via the two-step
  `FailedRecoverable`-then-force escalation.
- `StateMachine::transition` is validated **before** any write; illegal
  transitions return `ErrorKind::InvalidState` and leave no trace.

### 5.3 Durable vs ephemeral split

- **Durable (journaled):** model lifecycle (`ModelStarted`,
  `ModelChunkReceived` with `message_id` + `text_len` only — never the
  text), tool lifecycle (`ToolRequested/Started/Completed/Cancelled`),
  `FileChanged`, checkpoints (`CheckpointCreated`), compaction
  (`ContextCompacted`/`CompactRejected`), turn (`PromptReceived`,
  `TurnCompleted`), permissions, crash recovery, session lifecycle.
- **The text itself is not journaled per chunk.** Chunk text accumulates
  in message *parts* (SQLite JSON rows); the journal records lengths so
  replay can re-derive shape without unbounded journal growth.
- **Ephemeral/coalesced in memory only:** live model text and reasoning
  deltas flow through the agent's bounded `ChunkSink`: a 1024-event mpsc
  channel; when it is full, consecutive same-(session,message,kind) deltas
  coalesce into ONE pending frame capped at 64 KiB keeping the newest
  bytes, and a different stream replaces the pending frame. The server
  drains that channel eagerly because the client surface is durable and
  journal-driven — there is no global SSE bus, no `DeltaCoalescer` and no
  stored-part re-diff (`recover_text`) anywhere in the runtime. Terminal
  output is reconstructed from the durable `terminal_*` ledger rows and
  live ring snapshots, never journaled as text.

### 5.4 Journal validation

`replay_journal()` replays the full journal through the state machine and
fails with `Internal` on corruption (skipped states, illegal sequences).
Recovery uses it to detect that a crash never left a corrupt trace.

---

## 6. Crash recovery

**Never blindly re-run a command after a crash.** Unfinished operations
are reconstructed from durable state; deterministic FS ops verify the
expected resulting hash; unknown external effects are marked
`effect_status = unknown` and forced to verification.

### 6.1 The five `RecoveryStrategy` variants (core)

| Variant | Meaning | Recovery action |
|---|---|---|
| `VerifyHash { path, expected: FileHash }` | Deterministic FS op | Hash the file (through the CAS, bounded read). Matches → the op completed before the crash (`completed` / `verified`); mismatch or missing → the op truly never ran (`failed` / `failed`). |
| `MarkUnknown` | Command with unknown external effects | Record `effect_status = unknown` (`interrupted`) and force verification instead of re-running. |
| `Idempotent` | Safe to re-run (reads, idempotent calls) | Mark failed so the scheduler may re-run. |
| `Manual` | Never re-run automatically | Require a human (`interrupted`, `needs_human`). |
| `None` | No recovery action | Record `interrupted`, no action. |

### 6.2 How recovery decides

`SessionHandle::recover_all` (and the agent's `recover_session` before a
turn):

1. Scans durable `tool_run` rows still `running` (the only durable record
   that survives a crash) and applies each row's recorded strategy.
2. Journals `CrashDetected`, then one `RecoveryApplied` per recovered op
   with `status`, `effect`, and `action` tags
   (`verified`/`not_applied`/`unknown_effect`/`rerun_allowed`/
   `needs_human`/`no_action`).
3. **Idempotent:** a second sweep finds nothing pending and appends
   nothing (test-locked).
4. **Interrupted turn detection:** an op-active session with *no* pending
   tool rows means the crash hit the model stream itself — journal
   `CrashDetected` and land on the honest target: `FailedRecoverable` →
   `WaitingForPermission` (durable permission requests are resumable only
   from that state) → `NeedsUserInput` → stay put.
5. **Contradiction detection:** pending tool rows while the journal says
   idle/suspended/terminal → fix the rows, keep the state, set the
   `contradiction` flag.
6. **Tamper detection:** the `expected_hash` column must agree with the
   strategy's `expected` hash, or recovery fails `Malformed`; an unknown
   strategy tag in a row is `Malformed`, never silently defaulted.
7. **Orphans:** children owned at restart are reported and cleared — the
   runtime must never pretend to own zombies (parent-death handling is the
   OS's job; the runtime's is to notice and record).
8. Hash verification is performed **through the CAS** (`SystemFileHasher`
   puts the verified content into the CAS as a deduplicated audit
   artifact, bounded by `MAX_VERIFY_BYTES`).

### 6.3 `EffectStatus` semantics

`Unknown | Verified | Applied | Failed`, persisted on `tool_run` rows:

- `Unknown` — effects not determinable (crash mid-command); the op is
  forced to verification before its output may be reused.
- `Verified` — a deterministic check confirmed the effect (hash match).
- `Applied` — the op completed normally during the turn.
- `Failed` — the op failed (or hash mismatch proves it never ran).

---

## 7. Operation metadata and the one-operation model

### 7.1 `OpMeta` (core)

Every async operation carries the full envelope from the spec:

```rust
pub struct OpMeta {
    pub operation_id: OpId,
    pub session_id: SessionId,
    pub state: OpState,          // Pending | Running | Done | Failed | Cancelled
    pub start_time_ms: i64,
    pub deadline: Deadline,      // ensure_alive(): cancelled → Cancelled, expired → Timeout
    pub retry_policy: RetryPolicy,
    pub cancellation: CancellationToken,
    pub recovery: RecoveryStrategy,
}
```

`ensure_alive(now_ms)` fails fast on cancellation or deadline expiry
*before any work*.

### 7.2 The one-operation-model rule

A single operation has **exactly one identity — one `OpId`** — shared by
every component that touches it:

- the **scheduler** `ScheduledOp { meta: OpMeta, ... }` (same id, same deadline,
  same retry policy, child cancellation token);
- the **durable tool-run row** (`start_tool_run(op_meta.clone(), ...)` in
  `faktor-session`, which also registers the op + token in the session's
  `OpRegistry` so abort fans cancellation out);
- the **provider request** (`RequestMeta { operation_id, session_id,
  provider, attempt, deadline_ms, cancellation }` — never serialized to
  the wire);
- the **journal** events carrying `op_id`.

There is no second identity space: cancel the `OpId`, and scheduler task,
tool row, provider stream, and journal all observe the same token.
Turn ops are `OpKind::Turn`; tool ops are `OpKind::Tool` (drives abort's
event kind). Tools are never blindly retried: their `OpMeta` retry policy
has `max_attempts: 1`; retries are a scheduler concern with explicit
policies.

---

## 8. Context engine

### 8.1 Five memory classes (spec §8)

| Class | Content | Treatment |
|---|---|---|
| 1. Immutable instructions | system prompt, tool schemas, project rules | static, cacheable prefix |
| 2. Durable task state | the task ledger (decisions, constraints, files touched) | compact, kept whole |
| 3. Repository knowledge | retrieved evidence (exact/lexical/symbol/semantic) | bounded by budget |
| 4. Recent conversation | recent turns from the message view | paged, bounded, oldest archived |
| 5. Historical artifacts | summarized/compacted history | CAS artifacts + summaries |

The budget is enforced **BEFORE anything is sent to a provider** — the
engine never discovers the limit from a provider error.

### 8.2 `ContextBudget` — the 32K local profile math

Default (`ModelCapabilities::small_local()` profile — context ≤ 32,768):

```
system         5,000
tools          0
working        3,000
retrieved      7,000
recent        10,000
output_reserve 5,000
safety         2,000
────────────── ──────
total         32,000   (exact, test-locked)
context_max   25,000   (total − output_reserve − safety; test-locked)
```

Larger contexts scale proportionally (working 4K; recent = clamp
(context/8, 10K..60K); retrieved = clamp(context/12, 7K..24K); output
reserve = max_output + 1K capped at context/4; safety = max(context/32,
2K); system = clamp(context/16, 5K..12K); tools = remainder capped 8K).
Hostile capability metadata (usize::MAX, zero context) saturates, never
panics (test-locked).

### 8.3 Compaction hard invariant

`faktor-context` + `faktor-session` enforce, with **no exceptions**:

- A successful compaction must land **at or below the target** (`after ≤
  target`) **and** achieve the configured minimum reduction: `reduction =
  1 − after/before ≥ min_reduction_ratio` (**default 0.25**).
- A "summary" that reduces context by ~1% is **rejected** with a
  `CompactRejected` journal event, and deterministic pruning takes over.
- `hard_cap = min(target, before × (1 − ratio))` — the compactor can never
  accept a plan above it.
- **Death-spiral convergence:** the compactor retries with the *plan's*
  after-tokens as the new before only while it is still converging toward
  the hard cap; a compaction that cannot shrink is terminal. Compaction is
  an **interior event**: the session state does not move.
- Accepted strategies: `LlmSummary` (weak-but-honest summarizer output is
  rejected when it does not shrink enough) or `DeterministicPruning`
  (task ledger preserved whole; only old recent turns archived).
- Proactive trigger: effective usage (`used / context_max`) ≥
  `compact_at_usage` (default 0.65) before a provider call.

### 8.4 Prefix caching ordering

The assembler orders memory classes for prefix caching:

```
StaticPrefix     (instructions + tools + project rules)   — fully cacheable
SemiStable       (task ledger/durable state)              — cacheable with prefix
Volatile         (recent turns, evidence)                 — always after
```

The static prefix plus semi-stable tokens form the provider-cacheable
prefix; volatile content is appended last so provider prompt caching is
not invalidated by every new turn.

---

## 9. Provider architecture

### 9.1 The request pipeline

Every provider call passes the same pipeline:

```
GenericAgentRequest
        ↓
CapabilityValidation   (CapabilityValidator: tools/reasoning/max_output vs ModelCapabilities;
                         violations are loud errors, never silent truncation)
        ↓
Provider Normalizer    (RequestNormalizer → NormalizedRequest: whitelisted fields only;
                         internal meta — operation_id/session_id/attempt/deadline/cancellation —
                         structurally cannot leak onto the wire; 7-field frozen shape, test-locked)
        ↓
Wire Serializer        (inside each adapter: exactly the frozen vendor shapes)
        ↓
HTTP Transport         (inside each adapter; state-aware retries, circuit breakers)
```

`GenericAgentRequest { model, system, messages, tools, max_output,
reasoning, stream, meta }`; chunks are normalized
(`Text | Reasoning | ToolCall{id,name,input,complete} | Usage | Done`);
errors are `ProviderError { kind, message, retryable, code }` with
retryability decided by kind (`Network | Timeout | RateLimited | Server`
retryable; `BadRequest | Auth | Cancelled | Malformed` not).

### 9.2 `ModelCapabilities` is the only behavior switch

`context, max_output, tools, parallel_tools, thinking, vision,
json_schema, streaming, embeddings, reasoning`. Defaults are conservative
(no tools, no thinking, small context) so an unprobed model **fails safe,
not loud**. There is **no `if provider == "…"` anywhere in the agent** —
the agent reads capabilities; adapters set them; provider quirks stay
inside adapters. Capabilities come from discovery/probing, never
hard-coded lists.

### 9.3 Adapter families

| Family | Wire | Notes |
|---|---|---|
| `ollama` | native `/api/chat` | Discovery via `GET /api/tags` (never a hard-coded list — `ollama pull` makes models appear), capability probing via `GET /api/show`, native streaming with tools/thinking/keep_alive, `/api/embed` embeddings; OpenAI-compatible mode is a fallback only. |
| `openai` | Chat Completions, Responses, OpenAI-compatible | Wire serializer locked by tests so internal option names can never leak. |
| `anthropic` | Messages API | Owns stream events, `tool_use` accumulation, `input_json_delta`. |
| `google` | Gemini | candidates → parts → functionCall normalization. |
| `deepseek` | OpenAI-shaped | First-class profiles: `Direct` (api.deepseek.com), `OpenRouter`, `Gateway`, `Compatible` (arbitrary endpoint), `LocalDerivative`. Capability normalization after discovery; all profiles produce capability-driven behavior (test-locked matrix). |
| `gateway` | OpenAI-compatible | OpenRouter-style model routing + extra headers; BYOK preserved, gateway key configured per provider and never persisted by the runtime. |

### 9.4 Registry and test matrix

- `ProviderRegistry` maps provider id → `Arc<dyn Provider>`; the agent
  asks the registry, never a provider string.
- `FakeProvider` is a scripted, adversarial harness (`with_script`,
  `die_mid_stream`, `inject_rate_limit`; scripts consumed exactly once —
  a replaying provider would let the loop re-execute forever).
- `provider::testing` is a mock HTTP server for wire-level adversarial
  tests.
- The capability-driven test matrix runs the same agent-level behaviors
  against every profile/family (e.g. DeepSeek's
  `all_profiles_produce_capability_driven_behavior`).

---

## 10. Tool runtime

### 10.1 Transactional editing (faktor-edit)

- Every agent edit is **optimistic and versioned**: `expected_hash` must
  match the current content or the edit is rejected **before any write**
  (`Conflict`).
- All ops are validated against a copy first, then applied with **ONE
  atomic write** (temp + fsync + rename) — no partial writes.
- **Parse-before-accept:** for supported languages (Rust, Python via
  tree-sitter), if the original parses cleanly and the edited version does
  not, the edit is suspicious: roll back (or flag, per mode).
- `FileHash` is the BLAKE3 identity used everywhere (CAS, edits,
  checkpoints, recovery).

### 10.2 Native CAS checkpoints (faktor-snapshot)

- Snapshots are **not Git repositories pretending to be undo history**.
  Before changing a file its original content is stored once in the CAS
  (BLAKE3 + Zstd; dedup is free: ten checkpoints of the same unchanged
  file = one copy). Git stays for branches/commits/worktrees/diffs only.
- `CheckpointRow { before_hash, after_hash }` records each write.
- **Rollback verification:** restore verifies the current file hash equals
  the recorded `after_hash`, then atomically writes the `before` content.
  An independently changed file is a `RollbackOutcome::Conflict`, never
  silently overwritten.

### 10.3 Process supervision (faktor-terminal)

- **Zero orphans.** Every child has a runtime owner (`ProcessOwner`:
  `Session | Workspace | Daemon`); kill targets the whole process group
  (Unix). On Windows every supervised child is assigned to a
  `faktor-winjob` Job created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
  (`crates/winjob`, re-exported as `JobGuard` through `faktor-terminal`);
  `taskkill /T` remains only the escalation path — daemon death terminates
  the whole tree through the OS. `transfer` is the deliberate ownership
  handoff; `kill_all_for(owner)` runs on session death. Children
  registered on a session block `end_session` until released/transferred.
  The session PTY authority lives in `crates/pty` (unix PTY; Windows ConPTY
  via `CreatePseudoConsole`): its children spawn `CREATE_SUSPENDED`, are
  assigned to the kill-on-close job, and are resumed before `Pty::spawn`
  returns, so no PTY child ever executes outside the job.
- **Bounded output.** A 200-line ring buffer is live; overflow spills to a
  CAS artifact (`artifact_max` default 100 MiB) — a 300 MB log never
  becomes a 300 MB RAM object. `CommandOutput { excerpt, exit_code,
  artifact, slice_hint, ring_lines }`; blocking pipe reads live on
  dedicated reader threads so they can never stall the async loop.
- `reap()` collects exited children (no zombies); kill is
  SIGTERM → grace → SIGKILL.

### 10.4 MCP / LSP supervision

- **MCP** (faktor-mcp): JSON-RPC over Content-Length framing; processes are
  supervised exactly like terminals (crashes, hangs, and garbage output
  never destabilize the agent runtime); every invocation has a deadline;
  responses are bounded.
- **LSP** (faktor-lsp): language servers are **workspace resources, not
  session resources** — one daemon shares `rust-analyzer` /
  `typescript-language-server` / `pyright` across sessions on the same
  workspace; requests multiplexed with per-request ids; heavy servers
  unloaded after workspace inactivity (idle unload).

### 10.5 Sandbox capabilities and network policy (faktor-sandbox)

- Permissions are expressed as `Capability` objects (`ReadWorkspace`,
  `WriteWorkspace`, `ReadExternal`, `WriteExternal`, `ExecuteShell`,
  `Network`, `Mcp`, `Git`), never scattered conditionals.
- Path checks are canonicalization-safe: symlink escapes and `..`
  traversal are rejected.
- `NetworkPolicy` is the three-mode sandbox: `DenyAll` /
  `AllowProviders { endpoints }` / `AllowConfigured { endpoints, domains }`.
- Tool-call JSON parsing has three modes (`ToolCallMode`): `Native`,
  `NativeWithRepair` (ONE deterministic repair pass on almost-valid JSON —
  repair happens once, never five times), `StructuredFallback` (extract
  from text); repeated identical failures trip the loop detector.
- **Loop detection** (threshold 3): the same normalized tool call
  (name + sorted args), same failure, or same patch key stops the turn
  (`loop_stopped`, lands `FailedRecoverable`) instead of repeating for 40
  turns.

---

## 11. Networking and streaming

### 11.1 Single async stack

- `faktor-core` is std-only (no tokio). Everything else uses one tokio
  runtime; the server is axum; `faktor-store`/`faktor-cas` are synchronous
  and heavy calls are wrapped in `tokio::task::spawn_blocking`.
- No bare `tokio::spawn` defines application state (§4); the SSE bus is
  polled lazily by connected streams — no background task, no unbounded
  lifetime.

### 11.2 State-aware retries and circuit breakers

- `RetryPolicy { max_attempts, base_delay_ms, max_delay_ms, jitter,
  class: RetryClass }` with `RetryClass { Network, RateLimited,
  ServerError, Always }`; `next_delay` adds jitter (full jitter),
  `should_retry` respects the attempt cap.
- The scheduler's execution loop combines retry-with-jitter with a
  **circuit breaker** per task name (`Closed → Open → HalfOpen` probe);
  budget-busy is a distinct `BudgetBusy` signal, never an error.

### 11.3 Streaming surfaces

- **Per-session journal SSE:** `GET /native/session/{id}/events?after=<seq>`
  streams durable journal frames (`id: <seq>`, `event: <kind>`, `data:` the
  `/native/events` row shape); `after` replays `seq > after` (0 = from the
  beginning). Catch-up is paged (≤ 256 events per poll), so a reconnect
  against a huge journal never balloons RAM and resumes exactly from the
  cursor. While the page is exhausted the stream emits `event: heartbeat`
  frames (250 ms poll cadence, 5 s SSE keep-alive), so it stays open.
- **Cursor pages:** `GET /native/events?session=&after=&limit=` serves the
  same durable read as a strict-DTO page (`hasMore`/`nextCursor`);
  `GET /native/messages?session=&before=&limit=` pages durable message rows
  newest first (`hasMore`/`nextBefore`); `GET /native/providers` lists the
  registered provider instances. There is no global event bus, no
  `GlobalEvent` envelope and no coalescing layer on the client surface:
  live provider deltas are an in-process `ChunkSink` the daemon drains, and
  clients observe durable truth by cursor.
- **Resume semantics:** `after` replays events with id > n; hostile or
  oversized cursors are never turned into errors — the stream stays open
  and the client re-receives from the resulting cursor. The UI connection
  is disposable: turns run detached from any SSE connection and resume from
  the journal.

---

## 12. Scheduler

`faktor-scheduler` schedules tool/subagent work as a **dependency DAG** of
`ScheduledOp` values — the whole `OpMeta` envelope travels with the op, so
the scheduler never builds a second identity:

```rust
pub struct ScheduledOp {
    pub meta: OpMeta,                                    // identity, deadline, retry, cancellation, recovery
    pub resources: ResourceRequest,                      // which ResourceClass budget it draws from
    pub reads: OwnershipSet,                             // files read (dependency overlap analysis)
    pub writes: OwnershipSet,                            // files written (overlapping writes serialize)
    pub dependencies: Vec<(OpId, DependencyPolicy)>,     // per-edge gate policy
    pub run: OpFn,
}
```

Each dependency edge carries a `DependencyPolicy`:

- `Success` (default) — the dependent runs only if the upstream ended
  `Done`; an upstream that `Failed`, was `Cancelled`, or was itself
  `Blocked` leaves the edge dead and the dependent can never run
  (`TaskStatus::Blocked`, propagated transitively).
- `Terminal` — the dependent runs after any terminal execution state
  (`Done | Failed | Cancelled`); a `Blocked` upstream does not satisfy it.
- `Always` — cleanup/finalizer edge: satisfied by any terminal upstream
  state, including `Blocked`.

- **Resource classes and budgets** (`ResourceClass`, 9 classes; default
  in-flight limits): `Model 1`, `DiskRead 16`, `DiskWrite 4`, `Cpu 2`,
  `Git 1` (serialized per repo in faktor-git), `Network 8`, `Terminal 2`,
  `Mcp 4`, `Indexing 1` (deliberately low — indexing yields). A class
  budget exists so e.g. embedding/indexing work can never starve an
  interactive session.
- **Event-driven scheduling, permit-before-spawn, no wave barriers.**
  `run_to_completion` validates the DAG, then drains a ready FIFO into a
  tokio `JoinSet`: a task starts only when every edge is satisfied AND it
  holds a resource permit (`try_acquire` before spawning); a task that
  finds the budget full returns `BudgetBusy` and cycles back to the
  ready-queue tail for the next drain — no artificial synchronization
  across rounds. Completion of any task immediately frees its permit and
  decrements its dependents, so a long task never gates short tasks that
  became ready after it.
- **Deadlock detection:** `validate()` rejects unknown dependencies and
  cycles before any run (`ErrorKind::Deadlock`); if every ready task is
  budget-blocked and nothing else is making progress, that is a
  configuration deadlock and errors loudly.
- **Ownership overlap:** independent reads/subagents run concurrently;
  edits touching overlapping `writes` ownership sets do not. A failed or
  blocked task does not deadlock the DAG — `Terminal`/`Always` edges let
  dependents run, and `Blocked` propagates instead of hanging.
- Every task honors deadline, cancellation, retry-with-jitter, and
  circuit breaking (`CircuitBreaker`: `Closed → Open → HalfOpen` probe);
  task status is observable via `status(id)`/`statuses()`.

---

## 13. Performance budgets (§37)

Numbers are enforced by tests as gates. `cargo test --workspace` runs
them; soak/perf runs are `#[ignore]`-gated (see §14).

| Budget | Value | Enforced by |
|---|---|---|
| Context total (small/local profile) | 32,000 tokens (5K+3K+7K+10K+5K+2K) | `context::budget` exact-math tests |
| Context usable before provider call | 25,000 tokens (context_max) | same |
| Compaction minimum reduction | 25% (`min_reduction_ratio` default 0.25) | `session::compaction`, `context::compactor` |
| Compaction convergence | must land ≤ hard_cap, never grow | compactor tests |
| Terminal output ring | 200 lines live; artifact spill ≥ 100 MiB | terminal tests |
| Tool result excerpt | truncated to 2000 chars on the wire | agent runtime |
| Agent chunk coalescing | 1024-event bounded channel; 64 KiB drop-oldest pending frame | agent `ChunkSink` tests |
| Journal SSE catch-up page | ≤ 256 events per poll, cursor-resumable | server native SSE tests |
| Message page | ≤ 200 messages per page, `has_more` | session paging tests |
| Prompt bound | 512 KiB (max 64 files, 4 KiB path each) | session bounds |
| Message / part / tool-args / ledger / artifact / verify caps | 1 MiB / 4 MiB / 4 MiB / 1 MiB / 64 MiB / 64 MiB | session bounds |
| Turn deadline | layered: ONE logical turn ≤ 30 min per slice (`DEFAULT_TURN_BUDGET_MS`); the task spans many slices and is bounded only by its durable budget (tokens/turns/money) — the historic 24 h `TURN_DEADLINE_MS` is gone | session layered-budget tests |
| Scheduler parallelism gate | 32 tasks × 30 ms at DiskRead 16 completes < 400 ms | scheduler perf test |
| Loop detector threshold | 3 identical normalized calls | agent loop tests |

---

## 14. Testing philosophy

**Adversarial-only.** Every test attempts to break the system: crash
mid-operation, corrupt storage, truncate streams, race writers, malicious
payloads, oversized input, path traversal, illegal state transitions,
duplicate replay, out-of-order events. Happy-path-only tests are rejected
in review. The harness is adversarial too — `FakeProvider` scripts die
mid-stream, inject rate limits, and emit malformed tool calls; recovery
tests forge crash states (running `tool_run` rows, forced journal
entries) and verify idempotent, tamper-loud behavior.

Test crates (each an independent workspace member):

- `tests/integration` — end-to-end behavior against the daemon: crash
  recovery, permissions, compaction, paging and the native HTTP surface.
- `tests/fault` — fault injection: crash, corruption, truncation, races.
- `tests/soak` — long-run stability (memory bounds, journal growth).
- `tests/performance` — perf gates (§13) — `[perf]`.
- `tests/visual` — pixel-level UI compatibility: zero-pixel-diff against
  `fixtures/screenshots` **outside branding masks** (`BrandingMask` +
  `compose_plus_overlay` for the "+" overlay region); fixtures regenerated
  by an ignored `[visual]` test.

Ignore conventions (AGENTS.md, verified by CI):

- Long tests are `#[ignore]`-gated and named with `[soak]`, `[perf]`,
  `[visual]`, or `[fault]` prefixes; they do not run in normal CI.
- `cargo test --workspace` runs everything else; each crate's unit tests
  must pass before the crate is considered done.

---

## 15. Build and migration strategy

**Build:** a single Rust workspace (`cargo build --workspace`); no
external runtime beyond the native binary and SQLite. Native targets:
macOS, Linux, Windows (process groups on Unix; Windows children are
assigned to `faktor-winjob` kill-on-close Job Objects, with ConPTY children
spawned suspended → assigned → resumed before exposure).

**Migration stages A–K** (from the original spec) — the derived client
compatibility shells (status labels in §1) are served at every stage, so
the product never regresses:

| Stage | Deliverable |
|---|---|
| A. Freeze | lock the Faktor-owned UI baselines (the VS Code panel selftest/VSIX gates and the JetBrains behavioral/visual matrices) |
| C. Persistence | `faktor-store` + `faktor-cas` + the durable session state machine (journal, tool-run ledger, recovery) |
| D. Providers | `faktor-provider` hub + adapter families + registry + capability normalization |
| E. Tools | transactional edit, native snapshots, fs/git, terminal supervision, MCP/LSP, sandbox |
| F. Agent loop | the durable turn machine driven by `faktor-agent` on top of the session |
| G. Context | budgets, five memory classes, ledger, death-spiral-proof compaction |
| H. Indexing | hybrid index + rank-fused retrieval + structured memory |
| I. Snapshots | CAS checkpoints with rollback verification wired into edits |
| J. Agent manager | daemon-owned background agents (Agent Manager cards) |
| K. Compat retirement (DONE) | the optional pre-cutover wire-compatibility surface + fixtures were removed by explicit owner decision — nothing in the runtime depended on them; the native protocol is the daemon's own surface. The Faktor-owned-UI migration then removed every vendored UI corpus/bundle and the message-ABI bridge. |

---

## 16. Faktor Native Protocol v1

The daemon's own HTTP contract is **Faktor Native Protocol v1**
(`docs/native-protocol.md`): session/turn/task/operation lifetimes,
cursor-based paging, and the native endpoints under `/native/...` plus the
projection/catalog surfaces. It is not a compatibility artifact — it
evolves with the runtime, and its tests are ordinary crate tests.

"v1" is a documentation label, not a wire constant: the code defines no
native protocol version constant. `faktor-core` exports only `VERSION`
(the crate version) and `UX_BASELINE`; the contract is the daemon's route
set plus its strict DTOs.

The **pre-cutover wire-compatibility subsystem was retired by explicit
owner decision**: the frozen routes, the mirrored DTOs, the fixture corpus,
the fixture generators/replayers and the compatibility certification lane
no longer exist. No foreign wire/behavior parity is claimed or measured.
The vendored UI sources were removed as well; the Faktor-owned panels are
the only rendering implementations.

- **Startup line:** `faktor-cli serve --port 0` prints exactly
  `faktor server listening on http://127.0.0.1:<port>` and **nothing else**
  to stdout (logging goes to stderr). The client parses stdout for this
  line; there is no JSON handshake. The password never appears on stdout.
- **Auth:** the frontend generates a 64-hex `FAKTOR_SERVER_PASSWORD` and
  passes it via the environment. Every endpoint is authenticated with
  `Authorization: Bearer <password>` or `x-faktor-server-password:
  <password>`; the legacy per-start `AuthToken` rides the same Bearer
  header. The pre-cutover `Basic` compatibility form is **gone** (a Basic
  header is rejected like any other unknown scheme; see the migration note
  in `crates/server/src/auth.rs`); wrong/missing credentials → 401.
- **Native surface:** `/session/{id}/projection`, `/models`,
  `/capabilities`, and the `/native/...` mounts (health/ready, usage,
  session listings, task runs, tournaments, terminals, attachments,
  evidence, semantic introspection, messages/events cursor pages). See
  `docs/native-protocol.md` for the endpoint list and paging semantics.
- **Strictness:** `deny_unknown_fields` on request bodies (unknown fields
  → 400); unknown sessions → 404; malformed ids/empty prompts → 400;
  oversized → 413-class `Oversized`; protocol drift is loud, never silent.
- **Error mapping:** `faktor_protocol::error::from_core` maps every
  `faktor-core` error to an `ApiError { code, message, http_status,
  retryable }`.

---

## 17. Daemon configuration (`faktor-plus.json`)

The daemon config file is strict JSON: `config_version` 1 is the only
accepted version (absent defaults to 1), unknown keys anywhere are parse
errors, and provider keys are referenced by environment-variable name —
the runtime never stores secrets. Base sections (`providers`, `mcp`,
`verification`, `sandbox`, `tasks`, `completion`, `efficiency`,
`embeddings`) keep their crate docs; the parsed-but-previously-undocumented
sections are:

| Section | Purpose | Defaults |
|---|---|---|
| top-level `semantic` | Additive semantic-provider registry config. Extracted and strictly parsed from the raw document BEFORE the frozen `Config` shape sees it (`serve_config_and_semantic`), so the key never reaches the strict layer. | Absent/`null` keeps `graph::SemanticCfg::default()`: fallback-only registry, optional `max_payload_bytes`/`max_entity_refs` caps, `providers` empty. Unknown keys inside the section fail startup. |
| `[cloud]` | Commercial control plane (identity/org/RBAC) and the durable SCM store. | `enabled = false` (every control-plane route answers 409 `cloud_disabled`, no database file); `database = control-plane.db`, `scm_database = scm.db`, `payload_dir = payloads`; optional `[cloud.sso]` and `[cloud.github_app]` sub-sections, both disabled by default. |
| `[billing]` | Commercial metering service: durable usage ledger, entitlements and credits. | `enabled = false` (billing routes answer 409 `billing_disabled`, no billing database); `database = billing.db`; `organization` required when enabled; `account`/`account_name` optional; `managed = true` (Faktor-managed spend may debit credits; `false` = BYOK-only); `default_plan`/`plans`/`managed_providers` are the only feature/limit and managed-provider sources; `[billing.report]` disabled (interval 60 s, period 24 h, catch-up cap 24, attempts cap 5). |
| `[updater]` | Signed updater/distribution lifecycle: channels, ed25519 operator keys, staged content-addressed installs. | `enabled = false` (409 `updater_disabled`, no `update.db` and no install directory); `channel = stable`; `install_root = install`; `keys` required when enabled (an empty allowlist refuses every manifest); `max_artifact_bytes = 256 MiB` (cap 4 GiB); `clock_skew_ms = 5 min` (cap 1 h); `allow_legacy_manifests_once = false`. |
| `[workers]` | Remote/VPC worker plane: registration tokens, immutable job generations, leases, heartbeats, placement seam. | `enabled = false` (409 `workers_disabled`, no worker database, local execution unchanged); `database = workers.db`; `organization` required when enabled; `trust_domain` defaults to the organization id; placement requirements `os`/`arch`/`toolchains`/`network`/`region`/`min_cpu_cores`/`min_memory_mb`/`gpu` default empty = any registered worker of the trust domain. |
| `[worker_plane]` | The deployment boundary of the worker HTTP surface: a SECOND listener with its own identity, separate from the loopback native listener. | `enabled = false` (no second socket); `bind = 127.0.0.1:8790` (a non-loopback bind is refused unless `trusted_gateway = true` acknowledges the gateway); `tls = false` (`true` is a typed startup refusal — no inbound TLS stack is compiled); `auth = worker_tokens` (`gateway_mtls` requires `trusted_gateway`); `bearer` optional transport credential; the daemon password is never accepted on the worker socket. Enabling it requires `[workers] enabled = true`. |
| `[enterprise]` | Enterprise retention/audit/admin plane: retention classes + guarded GC, append-only audit ledger, deletion jobs, admin settings, layered config. | `enabled = false` (409 `enterprise_disabled`, no enterprise database); `database = enterprise.db`; `organization` required when enabled; `policy`/`preferences` are the local organization-policy and user-preference layers (empty = no layer). |
| `[worker_node]` | THIS host acting as a remote worker node of a control plane (the client half of `[workers]`). | `enabled = false` (`faktor worker run` refuses typed before any network or filesystem effect); when enabled `worker_id`, `control_plane_url`, `trust_domain` and exactly one of `token`/`token_payload` are required; capability advertisement `os`/`arch`/`toolchains`/`sandbox`/`network`/`region`/`cpu_cores`/`memory_mb`/`gpu`; `payload_dir` defaults to `<data_dir>/worker_payloads`; `discard_workspace_on_success = true`; `iterations` is bounded by the worker crate. |
