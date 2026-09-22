# Faktor Native Protocol v1

The daemon's own HTTP surface (architecture spec §16). The Faktor-owned
IDE panels are the only clients, and this protocol is optimized around the
Faktor runtime. Foreign wire compatibility was retired by explicit owner
decision; nothing here pretends to be it.

"v1" is this document's label for the daemon's own contract, not a runtime
constant: the code defines no native protocol version constant (the only
version constants are `faktor-core`'s `VERSION` and `UX_BASELINE`). The
contract IS the route set and the strict DTOs below.

All endpoints require daemon auth (`Authorization: Bearer
<FAKTOR_SERVER_PASSWORD>` or `x-faktor-server-password`; the legacy
per-start token rides the same Bearer header). The pre-cutover `Basic`
compatibility form is gone — clients that still send it must migrate to
the Bearer claim (migration note in `crates/server/src/auth.rs`); there is
no downgrade path. JSON field names on the
native surface are camelCase unless noted. Request bodies of the native
surface are first-class strict DTOs (`deny_unknown_fields`; audit 56):
an unknown field — a misspelled option such as `hardBudegt` included —
is a loud 400, never silently ignored. They are strict *native* shapes of
this runtime, not frozen compatibility envelopes.

## Lifetimes

| Object | Lifetime | Identity | Notes |
|---|---|---|---|
| Session | Created → Open for days → Suspended/Closed | numeric `id` | Row + journal + queue survive crashes (§4–§6 of the architecture spec) |
| Turn | One logical turn per prompt admission; `active → completed/cancelled/failed` | `opId` + durable turn record | Exactly one `TurnCompleted` journal event per genuine end |
| Task | The durable structured task state of a session (`faktor-context` ledger): goal, steps, decisions, changed files | session-scoped JSON | Survives compaction; bounded by construction |
| Operation | Any async op (`OpMeta` envelope: operation_id, session_id, state, start_time, deadline, retry, cancellation, recovery) | `opId` | Tools and provider calls are sub-operations of a turn |
| Agent | A background agent session owned by the daemon (Agent Manager) | separate `sessionId` | Listed under `/session/{id}/agents` |

## Paging and cursors

- Message pages are **cursor-based**: `GET /session/{id}/messages?cursor=<seq>&limit=<n>`.
  The cursor is the exclusive lower message sequence of the page
  (`seq > cursor`), newest first; a page carries `nextCursor`
  (`null` = end of history) and `hasMore`. The client never derives
  ordering from offsets; inserts are append-only, so pages never shift.
- Event resumes are **journal-sequence cursors**: `after` is the last
  consumed event seq and replays `seq > after` (0 = from the beginning).
  The SSE `id:` field of every frame IS the event sequence, so a
  reconnect simply sends the last id received. Oversized/unknown cursors
  are clamped, never errors.
- Turn records, checkpoints, verifications and agent lists are bounded
  newest-first listings; paging over them uses the same `cursor/limit`
  convention where the store supports it, and full bounded listings
  otherwise.

## Money encoding

Monetary quantities are micro-unit integers (microUSD) held as `u64` in the
runtime; the served domain is bounded by `i64::MAX` for credit amounts (the
durable ledger column is a signed 64-bit `INTEGER`). On the wire a monetary
field is a **quoted decimal string**, never a JSON number:

- The rule is the field name: any key carrying the money token — snake_case
  `*_micro` (`granted_micro`, `consumed_micro`, `refunded_micro`,
  `held_micro`, `provider_cost_micro`, `managed_cost_micro`,
  `byok_cost_micro`, `managed_spend_micro`, `byok_spend_micro`,
  `amount_micro`, and the money-named plan limits
  `max_managed_spend_micro_per_period` / `min_credit_balance_micro`) or
  camelCase `*Micro` (`spentCostMicro`, `maxCostMicro`, `openReservedMicro`,
  `uncertainReservedMicro`, `predictedMicro`, `spentMicro`,
  `providerReportedMicro`, `settledCostMicro`) — is a decimal string in the
  `u64` range, e.g. `"granted_micro": "9223372036854775807"`. The rule holds
  at every depth, including money fields inside embedded durable JSON
  (routing decisions, tournament candidates).
- Clients MUST parse these with string-backed money or arbitrary-precision
  integers (TypeScript `BigInt`, Kotlin `java.math.BigInteger`), never with
  IEEE-754 `number` (integer-exact only to 2^53-1). A missing optional cap
  is `null`, never `0`.
- On INPUT the daemon accepts both the decimal string and a legacy JSON
  integer for every money field (compatibility with pre-encoding clients);
  malformed, negative, fractional and overflowing values are typed 400s.
- Every other numeric field stays a JSON number: ids, session/task/run ids,
  sequence numbers, cursors, counts, token counters, latencies and
  timestamps. No `micro` token in the name, no string on the wire.

## Liveness and readiness

- `GET /native/health` — liveness: 200 `{ok: true, version, worker_plane}`
  whenever the process responds (auth-gated like every route). "The process
  is up", nothing more; it exists so probes that must not flap on recovery
  do not have to distinguish readiness semantics. `worker_plane` is an
  **additive** object: `state` is always present — `disabled` when no
  dedicated worker-plane listener is wired, else `serving` / `unavailable` /
  `stopped`; `bind` appears when the bound address is known (including after
  shutdown); `code` + `message` appear only for `unavailable` (the typed
  error's stable code and its message, bounded to 512 bytes, never carrying
  the transport bearer). `ok` and `version` are unchanged, so a client that
  knows only those keys keeps working.
- `GET /native/ready` — readiness: 200 `{ready: true}` only when the
  session store has **recovered** (the flag is set at the very end of
  `serve()` setup — the caller opens the store, applies migrations at open
  and runs crash recovery *before* serve, so the end of setup is exactly
  the recovered moment), the required runtime components exist (the
  `SessionManager` is a non-optional `Arc` in `ServerDeps`, so its
  presence is structural), and migrations are applied (implicit at store
  open). Before that moment the endpoint answers 503 `{ready: false}`;
  because the flag flips before `serve()` returns its handle, a client
  that holds a live handle observes only the ready state — the not-ready
  window is a startup property (`ServerDeps.simulate_not_ready` keeps it
  observable in tests). Liveness ≠ readiness: a daemon whose store failed
  recovery is alive but never ready.

## Endpoint list (v1)

Implemented (this revision of the daemon):

- `GET /session/{id}/projection` — one JSON snapshot of the session's
  state for UI badges/polling:
  `{ session: {id,title,provider,model,lifecycle}, state:
  {machine,label,active,terminal}, activeModel?, activeTool?, progress?,
  filesChanged: [...], lastCheckpoint?, verification: [...],
  contextUsage?, prefixStability?, queued: n }`. `activeModel` = the
  effective provider/model envelope of the current or most recent logical
  turn (durable turn record; `null` before the first turn). `activeTool` =
  the newest still-running durable tool-run row. `filesChanged` = the
  durable task ledger's changed files. `lastCheckpoint` = newest
  checkpoint row when the daemon runs with a checkpoint service wired,
  else `null`. `verification` = pending tool runs whose effects are
  `unknown` (recovery `mark_unknown`), capped. `progress` is the session's
  live bounded progress record (null before the runtime tracked one);
  `contextUsage` stays `null` in this revision (no durable context-usage
  read API yet); `prefixStability` is populated from the durable per-call
  prefix observations once a turn has settled one (null before). The
  machine state, activeTool and provider-call journal are the source of
  phase information.
- `GET /models` — the flat daemon model catalog:
  `[{provider, model, context, maxOutput, tools, parallelTools,
  reasoning, thinking, vision, structuredOutput, embeddings, streaming,
  source}]`, walking every registered provider instance × its
  `known_models()` × `capabilities(model)`. `source` is the provenance
  string: `"liveProbe"` when the provider reports a live runtime context
  limit for the model (e.g. an Ollama `/api/ps` allocation),
  `"providerCatalog"` when the entry carries a non-default capability
  profile (configured or probed), else `"conservativeDefault"` (the
  fail-safe default profile).
- `GET /capabilities` — introspection map for capability-driven UI:
  `{ "<provider>": { models: [{id, capabilities}],
  runtimeContextLimitSupported: bool } }` (same registry walk; the
  boolean is true when any known model of the provider reports a live
  runtime limit).
- `GET /native/health` / `GET /native/ready` — see "Liveness and
  readiness" above.
- `POST /native/session` — create one durable session:
  `{provider, model, workspace?, title?}` (strict DTO; `workspace`
  defaults to the daemon's own directory) → `{id, title, created_ms}`.
- `GET /native/sessions` — the durable session listing (newest first,
  capped at 1000): `{sessions: [{id, title, provider, model, state}]}`.
- `POST /native/session/{id}/prompt` — run ONE ordinary prompt through the
  daemon's single executor entry: `{session_id, prompt, files?}` (the body
  id must match the path) → `{op_id, run_id, accepted, queued}`. Empty
  prompts are a typed 400; unknown sessions 404.
- `GET /native/session/{id}/events?after=<seq>` — the durable journal SSE
  stream, cursor-resumable: frames are `id: <seq>`, `event: <kind>`,
  `data: <native_event_row>` (the exact shape of the `/native/events`
  page rows); `event: heartbeat` keep-alives carry no id. Catch-up is
  paged (bounded), so a reconnect against a huge journal never balloons
  RAM and resumes exactly from the cursor.
- `GET /native/usage` — cross-session aggregate with two documented
  layers. The legacy view keeps its frozen shape over the memory facts of
  kind `usage` (keys `budget`/`spent`): `{sessions, totals: {budget,
  spent}, perSession: [{sessionId, budget, spent}]}`; no runtime path
  writes those facts (they exist for simulators/tooling), so when nothing
  recorded them the totals are honest zeros and the list is empty. The
  `durable` layer is the AUTHORITATIVE aggregate over persisted rows:
  provider-call tokens plus prefix observations, task spend, and the
  cost-reservation groups (`reserved`/`dispatched`, `settled`, `refunded`,
  `uncertain`); a scan hitting its cap says `truncated: true` instead of
  pretending to be exact. With `?org=` the route serves the org-isolated
  Wave 3 billing fold (`since` and `limit` are its strict query
  parameters; unknown query keys are a 400).
- `GET /native/session/{id}/turns` — the session's durable turn records,
  newest first: `[{opId, status, provider, model, variant?, toolMode?,
  startedAt, updatedMs, queueSeq?, promptMessageId?}]` (`status` =
  `active|completed|cancelled|failed`). One record per admitted logical
  turn; empty before the first turn.
- `GET /native/session/{id}/tasks` — the durable task ledger as typed
  JSON: `[{goal, constraints, state, milestones: {completed, open},
  decisions, failures, changedFiles, tests: {run, failed}, preferences,
  verification}]`. One entry while a task is tracked, `[]` before any
  task data exists (a stored-but-empty ledger is not a task). `state` is
  derived: `running` while the turn machine is active, `in_progress`
  with open milestones (or a fresh goal with nothing completed yet),
  `done` once completed work exists with nothing left open, else `idle`.
  `verification` repeats the session's durable verification facts (same
  source as `/verification` below).
