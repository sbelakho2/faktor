# Faktor v7.5.6 wire compatibility manifest

**Measured status (unmodified-client round): transport-exact, shape-divergent.**
The pinned upstream client — `@kilocode/sdk@7.5.6` from
`Kilo-Org/kilocode@fa02955b`, vendored verbatim in
`compat/kilo-v756/upstream-sdk/` — is replayed against this daemon by
`cargo test -p faktor-tests-compat`. The corpus has **36 recorded steps**
(`compat/kilo-v756/sdk-traces/`): request bytes (method, path, query
encoding, body, Basic auth header, content-type) match the recorded client
bytes for **all 36**; **1 of 36** response surfaces passes structural
comparison (an empty `session.messages` page); **35 are locked divergence
fixtures** — the rich `Session`/`Message`/`Part` model the upstream client
declares, scalar `boolean` ops, and 9 routes the daemon does not register.
The shapes described below are what the DAEMON currently answers; unless a
section says otherwise they are **not** the shapes an unmodified v7.5.6
client parses. No "scaffold" claim is made for any surface that does not
pass.

Every row is auth-required (the frozen Basic
`base64("kilo:"+password)` form, plus the legacy Bearer /
`x-faktor-server-password` forms). Wire ids are numeric strings; wire
message ids on this surface are the durable message SEQUENCE (the same
identity `revert`/`diff`/`deleteMessage` consume; on a single-session
store the sequence equals the row id).

## Unmodified upstream client corpus

- **Vendored client**: `packages/sdk/js` (npm `@kilocode/sdk@7.5.6`), 47
  files / 945,920 bytes, byte-identical to the pinned commit. Every file is
  blake3-hashed in `compat/kilo-v756/upstream.json` (repository, tag,
  commit, fetch protocol, license hashes); the MIT text is in
  `compat/kilo-v756/LICENSES/kilocode-LICENSE.txt`. Re-fetch + re-hash:
  `compat/kilo-v756/vendor-sdk.sh`; offline hash verification:
  `cargo test -p faktor-tests-compat --lib upstream_manifest`.
- **Golden traces**: `compat/kilo-v756/sdk-traces/*.json`. Each step carries
  the call that produced it, the recorded request bytes, the recorded
  daemon response, the vendored-type provenance (`sdk_type`), a
  `passing`/`divergence` status, and an honest note. Recorded requests come
  from the client's own `fetch` config (`tests/compat/js/run.mjs` imports
  the vendored TypeScript through a harness-side `.js`→`.ts` resolver; the
  upstream tree is never edited). Responses are daemon behavior; for
  `passing` steps an exact structural match is required, for `divergence`
  steps the fixture locks the current behavior AND the documented missing
  SDK fields — if the daemon ever satisfies the SDK shape the test fails
  and forces promotion + a docs update.
- **Running it**: `cargo test -p faktor-tests-compat --lib
  unmodified_upstream_client`. Node >= 22.15 is required (the upstream
  client is TypeScript); without `node` the replay **skips with an explicit
  message** — CI's `rust:1.98` image has no node — and
  `FAKTOR_COMPAT_REQUIRE_NODE=1` turns the skip into a failure for lanes
  that must run it. The offline half (manifest hashes + corpus invariants)
  always runs. Regenerate goldens: `FAKTOR_COMPAT_FREEZE_TRACES=1`.
- **Divergences fixed this round** (wire-permitted, compat layer only):
  1. `POST /session/{sessionID}/message` now accepts the real SDK v2 input
     union (optional `model`, `tools` name→bool map, `snapshotInitialization:
     "wait"`, `text` parts with `synthetic`/`time`, `file` parts by
     `filename`/`url`, `agent`/`subtask` inputs) while still accepting the
     scaffold body; unknown TOP-LEVEL fields stay 422.
  2. `POST /session/{sessionID}/unrevert` accepts the SDK's body-less form
     and refuses honestly 409 (no durable revert target) instead of failing
     extraction 415.
  3. `GET /session/{sessionID}/diff` accepts the SDK's `messageID=` and
     `full=true|false` spellings (the scaffold `message=`/`full=1` stay
     accepted); an unknown `messageID` is now an honest 409 instead of
     silently ignoring the filter.

