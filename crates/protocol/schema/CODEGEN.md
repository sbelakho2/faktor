# Protocol codegen: generated vs handwritten (audit 25 + audit 15)

`crates/protocol/src/schema.rs` is the ONE canonical description of the
plain `faktor-protocol` DTOs, the migrated native wire DTOs and the frozen
error-code constants. It is emitted to
`crates/protocol/schema/faktor-protocol.schema.json` by
`cargo run -p faktor-protocol --bin faktor-protocol-schema`. The DTO/parser
portion for the two IDE clients is regenerated from that artifact by
`node scripts/protocol-codegen.mjs`.

## Generated (never edited by hand)

| Artifact | Contents |
| --- | --- |
| `crates/protocol/schema/faktor-protocol.schema.json` | DTO shapes (`Message`, `Part`, `ToolResultBody`, `PageMeta`, `MessagesPage`, `SessionState`, `AgentStateView`) plus the migrated native wire DTOs (`AttachmentId`, `AttachmentUpload`, `TaskRun`, `TaskRunStarted`, `TaskRunCancelled`, `TaskRunWorkItem`, `TaskRunStartRequest`, `SessionPromptRequest`, `SessionPromptReceipt`) and the error-code table derived from `faktor_protocol::error::from_core`. |
| `apps/vscode/src/generated/protocolDto.ts` | TypeScript DTO types, strict validators, default constructors, error-code constants and `parseProtocolErrorEnvelope`. |
| `apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/GeneratedProtocolDto.kt` | Kotlin data classes / sealed enum, parsers, error-code constants and `parseProtocolErrorEnvelope`. |

Regenerate after changing `crates/protocol`:

```
cargo run -p faktor-protocol --bin faktor-protocol-schema -- --out crates/protocol/schema/faktor-protocol.schema.json
node scripts/protocol-codegen.mjs --write
```

Drift gates:

- `cargo test -p faktor-protocol` — the checked-in artifact must equal the
  emitter output (Rust half, no node needed).
- `node scripts/protocol-codegen.mjs --check` — full CI gate: re-emits the
  artifact with cargo into a temp dir, diffs the artifact AND both
  generated clients, and validates the native endpoint inventory below.
- `node scripts/protocol-codegen.mjs --check-clients` — node-only half used
  by lanes without cargo (also validates the inventory against the
  checked-in artifact).

## Native endpoint inventory (audit 15)

Every public native endpoint served by the daemon router
(`crates/server/src/api/lifecycle.rs` plus the three-route dedicated
worker-plane router in `crates/server/src/worker_plane.rs`) is classified
here. The classification is SHRINK-ONLY: the audited handwritten set in
`HANDWRITTEN_FROZEN` (scripts/protocol-codegen.mjs) must match the table's
`handwritten-grandfathered` rows exactly, so a migration must delete its
frozen entry in the same commit and a new handwritten DTO surface is refused
until a review-visible edit of that list — while `generated`, `no-body` and
`streaming-special-case` classifications are always allowed.

| Classification | Meaning |
| --- | --- |
| `generated` | the endpoint's request/response DTOs are described in the canonical schema and regenerated into both IDE clients; every listed DTO must exist in `faktor-protocol.schema.json` |
| `handwritten-grandfathered` | a structured JSON DTO surface that predates the migration and is still hand-rolled in the clients; this set may only shrink |
| `no-body` | no JSON DTO the clients parse: no request body (or a raw provider payload) and a plain ack response |
| `streaming-special-case` | the response is an SSE stream or a raw byte payload whose client handling is deliberately framework-specific |

