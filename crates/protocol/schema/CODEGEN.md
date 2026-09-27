# Protocol codegen: generated vs handwritten (audit 25)

`crates/protocol/src/schema.rs` is the ONE canonical description of the
plain `faktor-protocol` DTOs and the frozen error-code constants. It is
emitted to `crates/protocol/schema/faktor-protocol.schema.json` by
`cargo run -p faktor-protocol --bin faktor-protocol-schema`. The DTO/parser
portion for the two IDE clients is regenerated from that artifact by
`node scripts/protocol-codegen.mjs`.

## Generated (never edited by hand)

| Artifact | Contents |
| --- | --- |
| `crates/protocol/schema/faktor-protocol.schema.json` | DTO shapes (`Message`, `Part`, `ToolResultBody`, `PageMeta`, `MessagesPage`, `SessionState`, `AgentStateView`) and the error-code table derived from `faktor_protocol::error::from_core`. |
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
  artifact with cargo into a temp dir and diffs the artifact AND both
  generated clients.
- `node scripts/protocol-codegen.mjs --check-clients` — node-only half used
  by lanes without cargo.

## Handwritten (deliberately NOT generated)

- `crates/protocol/src/native.rs`, `crates/protocol/src/error.rs`: the Rust
  source of truth.
- Every native HTTP-wire DTO and parser in `crates/server/src/native/*.rs`
  and the clients' counterparts (`apps/vscode/src/nativeClient.ts`'s
  `Native*` interfaces/validators, `apps/jetbrains/shared/.../NativeProtocol.kt`'s
  `Native*` data classes/parsers). The native surface is much larger than
  `faktor-protocol`; migrating it is INCREMENTAL and each migration must
  keep the wire shape byte-identical.
- All behavior/UI code: HTTP clients, retry/error classification, SSE
  readers, panels, webviews.

## Migration log

| Step | Migrated surface | Notes |
| --- | --- | --- |
| 1 | Error envelope `{error:{code,message,retryable}}` | `apps/vscode/src/nativeClient.ts` `apiError()` and `apps/jetbrains/backend/.../NativeClient.kt` `apiError()` now call the generated `parseProtocolErrorEnvelope`; the duplicated field checks were deleted. Wire behavior unchanged (missing/ill-typed `code`/`message` still falls back to `http_error`; a non-boolean `retryable` still reads as false). |
| 2 (open) | Page metadata (`page`, `hasMore`, `nextCursor`/`nextBefore`) | The clients' page DTOs are projections of the native wire routes (camelCase `sessionId`, `nextCursor`), not of `faktor_protocol::native`; migrate only when the server route DTOs are moved into `faktor-protocol`. |
| 3 (open) | Conversation messages/parts | The native wire message rows differ from `faktor_protocol::native::Message` (wire: `createdMs`/`data`/`kind` parts; protocol: snake_case typed parts used by ACP). Do not generate the wire projections from the protocol shapes until the two converge. |

The migration is deliberately conservative: a generated client may only
replace a handwritten DTO when the wire shape is provably identical.