### What the unmodified client still cannot drive (honest)

- **Rich session model.** `session.create` (SDK `Session3`),
  `session.list` (`Session1[]`), `session.get` (`Session2`),
  `session.fork` (`Session5`), `session.update` (`Session4`, PATCH) require
  `id`, `slug`, `projectID`, `directory`, `version`, `time{created,updated}`,
  `summary`/`cost`/`tokens`/`share`/`revert`; the daemon answers
  `{sessionID,title,createdMs}` / `{sessions:[SessionSummary]}` /
  `SessionSummary`, and `PATCH /session/{sessionID}` is not registered
  (405).
- **Rich message model.** Message pages and the accepted-turn response
  require `Message`/`AssistantMessage` (`id`, `time.created`,
  `parentID`, `mode`, `agent`, `path`, `cost`, `tokens`) and `Part` (`id`,
  `sessionID`, `messageID`, `state`/`time` objects); the daemon answers its
  scaffold `{info:{sessionID,messageID,role,createdMs,providerID,modelID},
  parts}`. Only the empty page `[]` is structurally identical.
- **Scalar ops.** The SDK declares `boolean` for `session.delete`,
  `session.deleteMessage`, `session.summarize`, `session.abort`,
  `global.dispose`, `instance.dispose`, `instance.reload`; the daemon
  answers `{ok:true}` / `{aborted:[…]}` / `{sessionID,title,summary}`.
- **Unregistered routes (404/405 for the real client)**: `GET /config`,
  `GET /config/overlay`, `GET /provider`, `POST /pty`, `GET /permission`,
  `GET /question`, `GET /network`, `PATCH /session/{sessionID}`,
  `PUT /auth/{providerID}`. The daemon's `/config/update`,
  `/provider/list`, `/pty/create`, `/permission/list`, `/question/list`,
  `/network/list`, `/auth/set` are different operations or paths.
- **Diff entries.** With checkpoint rows the daemon answers
  `{path,status,diff?}`; the SDK declares `SnapshotFileDiff`
  `{file,patch,before,after,additions,deletions,status}`. This round's
  harness has no checkpoint rows, so only the empty projection is covered.
- **Streaming.** The SSE request (`GET /global/event`) is transport-locked
  and opens `200 text/event-stream`, but the daemon's event payload union
  is not the SDK's `GlobalEvent` union, and an idle session emits no frame;
  the divergence is note-locked rather than body-compared.
- **Unreachable by this client/env**: provider OAuth
  (`PUT/DELETE /auth/{providerID}`) and project/workspace lifecycle routes
  (`/project`, `/experimental/workspace`) have no daemon counterpart; PTY
  interactivity is unavailable because the supervisor has no PTY
  abstraction.

## Daemon shapes (scaffold wire; divergence-documented above)

### 1. `GET /session/{sessionID}/message?before=&limit=` — page

Bare JSON **array** of `{info: Message, parts: Part[]}`, newest first
(`before`/`limit` paging unchanged; the wire omits `seq`):

```json
[
  {
    "info": {
      "sessionID": "1",
      "messageID": "3",
      "role": "assistant",
      "createdMs": 1750000002000,
      "providerID": "ollama",
      "modelID": "qwen3.8"
    },
    "parts": [ { "type": "text", "text": "pong" } ]
  },
  {
    "info": { "sessionID": "1", "messageID": "2", "role": "user",
              "createdMs": 1750000001000,
              "providerID": "ollama", "modelID": "qwen3.8" },
    "parts": [ { "type": "text", "text": "fix it" } ]
  }
]
```