| Endpoint | Classification | Generated DTOs |
| --- | --- | --- |
| `GET /capabilities` | handwritten-grandfathered |  |
| `GET /models` | handwritten-grandfathered |  |
| `GET /native/agents` | handwritten-grandfathered |  |
| `POST /native/agents/{child_id}/budget` | handwritten-grandfathered |  |
| `POST /native/agents/{child_id}/cancel` | no-body |  |
| `POST /native/agents/{child_id}/model` | handwritten-grandfathered |  |
| `POST /native/agents/{child_id}/pause` | no-body |  |
| `POST /native/agents/{child_id}/resume` | no-body |  |
| `POST /native/agents/{child_id}/retry` | no-body |  |
| `POST /native/agents/{child_id}/steer` | handwritten-grandfathered |  |
| `GET,POST /native/approvals` | handwritten-grandfathered |  |
| `POST /native/approvals/{id}/decide` | handwritten-grandfathered |  |
| `POST /native/credits/grant` | handwritten-grandfathered |  |
| `GET,POST /native/enterprise/artifacts` | handwritten-grandfathered |  |
| `POST /native/enterprise/artifacts/{id}/eligible` | no-body |  |
| `GET /native/enterprise/audit` | handwritten-grandfathered |  |
| `GET,POST /native/enterprise/deletion-jobs` | handwritten-grandfathered |  |
| `GET /native/enterprise/deletion-jobs/{id}` | handwritten-grandfathered |  |
| `POST /native/enterprise/deletion-jobs/{id}/advance` | no-body |  |
| `POST /native/enterprise/effective-config` | handwritten-grandfathered |  |
| `POST /native/enterprise/retention/gc` | handwritten-grandfathered |  |
| `GET,PUT /native/enterprise/settings` | handwritten-grandfathered |  |
| `GET /native/enterprise/status` | handwritten-grandfathered |  |
| `GET /native/enterprise/tombstones/{scope_key}` | handwritten-grandfathered |  |
| `GET /native/entitlements` | handwritten-grandfathered |  |
| `GET /native/events` | streaming-special-case |  |
| `GET /native/evidence/{id}` | handwritten-grandfathered |  |
| `POST /native/evidence/{id}/retrieve` | handwritten-grandfathered |  |
| `GET /native/health` | handwritten-grandfathered |  |
| `GET /native/identity` | handwritten-grandfathered |  |
| `GET /native/index/coverage` | handwritten-grandfathered |  |
| `GET /native/jobs/{id}` | handwritten-grandfathered |  |
| `POST /native/jobs/{id}/result` | handwritten-grandfathered |  |
| `POST /native/jobs/claim` | handwritten-grandfathered |  |
| `GET /native/messages` | handwritten-grandfathered |  |
| `GET /native/orchestrator/graph` | handwritten-grandfathered |  |
| `GET,POST /native/orgs` | handwritten-grandfathered |  |
| `GET,POST /native/orgs/{id}/members` | handwritten-grandfathered |  |
| `POST /native/permission/reply` | handwritten-grandfathered |  |
| `GET /native/permissions` | handwritten-grandfathered |  |
| `GET /native/providers` | handwritten-grandfathered |  |
| `GET /native/ready` | handwritten-grandfathered |  |
| `GET /native/repositories` | handwritten-grandfathered |  |
| `POST /native/scm/webhook` | no-body |  |
| `GET /native/semantic/capabilities` | handwritten-grandfathered |  |
| `GET /native/semantic/status` | handwritten-grandfathered |  |
| `POST /native/session` | handwritten-grandfathered |  |
| `POST /native/session/{id}/abort` | handwritten-grandfathered |  |
| `GET /native/session/{id}/agents` | handwritten-grandfathered |  |
| `POST /native/session/{id}/agents/{child}/presentation` | handwritten-grandfathered |  |
| `POST /native/session/{id}/attachments` | generated | `AttachmentUpload`, `AttachmentRef` |
| `GET /native/session/{id}/attachments/blob/{digest}` | generated | `AttachmentRef` |
| `GET /native/session/{id}/attachments/blob/{digest}/bytes` | streaming-special-case |  |
| `GET /native/session/{id}/attachments/ref/{ref_id}` | generated | `AttachmentRef` |
| `GET /native/session/{id}/attachments/ref/{ref_id}/bytes` | streaming-special-case |  |
| `GET,POST /native/session/{id}/board` | handwritten-grandfathered |  |
| `GET /native/session/{id}/checkpoints` | handwritten-grandfathered |  |
| `GET /native/session/{id}/events` | streaming-special-case |  |
| `POST /native/session/{id}/prompt` | generated | `SessionPromptRequest`, `SessionPromptReceipt` |
| `GET,POST /native/session/{id}/task-runs` | generated | `TaskRun`, `TaskRunStartRequest`, `TaskRunStarted` |
| `GET /native/session/{id}/task-runs/{run_id}` | generated | `TaskRun` |
| `POST /native/session/{id}/task-runs/{run_id}/cancel` | generated | `TaskRunCancelled` |
| `GET /native/session/{id}/tasks` | handwritten-grandfathered |  |
| `GET /native/session/{id}/tasks/{task_id}/verification` | handwritten-grandfathered |  |
| `GET,POST /native/session/{id}/terminal` | handwritten-grandfathered |  |
| `GET /native/session/{id}/terminal/events` | streaming-special-case |  |
| `POST /native/session/{id}/terminals/{terminal_id}/input` | handwritten-grandfathered |  |
| `POST /native/session/{id}/terminals/{terminal_id}/kill` | handwritten-grandfathered |  |
| `GET /native/session/{id}/terminals/{terminal_id}/output` | streaming-special-case |  |
| `POST /native/session/{id}/terminals/{terminal_id}/reconcile` | handwritten-grandfathered |  |
| `POST /native/session/{id}/terminals/{terminal_id}/resize` | handwritten-grandfathered |  |
| `POST /native/session/{id}/tournament` | handwritten-grandfathered |  |
| `GET /native/session/{id}/tournament/{tournament_id}` | handwritten-grandfathered |  |
| `GET /native/session/{id}/tournaments` | handwritten-grandfathered |  |
| `POST /native/session/{id}/tournaments/{tournament_id}/abort` | handwritten-grandfathered |  |
| `POST /native/session/{id}/tournaments/{tournament_id}/decide` | handwritten-grandfathered |  |
| `GET /native/session/{id}/turns` | handwritten-grandfathered |  |
| `GET /native/session/{id}/usage` | handwritten-grandfathered |  |
| `GET /native/session/{id}/verification` | handwritten-grandfathered |  |
| `GET /native/sessions` | handwritten-grandfathered |  |
| `POST /native/sso/callback` | handwritten-grandfathered |  |
| `POST /native/sso/logout` | handwritten-grandfathered |  |
| `POST /native/sso/start` | handwritten-grandfathered |  |
| `GET /native/tasks/{id}/completion-steps` | handwritten-grandfathered |  |
| `GET /native/tasks/{id}/proof` | handwritten-grandfathered |  |
| `GET /native/terminals` | handwritten-grandfathered |  |
| `POST /native/updater/apply` | handwritten-grandfathered |  |
| `POST /native/updater/check` | handwritten-grandfathered |  |
| `POST /native/updater/downgrade` | handwritten-grandfathered |  |
| `POST /native/updater/rollback` | handwritten-grandfathered |  |
| `POST /native/updater/stage` | handwritten-grandfathered |  |
| `GET /native/updater/status` | handwritten-grandfathered |  |
| `GET /native/usage` | handwritten-grandfathered |  |
| `GET /native/workers` | handwritten-grandfathered |  |
| `POST /native/workers/{id}/heartbeat` | handwritten-grandfathered |  |
| `POST /native/workers/{id}/revoke` | no-body |  |
| `POST /native/workers/register` | handwritten-grandfathered |  |
| `POST /native/workers/tokens` | handwritten-grandfathered |  |
| `GET /session/{id}/projection` | handwritten-grandfathered |  |

