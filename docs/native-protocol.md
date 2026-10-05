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

The codegen classification of EVERY endpoint below (`generated` |
`handwritten-grandfathered` | `no-body` | `streaming-special-case`) and the
shrink-only handwritten budget live in
`crates/protocol/schema/CODEGEN.md`; `node scripts/protocol-codegen.mjs
--check` fails when a route is added without classification, when a
`generated` DTO leaves the canonical schema, or when the audited
handwritten set grows. The migrated attachment and task-run DTOs are
generated into both IDE clients (`ProtocolAttachmentId`,
`ProtocolAttachmentRef`, `ProtocolTaskRun*`).

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
  source, documentCapable, attachmentLimits}]`, walking every registered
  provider instance × its `known_models()` × `capabilities(model)`.
  `source` is the provenance string: `"liveProbe"` when the provider
  reports a live runtime context limit for the model (e.g. an Ollama
  `/api/ps` allocation), `"providerCatalog"` when the entry carries a
  non-default capability profile (configured or probed), else
  `"conservativeDefault"` (the fail-safe default profile).
  `documentCapable` is the model's document-delivery gate (the
  vision-like gate for `application/pdf` / `text/plain` attachments) and
  `attachmentLimits` is the daemon-advertised attachment admission
  contract of that exact provider/model, assembled by the ONE Rust source
  of truth (`faktor_provider::AttachmentLimits::for_model`):
  `{maxUploadBytes, maxRequestBytes, maxAttachmentBytes,
  image: {mimes: [{mime, maxBytes}], maxRequestBytes},
  document: {capable, mimes: [{mime, maxBytes}], maxRequestBytes}}`.
  Clients consume these values instead of mirroring daemon constants and
  keep only a conservative emergency ceiling for the window before the
  catalog is read.
- `GET /capabilities` — introspection map for capability-driven UI:
  `{ "<provider>": { models: [{id, capabilities, documentCapable,
  attachmentLimits}], runtimeContextLimitSupported: bool } }` (same
  registry walk; the boolean is true when any known model of the provider
  reports a live runtime limit).
- `GET /native/health` / `GET /native/ready` — see "Liveness and
  readiness" above.
- `POST /native/session` — create one durable session:

The create-session contract validates BOTH the provider and the model against the daemon's registered, priced catalog: an id the router cannot serve is a typed `not_found` refusal BEFORE any workspace/session row exists. A served session model may still be replaced by Economy routing (a priced equivalence among served candidates); the substitution is durable and loud, never silent.
  `{provider, model, workspace?, title?}` (strict DTO; `workspace`
  defaults to the daemon's own directory) → `{id, title, created_ms}`.
- `GET /native/sessions` — the durable session listing (newest first,
  capped at 1000): `{sessions: [{id, title, provider, model, state}]}`.
- `POST /native/session/{id}/prompt` — run ONE ordinary prompt through the
  daemon's single executor entry: `{session_id, submission_id, prompt,
  files?}` (the body id must match the path) → `{op_id, run_id, accepted,
  queued}`. `submission_id` is REQUIRED: the client submission UUID of this
  logical prompt, 1..=64 ASCII `[0-9a-f-]` (UUID-shaped, lowercase hex; the
  SAME contract as the task-start field); any other shape is a typed 400.
  It is the durable idempotency key (finding 1): the handler claims a
  `prompt_admission` row BEFORE the `PromptReceived` journal append and any
  queue/message mutation, so
  - a repeated key with the SAME normalized body (session id, prompt text
    and file list) returns the original receipt byte-for-byte and performs
    NO further mutation — no journal append, no user message, no queue row;
  - a repeated key with a DIFFERENT body is a typed `conflict` (409)
    naming the stored digest mismatch;
  - a concurrent duplicate while the first prompt is still pending is a
    typed `conflict` (409) in-flight; a prompt refused before acceptance
    releases its key so the same submission may be retried, while an
    ambiguous failure keeps it pending (a retry answers in flight, never a
    duplicate).
  The prompt admission table is SEPARATE from `task_admission`: a prompt
  key never aliases a task start. Empty prompts are a typed 400; unknown
  sessions 404.
- `POST /native/session/{id}/task-runs` — start ONE task through the same
  executor: strict body `{goal, submission_id, criteria?, work_items?,
  model?, max_tokens?, max_cost_micro?, files?, attachments?,
  completion_contract?, mutation_mode?, ownership?, routing_mode?}`
  (unknown fields refused; absent `work_items` = one mutating `main` item).
  `submission_id` is REQUIRED: the client submission UUID of this logical
  start, 1..=64 ASCII `[0-9a-f-]` (UUID-shaped, lowercase hex); any other
  shape is a typed 400. It is the durable idempotency key (finding 1):
  the executor claims a `task_admission` row BEFORE any task mutation, so
  - a repeated key with the SAME normalized request (goal, files, the
    effective attachments/criteria, the completion contract and the run
    envelope) returns the original run receipt byte-for-byte and performs
    NO further mutation — no task-row re-goal, no completion-contract
    record, no budget adjustment, no shadow start, no prompt enqueue;
  - a repeated key with a DIFFERENT request is a typed `conflict` (409)
    naming the stored digest mismatch;
  - a concurrent duplicate while the first start is still pending is a
    retryable typed `conflict`; a start that fails before acceptance
    releases its key so the same submission may be retried.
  Response: `{task_id, run_id, state}` (the run projection served by the
  task-run list/state endpoints).
- `POST /native/session/{id}/attachments` — upload ONE durable typed
  attachment: `{mime, filename?, data_base64}` (strict DTO) →
  `{ref_id, digest, mime, filename, size}` (`ref_id` is the stable
  surrogate row id of THAT reference; the response is additive over the
  original `{digest, mime, filename, size}` shape). `data_base64` is the
  CANONICAL standard-alphabet base64 of the raw bytes: whitespace and every
  non-canonical form (bad length/padding, non-zero trailing bits, foreign
  alphabet) are typed 400s, and the decoder reads the wire bytes directly
  into one pre-sized bounded destination. The decoded bytes are bounded by
  the advertised `attachmentLimits.maxUploadBytes`; an EXACT-metadata
  re-upload dedupes to the same reference (same `ref_id`), while identical
  bytes under a different mime/filename are a DISTINCT reference with its
  own `ref_id` and metadata. IMAGE and DOCUMENT delivery is validated
  model-aware at task admission, never at upload: images against
  `vision` + the image MIME/byte contract, `application/pdf` /
  `text/plain` against the chosen model's `documentCapable` and document
  MIME/byte contract; a refusal keeps the durable bytes. Ordinary
  workspace source files ride the repository-context `files` path of a
  prompt/task start and are never uploaded blindly.
- `GET /native/session/{id}/attachments/ref/{ref_id}` and
  `.../ref/{ref_id}/bytes` — resolve ONE durable attachment REFERENCE by its
  CANONICAL decimal surrogate id (no leading zeros; ref ids start at 1, so
  `"0"` is a typed 400): the metadata is exactly
  THAT reference's (`ref_id` included), and the bytes are served with THAT
  reference's MIME. A blob may back several references with distinct
  metadata, so digest-addressed retrieval is never used for a specific
  reference. A non-canonical/non-decimal segment is a typed 400 that names
  the blob route; an unknown/foreign ref id is a typed 404.
- `GET /native/session/{id}/attachments/blob/{digest}` — resolve ONE blob's
  metadata by its 64-char hex digest: exactly one reference of this session
  => that reference; zero => a typed 404; several references => a typed 409
  whose message lists the candidate `ref_id`s (each `ref/{ref_id}` route then
  serves its own metadata and MIME). The ref and blob route grammars never
  share a segment: a 64-decimal-digit digest with leading zeros is a DIGEST
  on this route, never a ref id. First-party clients expose this read as
  `attachmentBlobReference` (VS Code `NativeClient`, JetBrains
  `NativeClient`), surfacing the 409 as a typed API error rather than
  guessing a reference.
- `GET /native/session/{id}/attachments/blob/{digest}/bytes` — the raw CAS
  bytes of one blob referenced by this session, served as
  `application/octet-stream` (a digest-only blob has no single reference
  MIME). The blob must be referenced at least once by the session; an
  unknown/unreferenced digest is a typed 404.
- Attachment byte retrieval is bounded by the HTTP attachment contract: both
  `.../bytes` routes serve at most `MAX_ATTACHMENT_UPLOAD_BYTES` (7 MiB
  decoded, `crates/server/src/native/attachment.rs`) BY DESIGN — the
  retrieval ceiling is the upload contract, not a suggestion, so HTTP
  retrieval is never tighter than HTTP admission. The core CAS/attachment
  ceiling (`MAX_ATTACHMENT_BYTES`, 32 MiB) stays for programmatic callers:
  an attachment larger than 7 MiB can be stored programmatically but is NOT
  retrievable over HTTP. Clients bound these responses by the same contract
  (VS Code `ATTACHMENT_RESPONSE_MAX_BYTES`, JetBrains
  `NativeClient.ATTACHMENT_RESPONSE_MAX_BYTES`) instead of their generic body
  caps, and refuse an over-bound response typedly.
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

### Control-plane surface (disabled by default)

With no `[cloud]` section every route below answers a typed 409
`cloud_disabled` and nothing else in the daemon changes. Every route rides
the daemon password AND, except the bootstrap, an
`x-faktor-control-token` control-plane principal; a principal is fixed to
one organization, and a path `{id}` naming a foreign organization is the
same 404 a nonexistent one answers (no existence leak). Mutating
control-plane routes require a bounded printable-ASCII idempotency key:
the same key + same request replays the recorded response; the same key +
a different request is a typed 409.

- `GET /native/identity`, `GET /native/orgs`, `POST /native/orgs` — the
  caller's identity/organization and the daemon-owner-only bootstrap that
  mints the first owner session token (header `Idempotency-Key`).
- `GET /native/orgs/{id}/members`, `POST /native/orgs/{id}/members` — the
  member page and the invite (header `Idempotency-Key`); the invite
  response presents the single-use invitation token exactly once.
- `POST /native/invitations/accept` — redeem ONE invitation as the
  authenticated control-plane user. Strict body:
  `{token, idempotency_key}` (unknown fields refused). The idempotency key
  rides the BODY because the client holding only the invitation token may
  not know an organization to key against; the SAME
  `validate_idempotency_key` contract (bounded printable ASCII) and
  same-key replay semantics as the header-keyed routes apply. The
  accepting user is the principal's subject and must match the invited
  email — a foreign organization's principal (or a service account) is a
  typed `permission_denied` (403) and creates no membership. The
  membership insert and the invitation-accepted update commit in ONE
  transaction with the idempotency claim:
  - a same-key retry replays the recorded membership byte-for-byte and
    never inserts a second membership;
  - the same token under a NEW key is a typed `conflict` (409;
    `already accepted` / `has expired` / `was revoked`), and an unknown or
    empty token is a typed `unauthorized` (401);
  - success: `{ok: true, membership: {id, organization, user, role,
    created_ms}}`.
- `GET /native/repositories` — one cursor page of the organization's
  synced repositories.
- `GET,POST /native/approvals` and `POST /native/approvals/{id}/decide` —
  the approval queue (request is header-`Idempotency-Key`-keyed; a
  decision is idempotency-keyed and replays its recorded outcome).

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