Paging signal: `x-has-more: true|false` response header (the daemon entry
DTO rejects unknown fields, so paging cannot ride it). Prompt (user)
messages appear with their parts: user rows are stored as `{text, files}`
message data, projected as their wire text part. **Divergence:** the real
SDK `Message`/`Part` objects carry the rich fields listed above.

### 2. `POST /session/{sessionID}/message` — send (response)

Daemon: `{info: AssistantMessage, parts: Part[]}` where `info` is the
durable assistant message row the accepted turn produced and `parts` its
wire parts (turn runs to completion inside the request; SSE carries
progress):

```json
{
  "info": {
    "sessionID": "1",
    "messageID": "3",
    "role": "assistant",
    "createdMs": 1750000002000,
    "providerID": "ollama",
    "modelID": "qwen3.8"
  },
  "parts": [ { "type": "text", "text": "pong" } ]
}
```

Queueing semantics (documented choice): a prompt that durably queues
behind an active logical turn answers **HTTP 202 Accepted** with the same
shape — `parts: []` and `info.messageID: ""` (nothing is materialized
until the queued turn starts; the client polls the page / SSE). The 202
status IS the queueing signal: the daemon DTO has `deny_unknown_fields`, so
a `queued` field would be protocol drift. A turn that ends without any
assistant content (provider failure before the first chunk) is an honest
`502 {ok:false, message}`, never a fabricated message. The REQUEST side
accepts both the real SDK input union and the scaffold body (see fixes
above); the RESPONSE is the divergence described above.

### 3. `GET /session/{sessionID}/diff?message=&file=&full=1` — file diffs

Bare JSON **array**, one entry per recorded checkpoint (file-change) row,
newest first. Status is the recorded before→after transition (`added` |
`deleted` | `modified`). Without full mode entries carry only
`path`+`status`:

```json
[
  { "path": "f.txt", "status": "deleted" },
  { "path": "created-empty.txt", "status": "added" },
  { "path": "f.txt", "status": "modified" }
]
```