## Handwritten (deliberately NOT generated)

- `crates/protocol/src/native.rs`, `crates/protocol/src/error.rs`: the Rust
  source of truth.
- `crates/protocol/src/schema.rs` re-declares the migrated native DTOs as
  schema entries; the behavioral Rust types stay authoritative
  (`faktor_core::attachment::AttachmentId`, the native handlers' serde
  shapes). Every migration must keep the wire shape byte-identical, and the
  `AttachmentId` schema entry has a parity test against the real serde
  struct.
- Every remaining `handwritten-grandfathered` native HTTP-wire DTO and parser
  listed in the inventory table above: in `crates/server/src/native/*.rs` and
  the clients' counterparts (`apps/vscode/src/nativeClient.ts`'s remaining
  `Native*` interfaces/validators, `apps/jetbrains/shared/.../NativeProtocol.kt`'s
  remaining `Native*` data classes/parsers). The native surface is much larger
  than `faktor-protocol`; migrating it is INCREMENTAL and each migration must
  keep the wire shape byte-identical.
- All behavior/UI code: HTTP clients, retry/error classification, SSE
  readers, panels, webviews.

## Migration log

| Step | Migrated surface | Notes |
| --- | --- | --- |
| 1 | Error envelope `{error:{code,message,retryable}}` | `apps/vscode/src/nativeClient.ts` `apiError()` and `apps/jetbrains/backend/.../NativeClient.kt` `apiError()` now call the generated `parseProtocolErrorEnvelope`; the duplicated field checks were deleted. Wire behavior unchanged (missing/ill-typed `code`/`message` still falls back to `http_error`; a non-boolean `retryable` still reads as false). |
| 2 (open) | Page metadata (`page`, `hasMore`, `nextCursor`/`nextBefore`) | The clients' page DTOs are projections of the native wire routes (camelCase `sessionId`, `nextCursor`), not of `faktor_protocol::native`; migrate only when the server route DTOs are moved into `faktor-protocol`. |
| 3 (open) | Conversation messages/parts | The native wire message rows differ from `faktor_protocol::native::Message` (wire: `createdMs`/`data`/`kind` parts; protocol: snake_case typed parts used by ACP). Do not generate the wire projections from the protocol shapes until the two converge. |
| 4 | Attachments + task-run DTOs (audit 15) | `AttachmentId`, `AttachmentRef`, `AttachmentUpload`, `TaskRun`, `TaskRunStarted`, `TaskRunCancelled`, `TaskRunWorkItem`, `TaskRunStartRequest` are canonical schema types now; both IDEs' parsed surfaces delegate to the generated `ProtocolAttachmentId`/`ProtocolAttachmentRef`/`ProtocolTaskRun*` parsers (the VS Code `NativeTaskRun`/`NativeAttachmentId` names and the JetBrains `NativeTaskRun*`/`NativeAttachmentId` names are aliases), and the six routes are classified `generated` in the inventory (the attachment routes split the ref-id and digest grammars: `ref/{ref_id}` metadata/bytes vs `blob/{digest}` metadata/bytes). The server stays the strict authority: `NativeAttachmentUpload`/`StartTaskRunRequest` keep their `deny_unknown_fields` serde DTOs, mirrored field-for-field by the schema. |
| 5 | Ordinary-prompt DTO (idempotency finding 1) | `SessionPromptRequest` (strict: `session_id`, the REQUIRED `submission_id` 1..=64 ASCII `[0-9a-f-]`, `prompt`, optional `files`) and `SessionPromptReceipt` (`op_id`, `run_id`, `accepted`, `queued`) are canonical schema types now; the prompt route is classified `generated`. The server's `NativePromptRequestBody` stays the strict `deny_unknown_fields` authority, mirrored field-for-field; a repeated `submission_id` with the equal body replays the stored receipt byte-for-byte through the durable `prompt_admission` claim. |

The migration is deliberately conservative: a generated client may only
replace a handwritten DTO when the wire shape is provably identical.