- `GET /native/session/{id}/checkpoints` — the session's durable
  checkpoint rows, newest first:
  `[{sequence, path, beforeHash, afterHash, beforeExists, afterExists,
  createdMs, restoredMs?}]`. Empty when the daemon runs without a
  checkpoint service wired or nothing was recorded yet.
- `GET /native/session/{id}/verification` — everything the session owes
  verification: `{owed: [{opId, tool, startedMs, status, effectStatus}],
  failedChecks: [{id, detail, status}]}`. `owed` = still-open durable
  tool runs whose recovery strategy is `mark_unknown` (unknown external
  effects are forced to verification — spec §7); `failedChecks` = the
  durable memory facts of kind `verification` (one per failed REQUIRED
  check, recorded at genuine turn ends; `detail` carries
  `failed:<command>`). Bounded; empty arrays when nothing is owed.
- `GET /native/session/{id}/agents` — the session's real agent listing
  (path-id form of `/native/agents?session=`): every orchestrated run's
  children plus the parent's own durable task runs, projected from the
  `orchestrator_*` and task-run rows. Empty ONLY when the session
  genuinely has no task run.
- `GET /native/session/{id}/terminal` — the session's terminal view over
  the durable session-owned `terminal_*` ledger rows:
  `[{id, pid, alive, sessionId, taskId, agentId, operationId, spawnedMs,
  state, ptyId?}]` (`ptyId` only when a live PTY handle of this boot owns
  the row). The path session id is still validated (unknown → 404). The
  scoped control routes are `GET /native/terminals?session=`,
  `GET /native/session/{id}/terminal/events`, output snapshots at
  `GET /native/session/{id}/terminals/{terminal_id}/output`, spawn at
  `POST /native/session/{id}/terminal`, and
  `input`/`resize`/`kill`/`reconcile` posts under the same terminal path.
