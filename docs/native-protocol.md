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
| Agent | A background agent session owned by the daemon (Agent Manager) | separate `sessionId` | Listed by `GET /native/agents?session=<id>` |

## Paging and cursors

- Message pages are **cursor-based**: `GET /native/messages?session=<id>&before=<seq>&limit=<n>`.
  `before` is the exclusive upper message sequence of the page (walking
  OLDER; omit it for the newest page), newest first; a page carries
  `nextBefore` (`null` = no older page) and `hasMore`. The client never
  derives ordering from offsets; inserts are append-only, so pages never
  shift. There is no `/session/{id}/messages` alias.
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
  `contextUsage` is the newest authoritative context manifest's checked
  usage or `null` (see "Long-task context, compaction, memory and
  instructions" below; null is the S62 `Unavailable` state, never 0); `prefixStability` is populated from the durable per-call
  prefix observations once a turn has settled one (null before). The
  machine state, activeTool and provider-call journal are the source of
  phase information. Rev8 adds the `compaction` and `longTask` fields
  documented in that section.
- `GET /models` — the flat daemon model catalog:
  `[{provider, model, context, maxOutput, tools, parallelTools,
  reasoning, thinking, vision, structuredOutput, embeddings, streaming,
  source, capabilityAuthority, capabilitiesGated, documentCapable,
  attachmentLimits}]`, walking every registered provider instance × its
  `known_models()` × `capabilities(model)`.
  `source` is the provenance string: `"liveProbe"` when the provider
  reports a live runtime context limit for the model (e.g. an Ollama
  `/api/ps` allocation), `"providerCatalog"` when the entry carries a
  non-default capability profile (configured or probed), else
  `"conservativeDefault"` (the fail-safe default profile).
  `capabilityAuthority` is the AUTHORITY of the capability lookup —
  `"known"` (model-specific: probed for this exact id or declared per
  exact id), `"declared_generic"` (an endpoint-declared generic profile:
  config `models = [...]`, a `*` wildcard, or a documented adapter family
  default — usable as declared) or `"unknown"` (no probe, no declaration:
  the unverified fallback). `capabilitiesGated` is true exactly when the
  runtime would gate that Unknown profile (tools/vision/JSON
  schema/reasoning forced off, context clamped to the conservative
  default, see `docs/architecture.md` §9.2.1) — clients must render this,
  never hide it: an unknown model is never silently "known".
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
  `{ "<provider>": { models: [{id, capabilities, capabilityAuthority,
  documentCapable, attachmentLimits}], runtimeContextLimitSupported: bool
  } }` (same registry walk; the boolean is true when any known model of
  the provider reports a live runtime limit).
- `GET /native/health` / `GET /native/ready` — see "Liveness and
  readiness" above.
- `POST /native/session` — create one durable session:

The create-session contract validates BOTH the provider and the model against the daemon's registered, priced catalog: an id the router cannot serve is a typed `not_found` refusal BEFORE any workspace/session row exists. A served session model may still be replaced by Economy routing (a priced equivalence among served candidates); the substitution is durable and loud, never silent.
  `{provider, model, workspace?, title?}` (strict DTO; `workspace`
  defaults to the daemon's own directory) → `{id, title, created_ms}`.
- `GET /native/sessions` — the durable session listing (newest durable
  `listing_revision` first, capped at 1000):
  `{sessions: [{id, title, provider, model, state, created_ms, updated_ms,
  listing_revision}], hasMore, nextBefore: {beforeRevision, beforeId,
  beforeMs} | null, snapshotRevision, snapshotMs}`. Pages are bound to
  `snapshotRevision` (returned by page 1 and passed back with
  `beforeRevision` + `beforeId`); `snapshotMs`/`beforeMs`/`updated_ms` are
  wall-clock presentation metadata only, so a same-millisecond update can
  never move a conversation across a page cursor.
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
  paged (≤ 256 events per poll), so a reconnect against a huge journal
  never balloons RAM and resumes exactly from the cursor. Frames are never
  merged or renumbered; up to 64 frames are batched per HTTP chunk
  (flushed at 64 KiB, a frame is never split), live delta-only tails
  linger ≤ 8 ms for that batch, and every
  non-delta event flushes immediately. An append wakes the stream through
  the session's journal generation (250 ms fallback poll); heartbeats are
  emitted once after the tail drains and then at most every 5 s.
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
  parameters; unknown query keys are a 400). With a `[billing]` authority
  wired the org-less legacy alias is refused 400 `org_required`: it would
  serve whole-daemon data without the tenant scope. Without billing it
  keeps its frozen shape byte-identically.
- `GET /native/usage/export` — a BOUNDED download of the same
  organization-scoped usage ledger `/native/usage?org=` folds. Strict query
  `org` (required), `since` (exclusive cursor), `limit` (1..=5000, default
  5000) and `format` (`csv` | `json`, default `json`); unknown keys are a
  400. It runs the same read-through ingestion first, then pages at most
  5000 rows / 2 MiB and answers as an attachment
  (`content-disposition: attachment`); a larger result is a typed 413
  `export_too_large` that names the bound, never a silently truncated
  download (`truncated` names a limit-bound page). CSV neutralizes
  spreadsheet formula prefixes (`=`, `+`, `-`, `@`, tab, CR) with a leading
  quote and quotes RFC 4180 fields. The route is `BillingRead`
  (viewer+), tenant-scoped: a foreign `org` is the same typed 404 as a
  missing one, and no `[billing]` authority is the shared 409
  `billing_disabled`.
- `GET /native/billing/overview` — the durable commercial account of the
  caller organization: `account` (id/name/managed/disabled/created_ms),
  `plan_id`/`plan_found`, `subscription` (id/plan/status/started_ms/
  expires_ms/updated_ms), `subscription_active` (derived at read time),
  derived `trial` (`state` = `none` | `active` | `expired`, `ends_ms`), and
  the typed availability of every capability this build does not
  implement: `payment`, `invoices`, `seats`, `tax` and `currency` are
  `unavailable` with stable causes (`payment_provider_not_configured`,
  `invoices_not_available`, `seat_billing_not_modeled`,
  `tax_not_configured`, `currency_not_configured`); `usage_export` is
  `available` with formats `[csv, json]`. It NEVER folds usage and NEVER
  infers a paid state from a local plan row. `BillingRead` (viewer+).
- `POST /native/billing/subscription` — the plan-change / cancellation
  EFFECT (OWNER only, `BillingWrite`), durable and immediate at the next
  admission boundary (in-flight transactions continue). Strict body:
  `{"action":"change","plan_id":...,"status"?: "active"|"trialing"|
  "expired","expires_ms"?: ...}` or
  `{"action":"cancel","confirm":true}`; `cancel` carries no plan/status/
  expiry, `change` cannot set `canceled`, a `trialing` row REQUIRES
  `expires_ms` (an open-ended trial is not a trial), and an undefined plan
  is a typed 400. `Idempotency-Key` is required and durable/request-bound:
  the same canonical request replays the recorded response (`replayed:
  true`), a different request under the same key is a 409
  `idempotency_conflict` with nothing written. The response carries the new
  `subscription`, the new `billing` overview, and `proration` — which is
  `unavailable` with cause `plan_prices_not_configured` because the plan
  table carries no price; no money delta is invented. A change to the same
  durable row reports `replayed: true` without bumping `updated_ms`.
- `GET /native/outcomes` — the Engineering Outcome Ledger (economic audit
  item 7): the durable completed-run metrics aggregate plus one bounded
  newest-first page of run records. `metrics` carries the first-class
  metrics (`successfulRunCostMicro` — spend on successful runs / successes;
  `verifiedCostPerSuccessfulChangeMicro` — TOTAL eligible-cohort spend over
  successful verified changes, so 9 failures at $10 plus 1 success at $1
  reads $91/change; both over the MONEY-COMPLETE cohort only, with
  `moneyCompleteRuns`/`moneyPendingRuns`/`moneyUnavailableRuns`,
  `moneyIncompleteExcluded` and `verifiedChangesMoneyComplete` naming the
  cohort and the exclusion count;
  `verifiedFirstVerificationPassPpm` (Verified First Verification Pass
  Rate: verified changes whose FIRST verification
  attempt passed — a verification-attempt measure, never an
  implementation-attempt claim), `reworkCostRatioPpm`, the local
  split `localOnlyVerifiedPpm` / `localFinalVerifiedPpm` /
  `localFirstSuccessPpm` / `localToHostedFallbackPpm` / `hostedOnlyPpm`
  with their counts (every local/hosted split is decided by each step's
  durable `executionClass`: `unknown` counts as neither local nor hosted;
  the first/final/fallback splits classify from the run's exactly-preserved
  boundary classes, `firstExecutionClass` / `finalExecutionClass`, so a
  folded display sequence never depresses them, and
  `localClassedRuns` / `localMeasurement` = `measured` | `partial` |
  `unmeasured` report the boundary-class coverage — mirroring the frontier
  rule),
  `attributionCompleteRuns` / `attributionPartialRuns` /
  `attributionUnavailableRuns` and the routing-learning exclusion count
  `attributionIncompleteExcluded` (a non-complete provider/model
  attribution is recorded but never fed the ledger's atomic learning feed;
  its samples kept the per-call feed),
  `avoidedHostedInferenceMicro` (null with
  `avoidedHostedInferenceMeasurement = "unavailable"` unless a run carried
  a MEASURED counterfactual; zero is only a measured zero),
  `meanTimeToVerifiedChangeMs`, `humanInterventionPpm`,
  `regressionRollbackPpm`, `proofCoveragePpm`, `frontierEscalationPpm` —
  `null` (never a fabricated 0%) unless durable execution-class authority
  exists, with `frontierEscalationMeasurement` = `measured` | `partial` |
  `unmeasured`)
  plus their exact totals and the fold's scan
  honesty (`scanned`/`truncated`); `runs` is a page of the ledger rows
  with the immutable engineering record plus the LATEST settlement
  projection (`moneyState`; money fields are decimal strings only when
  `moneyState == "complete"` and null otherwise — a pending/unavailable
  money fact is never a zero; per-step `spendMicro` follows the same rule;
  every `modelSequence` step carries its durable `executionClass`
  (`local` | `hosted_economy` | `hosted_balanced` | `frontier` |
  `unknown`, with `local` true only for `local` — the wire `local` is
  CLASS-DERIVED, so a pre-class row that recorded a local execution keeps
  `local: false` and exposes its recorded bit as `legacyLocal: true`
  instead of promoting it to a class claim);
  `firstExecutionClass` / `finalExecutionClass` are the run's
  exactly-preserved boundary classes (the classification authority,
  independent of the bounded sequence folding; a pre-field payload falls
  back to the preserved sequence boundary) and `attributionState`
  (`complete` | `partial` | `unavailable`) is the provider/model
  attribution honesty of the recorded sequence;
  `avoidedHostedMicro` is null with
  `avoidedHostedMeasurement = "unavailable"` when no counterfactual
  authority exists); `nextCursor` is the composite
  `{completedAtMs, runId}` continuation. Query (strict DTO): `limit`
  (1..=200, oversized rejected), `before_ms` + `before_run` given
  together; `org=<organization id>` requests the paid ORGANIZATION-scope
  read (`outcome_analytics`): the caller's control-plane principal must
  reach that organization (`BillingRead`, viewer+) and the organization's
  plan must map the entitlement — a denial is the typed 403
  `value_entitlement_refused` naming the entitlement, its `hook`
  (`outcome_ledger`), the `cause` and the resolved plan; with no
  `[billing]` authority wired the org-scope read answers the shared typed
  409 `billing_disabled`. The org-less read never consults the value gate:
  without a billing authority it keeps the frozen local shape, and with
  billing wired it is refused 400 `org_required` before any fold (the alias
  would bypass the tenant scope). Money fields are decimal strings
  (`*Micro`); rates are integer ppm. A corrupt or
  payload/projection-drifted ledger row is a typed 500, never a fabricated
  metric. See `docs/outcome-ledger.md`.
- `GET /native/value-entitlements` — the caller organization's DERIVED
  value-centric entitlement report: the plan-mapped paid value features
  that are independent of inference volume. The response's
  `value_entitlements` carries `organization_id`, `plan_id`, `plan_found`,
  `subscription_active`, `plan_entitlements` (the resolved plan's value
  feature tags: `advanced_verification_policy`, `organization_contracts`,
  `shared_intelligence`, `cross_repo_intelligence`, `outcome_analytics`,
  `signed_proof_provenance`, `audit_retention_history`,
  `model_optimization_learning`, `private_runners`,
  `enterprise_governance`), `governance` (`{sso, scim}` capability flags,
  effective only behind `enterprise_governance`; vocabulary only — no
  protocol implementation rides this route) and `decisions` — one entry per
  value entitlement naming its `hook` (`verification_policy`,
  `contract_compiler`, `intelligence_federation`, `cross_repo_index`,
  `outcome_ledger`, `proof_attestation`, `audit_history`,
  `outcome_learning`, `worker_placement`, `identity_federation`), whether
  it is `granted` now and, when not, the typed `cause` (`plan_missing`,
  `subscription_inactive`, `not_in_plan`). The route NEVER folds usage,
  credits or tokens: a 100% local organization with zero provider attempts
  resolves the same picture as a metered one (the offline default — no
  `[billing]` section — answers the shared typed 409 `billing_disabled`
  while the local product stays fully functional). Control-plane principal
  required (`BillingRead`, viewer and above).

  The report is descriptive: the DECISION is cloud-owned and ENFORCEMENT
  lives at the runtime capability boundaries, one seam per `hook`, each
  consulted before the paid side effect and refusing typed — never a
  silent empty result. `verification_policy` = the advanced
  verification-policy constructor (`[verification] advanced = true`; a
  denied plan refuses the daemon build typed and the default local policy
  is never gated); `contract_compiler` = consulting the organization's
  system-intelligence provider at drive-start contract compilation
  (denied: the provider is NOT consulted, the native contract stands
  byte-identically and the typed refusal is logged); `intelligence_federation`
  / `cross_repo_index` = the federated index query (one workspace =
  shared intelligence, several workspaces = cross-repo intelligence);
  `outcome_ledger` = `GET /native/outcomes?org=...`; `proof_attestation`
  = the signed proof/provenance attestation entry (entitlement checked
  before validation and before the injected signer runs); `audit_history`
  = `GET /native/enterprise/audit?extended=true` (the extended historical
  page; base reads are never gated); `outcome_learning` = the shared
  organizational learning feed; `worker_placement` = private-runner
  placement (checked before any job generation is minted; the 403
  `value_entitlement_refused` refusal is typed through the executor) and
  worker claim admission (the same 403 on the worker claim route) —
  gating is enforced at these daemon/service seams, not by the library
  API contract: the `faktor-worker` library's `WorkerPlane::new`/
  `new_for_embedded_host` construction is deliberately ungated for
  embedded hosts and tests, and the daemon's serve wiring always installs
  the `private_runners` claim gate on its ONE plane
  (`WorkerPlane::set_claim_entitlement_gate`);
  `identity_federation` = the SSO entry
  (`POST /native/sso/start` and `/callback`) behind the
  `enterprise_governance` entitlement AND the capability flag (the flag
  gates the entry, not an implementation). With no `[billing]` authority
  every paid augmentation is refused typed (`cloud_absent` on runtime
  seams, 409 `billing_disabled` on HTTP surfaces) while community/local
  behavior is byte-identical.
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
  `GET /native/session/{id}/terminal/events` (durable-seq keyset pages:
  `after` is strictly exclusive, ≤ 200 rows per page, `hasMore`/
  `nextCursor`; a cursor never re-scans the whole ledger), output snapshots at
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

### Long-task context, compaction, memory and instructions (Rev8)

These additive fields/routes back S08/S10/S11/S12/S62/S66/S69. Field names
are exactly as served (the embedded `faktor-foundation` shapes are
snake_case; the long-task wrappers are camelCase).

**Projection overlay** — `GET /session/{id}/projection` adds:

- `contextUsage`: `null` or the newest captured manifest's
  `{revision, call_id, model, capacity, input, output_reserve,
  safety_reserve, headroom, basis: {kind: "estimated"|"observed"|
  "unavailable", method?, version?, receipt?, reason?}, estimated,
  measured, categories: [{category, tokens}], sources: [...],
  overflow, overflow_reason, created_ms}`. `input`/`headroom` are `null`
  when the arithmetic cannot be trusted. `null` contextUsage is S62:
  render `Unavailable`, never a percentage. Draft estimates come only
  from `POST /native/session/{id}/context/preview` (a separate basis).
- `compaction`: newest durable compaction operation (same DTO as the
  status route) or `null`.
- `longTask`: `{executionPhase, autoCompaction: {trigger, startedMs,
  ageMs, stale}|null, compacting}`. `compacting` is true for a live manual
  operation or an un-stale threshold marker; a stale crash marker is
  never live progress.

**Compaction operations (S10/K-03/K-04)**

- `POST /native/session/{id}/context/compaction` — body
  `{operationId?, targetTokens?, minReductionRatio?, expectedRevision?,
  maxHistoryMessages?}` (strict; unknown fields 400). The POST is the
  executor: it returns the terminal state. To render `planning|reducing|
  committing` progress, issue it without awaiting the response and poll
  the status route (or projection) from another request; cancellation is
  a concurrent request against the same operation id.
- `GET /native/session/{id}/context/compaction` — newest operation.
- `GET /native/session/{id}/context/compaction/{operation}` — exact state.
- `POST /native/session/{id}/context/compaction/{operation}/cancel` —
  `{cancelRequested, signaled, operation}`; the durable state is the
  authority and a late cancel never rewrites a terminal outcome.

Operation DTO: `{operationId, state, phase, terminal, progress (0..1),
expectedRevision, manifestRevision, before, after, target,
minimumReduction, strategy, result, error: {code, message}|null,
createdMs, updatedMs, basis: {kind, method, version}}`.
`state` is one of `prepared | planning | reducing | committing |
cancelling | applied | refused | cancelled | failed | effect_unknown`.
`effect_unknown` is terminal and means the daemon stopped during the
atomic commit; it is never reported as applied/failed. A non-terminal
operation from a previous daemon is reconciled on read: pre-commit phases
become `cancelled` (prior basis remains), `committing` becomes
`effect_unknown`. `progress` is durable, not a fabricated animation.

`result` on a terminal operation carries
`{accepted, before, after, target, reduction, minimumReduction, strategy,
summaryRevision, summaryVersion, summaryDigest, checkpointThroughSeq,
rejectedReason, retainedFacts: {goal, constraints, openSteps, decisions,
knownFailures, changedFiles}, keptRecentTurns, removed: {archivedChunks,
archivedRefs, evidenceRefs, originalHistory: {kind, sessionId,
throughSeq, note}}, basis}`. `before`/`after` are exact history-estimator
wire tokens (the same unit the next request carries); an accepted
operation must meet `after <= target`, `reduction >= minimumReduction`,
and keep the newest user instruction plus the verbatim task identity.
Refusal/cancellation writes NO checkpoint. Duplicate `operationId` with
the same inputs replays the same durable row; different inputs are 409.
A stale `expectedRevision` is 409 `context_revision_conflict`.

**Memory records (S12/S66/K-10/K-11)**

- `GET /native/memory/records?session=&scope=session|workspace|project|all&
  includeDeleted=&afterMs=&afterKind=&afterKey=&limit=` — bounded page
  (`limit` 1..=200) of `{id, scope, scopeId, subject, predicate, value,
  provenance: {origin, recordedMs, assertedBy, sourceRef}, confidencePpm,
  version, createdMs, updatedMs, deleted: {deletedMs, reason,
  atRevision}|null, previous: {version, value, updatedMs}|null}`.
  `scope=all` merges this session's session+workspace scopes only.
  Workspace/project scope is SHARED by every session of that workspace
  (separate durable table); session scope is private to the owning
  session. Both are structurally isolated across workspaces.
- `POST /native/memory/records` — `{session, scope, subject, predicate,
  value: {type: "text"|"bool"|"number"|"path_list"|"json", value},
  provenance?, confidencePpm?, evidence?, sourceRevision?}`. Identity is
  `(scope, subject, predicate)`: an identical live value is a no-op
  (`deduped: true`, no version bump), a different live value is a typed
  409 conflict requiring an explicit update.
- `PUT /native/memory/records/{id}` — `{session, expectedVersion, value}`;
  stale versions are 409 `memory_version_conflict`.
- `DELETE /native/memory/records/{id}?session=&expectedVersion=&reason=`
  — tombstone + S66 receipt `{id, scope, scopeId,
  futureUseStoppedAtRevision, deletedValueHash, retainedPriorCopies:
  true, retainedPriorArtifactRefs, retentionNote, record}`. Future
  retrieval stops; earlier prompts/artifacts/logs are NOT claimed erased.
- `POST /native/memory/records/{id}/restore` — `{session,
  expectedVersion}` (live records are `memory_not_deleted`).
- `GET|PUT /native/memory/policy` — workspace memory policy with an
  explicit `revision` CAS (`auto_capture`, `retention_days`,
  `max_records`, `allow_session_scope`, `allow_workspace_scope`);
  `max_records` is enforced on new records (dedupe/update/restore still
  pass); failure learning is a separate authority and never
  reinterpreted.
- `POST /native/memory/records/retention` — `{session, scope?,
  olderThanDays?, dryRun?, limit?}`; a dry run lists exactly the
  tombstone rows a real bounded pass deletes (live records are never
  selected).

Typed errors: `memory_not_found` (404), `memory_conflict`,
`memory_version_conflict`, `memory_deleted`, `memory_not_deleted`,
`memory_policy_limit` (409),
`memory_scope_invalid`, `memory_malformed` (400), `memory_oversized`
(413), `memory_store_unavailable` (500). Provenance origin is one of
`user | tool | verification | model | import | recovery`. Scopes resolve
from the session row, so a project cannot read another workspace's
records.

**Instructions and precedence (S11/S69/I-11)**

- `GET /native/instructions?session=&prompt=&touched=` — loaded sources
  grouped by origin/scope with `{source, path, scope, priority, hash,
  bytes, active, reasonLoaded, contentPreview}`, `activeCount`,
  `skips`, `precedence`, `conflicts: [{scope, winner, loser, reason}]`,
  `managedAuthority`, `epoch`, `bundleHash`, and `binding`.
- `GET /native/instructions/preview?session=&prompt=&touched=` — bounded
  merged bundle (`text`, `bytes`, `truncated`, `sources`, `binding`).
- `PUT /native/instructions/source` — `{session, path, content, mode:
  "create"|"update", expectedHash?}`; one atomic bounded edit. Update
  requires the exact content hash read; create requires an absent path.
  An edit never rewrites a running turn's captured bundle.
- `GET|POST /native/instructions/records`, `GET|DELETE
  /native/instructions/records/{id}`, `POST
  /native/instructions/records/preview`, `POST
  /native/instructions/import`, `GET|POST
  /native/instructions/bindings` — the durable rule records, previews,
  import migration report and captured-bundle bindings.

`binding` is `{capturedEpoch, capturedBundleHash, capturedAtMs,
capturedTurnOpId, currentEpoch, currentBundleHash, matches, activeRun,
applyTiming: "next_task"|"restart_required", restartRequired}`.
Precedence is served highest-first; a managed lock is never relaxed by
project text, and repository instructions remain DATA, not executable
authority.

### Configuration descriptors and runtime truth (settings)

`GET /native/config/descriptors`, `GET /native/config/snapshot`,
`GET /native/config/search`, `GET /native/config/provenance`, `POST
/native/config/preview` and `POST /native/config/commit` are the settings
surface. Every descriptor carries `runtimeConsumer`:

- `{"state":"applied","consumer":"<module>"}` — a production path reads the
  key and the descriptor is writable;
- `{"state":"stored_only","reason":"..."}` — the value is durable/exported
  for compatibility but this build has NO runtime consumer. The descriptor
  is `writable:false` and a commit naming it is a typed 409 `read_only`
  whose `fieldErrors[].message` names the reason. A settings screen must
  render this as unavailable, never as "saved";
- `{"state":"unknown"}` — an unclassified descriptor (fail closed;
  read-only), and the wiring-matrix gate fails.

`applyTiming` is honest: `effective`/`next_call`/`next_task` values are
read by the request/task path, `restart_required` values are overlaid onto
the daemon config at boot (retry, efficiency, compaction model, embedding
provider/model and sandbox shell), and a key that cannot be applied is
stored-only rather than advertised as a live control. `faktor.model.contextLimit`
is a CAP applied to every context manifest (the served model capacity is
the other bound); `faktor.transcript.searchLimit`, `faktor.session.titleSearch`,
`faktor.sessions.maxParallel`, `faktor.budget.*` and
`faktor.permissions.maximumChildren` are consumed by their listing/task
routes.

### Control-plane surface (disabled by default)

With no `[cloud]` section the feature state is `cloud_disabled`, but it is
disclosed only AFTER identity: a missing, invalid or foreign
`x-faktor-control-token` is the same typed 401 `unauthorized` whether the
control plane is enabled or disabled, and a foreign organization is the
same 404 a nonexistent one answers. The one exception is the daemon
bootstrap (`POST /native/orgs` with NO token presented), which may answer
the typed 409 `cloud_disabled`. Every route rides the daemon password AND,
except the bootstrap, an `x-faktor-control-token` control-plane principal;
a principal is fixed to one organization (no existence leak). Mutating
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

### Operational/admin surface (disabled by default)

These route families have no IDE control on purpose: they are operator
surfaces (and the worker-node protocol). Each is inert — a typed 409 naming
the disabled section — until its config section is wired.

- **SSO**: `POST /native/sso/start` mints the IdP authorization URL and
  `POST /native/sso/callback` consumes the single-use state and mints ONE
  control-plane session. Both ride the daemon password; enable with
  `[cloud.sso] enabled = true` + `issuer` + `client_id` (optional
  `client_secret` payload name), otherwise 409 `sso_disabled`. Clients use
  only `POST /native/sso/logout` (daemon password + the session's own
  `x-faktor-control-token`).
- **Updater**: `GET /native/updater/status`; `POST
  /native/updater/{check,stage,apply,rollback,downgrade}`. Daemon password +
  control-plane principal (viewer for status/check, member for stage, admin
  for apply/rollback/downgrade); enable with `[updater] enabled = true` and
  an allowlisted `[[updater.keys]]`, otherwise 409 `updater_disabled`.
  `faktor updater` is the local-parity admin CLI.
- **Worker plane**: `POST /native/workers/register`, `POST
  /native/workers/{id}/heartbeat`, `POST /native/jobs/claim` and `POST
  /native/jobs/{id}/result` authenticate with the registration token in the
  body (never the daemon password); `GET /native/workers`, `POST
  /native/workers/tokens`, `POST /native/workers/{id}/revoke` and `GET
  /native/jobs/{id}` are operator reads/writes over the daemon password +
  control-plane principal (`WorkerRead`/`WorkerManage`). Enable with
  `[workers] enabled = true` (and `[worker_plane]` for the second listener),
  otherwise 409 `workers_disabled`. `faktor worker run` is the worker-node
  client; job status is the operator diagnostic for a claimed job.
- **Enterprise**: `GET /native/enterprise/{status,audit,settings,artifacts,
  deletion-jobs,deletion-jobs/{id},tombstones/{scope_key}}`; `PUT
  /native/enterprise/settings`; `POST /native/enterprise/{artifacts,
  artifacts/{id}/eligible,retention/gc,deletion-jobs,
  deletion-jobs/{id}/advance,effective-config}`. Daemon password +
  control-plane principal; enable with `[enterprise] enabled = true` +
  `database`/`organization`, otherwise 409 `enterprise_disabled`.
  `faktor enterprise` is the local-parity admin CLI.
- **SCM webhook**: `POST /native/scm/webhook` (signed GitHub delivery;
  no user credential). Enable by wiring the sync inbox with `[cloud]
  github_app enabled = true` + `payload_dir` + `webhook_secret`, otherwise
  409 `scm_webhook_disabled`.
- **Introspection**: `GET /native/semantic/status` and
  `GET /native/semantic/capabilities` report the wired semantic provider
  (status is consumed by both IDE clients); `GET /native/index/coverage`
  (`?session=<id>`) reports the durable index coverage the Inspect surface
  renders. They are read-only, daemon-password authenticated, and never
  fabricate an available provider.

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