With `?full=1` (or the SDK's `?full=true`) each entry also carries the
unified diff text (`diff`), resolved through the CAS (pre-after-blob rows
are refused honestly with 409, exactly like the snapshot `diff_latest`):

```json
[
  { "path": "f.txt", "status": "modified",
    "diff": " line1\n line2\n-old\n+new\n line6" }
]
```

Filters: `?message=<seq>` / the SDK's `?messageID=<seq>` limits the
projection to ONE checkpoint — the newest checkpoint recorded at-or-before
that message's `created_ms` (unknown message → honest 409). `?file=<rel
path>` keeps only the entries whose recorded path equals the relative path
(exact match; no filesystem access). No checkpoints → `[]`. **Divergence:**
the real SDK entry type is `SnapshotFileDiff`
(`file`/`patch`/`additions`/`deletions`).

## Operation manifest

This table inventories the DAEMON's compat operations and their own tests.
Per-surface unmodified-client status lives in `sdk-traces/` (see above);
rows here do not imply v7.5.6-client shape compatibility.

| operation | route | method | shape | status |
|---|---|---|---|---|
| session.create | `/session` | POST | `{sessionID,title,createdMs}` | implemented, tested; client-shape divergence (SDK `Session3`) |
| session.list | `/session` | GET | `{sessions:[SessionSummary]}` | implemented, tested; client-shape divergence (SDK `Session1[]`) |
| session.get | `/session/{sessionID}` | GET | `SessionSummary` | implemented, tested; client-shape divergence (SDK `Session2`) |
| session.update | `/session/{sessionID}` | POST | `{sessionID,title,updatedMs}` — title is the one durable session-row field the daemon owns: control chars stripped, bounded 1..=200 chars, persisted through the session layer (store row + bumped `updatedMs`); 400 malformed (no title), 404 unknown session | implemented, tested; the SDK uses PATCH (405) |
| session.status | `/session/{sessionID}/status`, `/session/status?session_id=` | GET | `SessionState` projection | implemented (aliases of `/state`); the SDK calls `/session/status` without a session filter (400) |
| session.fork | `/session/{sessionID}/fork` | POST | `{sessionID,title,createdMs}` (`<title> (fork)`) | implemented, tested (rows+parts copied in order; fork independent); client-shape divergence (SDK `Session5`) |
| session.summarize | `/session/{sessionID}/summarize` | POST | `{sessionID,title,summary}` (bounded 4 KiB digest of newest messages) | implemented, tested; SDK expects `boolean` (the required providerID/modelID body is accepted and ignored) |
| session.delete | `/session/{sessionID}` | DELETE | `{ok:true}` | implemented, tested; SDK expects `boolean`; refused 409 mid-turn (active turn record / active machine); durable end = `SessionEnded` journal + `lifecycle=Closed`; lingering queued prompts cancelled; registries closed. Residual gap: rows are retained (no store row-drop API), so a deleted session reads as Completed/Closed and refuses prompts |
| session.deleteMessage | `/session/{sessionID}/message/{messageID}` | DELETE | `{ok:true}` — durable one-transaction removal of the message row + its parts; sequences stay stable (paging skips the hole); 404 unknown message; 409 tool-result dependencies; 409 while in-flight newest | implemented, tested; SDK expects `boolean` |
| session.message (page) | `/session/{sessionID}/message` | GET | array `{info,parts}` (above) | implemented, tested; empty page structurally passes the SDK, non-empty entries diverge |
| session.message (send) | `/session/{sessionID}/message` | POST | `{info,parts}` (above) | implemented, tested (200 done, 202 queued, 502 no-reply); request accepts the SDK input union; response shape diverges |
| session.abort | `/session/{sessionID}/abort` | POST | `{aborted:[opId]}` | implemented, tested; SDK expects `boolean` |
| session.diff | `/session/{sessionID}/diff` | GET | `{path,status,diff?}[]` (above) | implemented, tested; accepts SDK `messageID`/`full`; non-empty entries diverge from `SnapshotFileDiff` |
| session.revert / unrevert | `/session/{sessionID}/revert`, `/unrevert` | POST | `{ok,restored?,conflict?}` / `{ok:false,message}` | implemented, tested; unrevert accepts the SDK's body-less form; SDK expects rich Session projections |
| session.state | `/session/{sessionID}/state` | GET | `SessionState` | implemented, tested; not an SDK method |
| permission.list | `/permission/list?session_id=` | GET | `{permissions:[{id,session_id,capability,detail}]}` | implemented, tested (real pending requests); SDK calls `GET /permission` (404) with an array response |
| permission.reply | `/permission/reply`, `/api/perm/{id}/resolve` | POST | `{ok:true}` | implemented, tested; SDK calls `/permission/{requestID}/reply` |
| question.list | `/question/list?session_id=` | GET | `{questions:[...]}` | implemented, tested; SDK calls `GET /question` (404) |
| question.reply | `/question/reply` | POST | `{ok:true}` | implemented, tested; SDK calls `/question/{requestID}/reply` |
| question.reject | `/question/reject` | POST | `{ok:true}` | implemented, tested; SDK calls `/question/{requestID}/reject` |
| network.list | `/network/list?session_id=` | GET | `{networks:[...]}` | implemented, tested; SDK calls `GET /network` (404) |
| network.reply | `/network/reply` | POST | `{ok:true}` | implemented, tested; SDK calls `/network/{requestID}/reply` |
| network.reject | `/network/reject` | POST | `{ok:true}` | implemented, tested; SDK calls `/network/{requestID}/reject` |
| config.get | `/config/get` | GET | `{config}` (daemon config RwLock) | implemented, tested; SDK calls `GET /config` (404) |
| config.update | `/config/update` | POST | `{ok:true}` — only `model`/`compact_at_usage`/`instructions` are daemon-editable; any other key → 400 with the allowlist | implemented, tested; the SDK updates via PATCH `/config` |
| config.overlay | `/config/overlay` | POST | `{ok:true}` (bounded full replace) | implemented, tested; the SDK READS via GET `/config/overlay` (405) |
| config.overlayUpdate | `/config/overlayUpdate` | POST | `{ok:true}` (bounded shallow merge) | implemented, tested; the SDK PATCHes `/config/overlay` |
| config.warnings | `/config/warnings` | GET | `{warnings:[...]}` | implemented, tested; SDK expects a bare `Array<{path,message,detail?}>` |
| config.set | `/config/set` | POST | `{ok:true}` (legacy full replace, kept) | implemented, tested |
| pty.create/update/remove | `/pty/create`, `/pty/update`, `/pty/remove` | POST | `409 {ok:false, message:"ptys unsupported by the local supervisor"}` | implemented as explicit rejection, tested; the SDK calls POST `/pty` (404), so the refusal is unreachable from it |
| global.dispose | `/global/dispose` | POST | `{ok:true}` | implemented, tested; SDK expects `boolean` |
| instance.dispose | `/instance/dispose` | POST | `{ok:true}` | implemented (same handler), tested; SDK expects `boolean` |
| instance.reload | `/instance/reload` | POST | `{ok:true}` after re-running daemon recover() | implemented, tested; SDK expects `boolean` |
| auth.set | `/auth/set` | POST | `{ok:true,password}` — rotates the server password; old credentials 401 immediately | implemented, tested; the SDK's `auth.set` is provider OAuth at `PUT /auth/{providerID}` (404) — a different operation |
| auth.remove | `/auth/remove` | POST | `{ok:true}` — back to the startup env password | implemented, tested; SDK `DELETE /auth/{providerID}` is provider OAuth (404) |

## Residual gaps (need backend subsystems outside this round's allowed files)

1. **Rich session/message projections** — `Session`/`Message`/`Part` require
   project/workspace/slug/version/time/cost/tokens and rich part records
   that the daemon does not model in this slice; the compat handlers would
   need a projection layer over the session layer.
2. **Missing client routes** — `GET /config`, `GET /config/overlay`,
   `GET /provider`, `POST /pty` (plus `/pty/{ptyID}`), `GET /permission`,
   `GET /question`, `GET /network`, `PATCH /session/{sessionID}`,
   `PUT/DELETE /auth/{providerID}` are not registered; adding them means
   api.rs routing + handlers.
3. **Scalar response shapes** — delete/deleteMessage/summarize/abort/
   dispose/reload answer objects where the SDK declares booleans; changing
   them breaks the daemon's own locked tests and the scaffold DTOs in
   `faktor-protocol`, so it was not done in the compat layer this round.
4. **SSE event union** — the daemon's journal/global frames are not the
   SDK `GlobalEvent` union; an idle session emits no frame, so the gap is
   note-locked (no body golden).
5. **Diff entries** — the harness has no checkpoint rows; the
   `SnapshotFileDiff` field naming (`file`/`patch`/`additions`/`deletions`)
   is untested against a non-empty projection.
6. **Node-gated replay** — the unmodified-client replay needs Node >= 22.15
   (the upstream client is TypeScript). It skips with an explicit message
   where node is absent (CI's `rust:1.98` image); the offline manifest +
   corpus checks always run, and `FAKTOR_COMPAT_REQUIRE_NODE=1` fails
   instead of skipping.
7. **session.delete row removal** — delete durably ends the session
   (journaled `SessionEnded` + lifecycle Closed, queued prompts cancelled,
   registries closed) but retains the row: a store-level `remove_session`
   SQL does not exist in this slice.
8. **session.update title via PATCH** — the daemon registers POST; the SDK
   uses PATCH. Method mismatch, not a payload mismatch.