- `POST /native/session/{id}/abort` — the native abort
  (`sdk_abort` semantics behind the strict DTO): body
  `{"session_id": <id>, "op_id": <opId>?}` (`op_id` targets one queued
  prompt or the active turn; absent = abort everything of the session).
  The body `session_id` must equal the path id. Unknown fields/typos in
  the body are 400; unknown sessions 404; a queued-prompt kill cancels
  its durable row without touching the state machine. Response
  `{aborted: [<opId>...]}`.

Wired under the daemon-level `/native` prefix (strict DTOs; the daemon
serves these daemon-level forms only — there are no exact
`/session/{id}/...` aliases for them):

- `GET /native/messages?session=<id>&before=<seq>&limit=<n>` — cursor-based
  message page (`seq < before`, newest first, `hasMore`/`nextBefore`, page
  cap 200).
- `GET /native/events?session=<id>&after=<seq>&limit=<n>` — journal event
  page with `seq > after` resume cursors (SSE `id:` = event seq; see §11.3
  of the architecture spec; page cap 256).
- `GET /native/providers` — the registry view of every registered provider:
  `[{instanceId, family, models: [{model, context, maxOutput, tools,
  parallelTools, reasoning, thinking, vision, structuredOutput,
  embeddings, streaming, source}], runtimeContextLimitSupported, health}]`.
  Auth/endpoint metadata stays in the provider layer and is never emitted.

## UI-adaptation principle

A UI is a UI, not a protocol peer. Adaptation happens at the boundary, in
one direction only:

1. **UI posts typed messages** — text, tool parts, file references — never
   foreign control envelopes. The Faktor-owned IDE panels (`apps/vscode`,
   `apps/jetbrains`) translate UI gestures into typed native requests.
2. **Client → Rust**: the client is a thin native-protocol client; all
   state lives in the daemon, all validation happens in the daemon.
3. **Never pretend to be a foreign protocol**: the pre-cutover
   wire-compatibility surface was retired; native endpoints own their
   shapes and strictness (unknown fields are loud 400s), and a native
   client never fabricates foreign frames.
