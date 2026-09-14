# Faktor v7.5.6 wire compatibility manifest

**Measured status (unmodified-client round): transport-exact, 33/37
response surfaces exact, 4 documented divergences.** The pinned upstream
client — `@kilocode/sdk@7.5.6` from `Kilo-Org/kilocode@fa02955b`, vendored
verbatim in `compat/kilo-v756/upstream-sdk/` — is replayed against this
daemon by `cargo test -p faktor-tests-compat --lib
unmodified_upstream_client`. The corpus has **37 recorded steps**
(`compat/kilo-v756/sdk-traces/`): request bytes (method, path, query
encoding, body, Basic auth header, content-type) match the recorded client
bytes for **all 37**; **33 of 37** response surfaces match structurally
(empty `session.messages` page, `session.create` ×3, `session.get`,
`session.status`, `session.prompt`, `session.messages.newest/before`,
`session.fork`, `session.diff.content`, `session.diff.missing-message`,
`session.revert`, `session.unrevert`, `global.health`,
`session.list`, `session.update`, `session.summarize`, `session.abort`,
`session.deleteMessage`, `session.delete`, `session.delete.clean`,
`config.get`, `config.warnings`, `config.overlay`, `pty.create`,
`pty.remove`, `permission.list`,
`question.list`, `network.list`, `global.dispose`, `instance.dispose`,
`instance.reload`); **4 are locked
divergence fixtures** (see "What the unmodified client still cannot drive").
Every run writes
`target/certification/kilo-compat.json` (`faktor-kilo-compat/v1`: commit,
sdk_version, requests/responses passed-of-total, required_divergences,
status), which `scripts/capabilities-manifest.mjs` consumes: `compat_v756`
stays **PARTIAL** until responses are N/N, and the Woodpecker PR/trusted
`kilo-compat` lane runs the replay with `FAKTOR_COMPAT_REQUIRE_NODE=1` (a
missing Node records `status: failed`, never a skip). The shapes described
below are what the DAEMON currently answers; unless a section says otherwise
they are **not** the shapes an unmodified v7.5.6 client parses. No
"scaffold" claim is made for any surface that does not pass.

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
  client is TypeScript). With `FAKTOR_COMPAT_REQUIRE_NODE=1` a missing Node
  is a hard failure that also records `status: failed` in the report (the
  required `kilo-compat` CI lane sets it); without it the replay records
  `status: skipped` and returns. The offline half (manifest hashes + corpus
  invariants) always runs. Regenerate goldens: `FAKTOR_COMPAT_FREEZE_TRACES=1`.
- **Divergences fixed this round** (wire-permitted, compat layer only):
  1. `POST /session/{sessionID}/message` now accepts the real SDK v2 input
     union (optional `model`, `tools` name→bool map, `snapshotInitialization:
     "wait"`, `text` parts with `synthetic`/`time`, `file` parts by
     `filename`/`url`, `agent`/`subtask` inputs) while still accepting the
     scaffold body; unknown TOP-LEVEL fields stay 422.
  2. `POST /session/{sessionID}/unrevert` honours the SDK's body-less form:
     the target IS the durable revert state (the newest checkpoint carrying
     the `restored_ms` marker written by rollback, cleared by redo), and
     the 200 answer is the SDK `Session9` rich session projection with no
     `revert` object (the revert state is gone — never a stale marker).
  3. `GET /session/{sessionID}/diff` accepts the SDK's `messageID=` and
     `full=true|false` spellings (the scaffold `message=`/`full=1` stay
     accepted); an unknown/malformed `messageID` is now the SDK-declared
     `400 BadRequestError` (`{name,data:{message,kind}}`) — `SessionDiffErrors`
     declares 400 and no other class, so the old 409 was not an SDK status.
  4. **Rich session superset + SDK list/update routes**: `session.create`,
     `session.get` and `session.fork` emit the SDK `Session1/2/3/5` rich
     fields (`id`, `slug`, `projectID`, `workspaceID`, `directory`, `model`,
     `version`, `time{created,updated}`) ADDITIVELY next to the frozen
     aliases (`sessionID`, `title`, `state`, `createdMs`, `updatedMs`).
     `GET /session` now answers the SDK's **bare `Session1[]`** (the old
     `{sessions:[…]}` envelope is gone; the SDK type is the contract and the
     daemon's checked-in consumers were updated with it; `limit` is honored,
     bounded), and `PATCH /session/{sessionID}` is registered, answering the
     rich `Session4` projection; `metadata`/`permission`/archive requests
     refuse `400 InvalidRequestError` instead of being silently dropped.
  5. **Whole status map**: `GET /session/status` without `?session_id=` now
     answers the SDK's `{ [sessionID]: SessionStatus }` map for every
     durable session (`{type:"idle"|"busy"}`; mid-turn states are `busy`).
     The `?session_id=` alias keeps returning the single state projection.
  6. **Rich message/part projections**: the accepted-turn response and the
     message page now carry the SDK `Message`/`Part` identity fields
     additively: `info.id` (= the durable sequence), `agent`, assistant
     `parentID`/`mode`/`path`/`cost`/`tokens`, user `model`, and part
     `id`/`messageID` (+ `sessionID` on the single-session send response).
     The page deliberately omits the SDK `time` object and the part
     `sessionID`: a forked session re-times copied rows and re-points part
     session ids, while the frozen page contract guarantees the fork
     projection stays structurally equal to its source (locked by
     `faktor-server`'s fork test); page ids are sequence-stable for the same
     reason. The daemon has one agent/mode ("default") and does not project
     per-message usage accounting, so `cost`/`tokens` are zeros — never
     guessed numbers.
  7. **Report contract**: the replay writes `faktor-kilo-compat/v1` to
     `target/certification/kilo-compat.json` (commit, sdk_version,
     requests/responses passed-of-total, required_divergences, status);
     `status: passed` is written only when every step is an exact pass. The
     capabilities manifest derives `compat_v756` from that report at the
     exact SHA — file existence of the fixture corpus proves nothing.
  8. **SDK scalar booleans**: `session.summarize`, `session.abort`,
     `session.deleteMessage`, `session.delete`, `global.dispose`,
     `instance.dispose` and `instance.reload` now answer the SDK's declared
     bare `boolean`. `abort` reports whether an operation was actually
     cancelled (a fully idle session answers `false`); `summarize` still
     computes the bounded digest server-side (the SDK type has no text
     field); the delete contract (only 400/404 declared) makes a PARKED
     session deletable — a `ReadyForNextTurn` session with no active turn
     record is durably Closed through the daemon's own end path (the one
     `/global/dispose` uses) because the session layer's active-state
     predicate also covers the parked machine; a genuinely mid-turn session
     still refuses 409.
  9. **SDK list aliases**: `GET /permission` answers the declared bare
     `PermissionRequest[]` over the daemon's REAL pending asks (every class;
     capability → `permission`, requested target string(s) → `patterns`, the
     full capability payload → `metadata`, `always` empty — this slice keeps
     no always-allow rules). `GET /question` and `GET /network` answer the
     declared bare `QuestionRequest[]`/`SessionNetworkWait[]` as **empty**:
     this slice has no structured-question or reconnect-wait subsystem (its
     question/network-class asks ride `/permission`), so inventing question
     text/options or a wait timestamp would be fabrication.
 10. **Config surface**: `GET /config` answers the BARE config object (the
     SDK `Config` type has no required fields; the legacy `/config/get`
     envelope stays); `GET /config/warnings` answers the declared bare
     `Array<{path,message,detail?}>` with `path` set to the documented
     literal `runtime` source label (this daemon's config is an in-memory
     runtime object with no file layer — a fabricated filename would be
     worse); `GET /config/overlay` answers a one-layer projection: the
     runtime object is the single source (`kind:"runtime"`, editable
     `false`, applies as the global layer), so `effective == global ==` the
     runtime config, `project` is `{}`, the SDK's file `targets` all report
     `exists:false` with empty paths/revisions (there are no config files),
     and `fields` carries the real per-key values with `source:"system"` and
     the honest editable allowlist (`collections` empty). No file
     path/revision/inheritance chain is fabricated.
 11. **`global.health`** carries the SDK's `{healthy: true, version}`
     ADDITIVELY next to the frozen `{ok, protocol}` aliases the legacy
     fixtures/consumers read.
 12. **SDK diff error class + content**: `GET /session/{sessionID}/diff`
     with an unknown or malformed `messageID` answers the SDK-declared `400
     BadRequestError` (`SessionDiffErrors` declares 400 and no other
     class); with `full=true` a recorded checkpoint projects the real
     unified diff AND the SDK-required `additions`/`deletions` counts,
     counted from that same diff (the frozen `{path,status,diff?}` keys stay
     additively; the counts ride only the `?full` projection — without it
     no CAS reads happen and the frozen path+status-only contract stays
     byte-identical). Both branches are exact corpus passes.
 13. **Revert/unrevert**: `POST /session/{sessionID}/revert` rolls the
     recorded checkpoint back through the CAS-verified rollback path and
     answers the SDK `Session8` rich session projection plus the durable
     revert marker `revert: {messageID (the request's own target, echoed
     byte-identically), workspace: "restored"}`; the rollback stamps
     `checkpoint.restored_ms`, which is the durable state unrevert reads.
     The conflict refusal (an independently edited file) keeps the frozen
     honest `409 {ok:false,message,conflict:{path}}` shape rather than
     claiming the SDK's `SessionBusyError` with a false "busy" reason.
 14. **SDK PTY create/remove**: `POST /pty` runs over the daemon's REAL PTY
     registry (`AppState.ptys`, the same `faktor-pty` authority+registry the
     native session-owned terminal surface drives) and answers the SDK `Pty`
     object from the values the spawn actually used (real `id`/`pid`,
     requested `command`/`args`/`cwd`/`title` echoed, `status` from the live
     child state); `exitCode` is omitted because the PTY backend exposes no
     exit-code authority, `sessionID` because the SDK create body carries no
     session id (no ownership row is minted — the native session-scoped
     route mints those). `DELETE /pty/{ptyID}` terminates the real child
     tree through the backend's bounded shutdown and answers the declared
     boolean (unknown id → SDK `404 PtyNotFoundError`). `env` VALUES never
     cross the daemon's ONE env authority: the map's NAMES become the spawn
     allowlist (documented, tested).
 15. **Replay checkpoint corpus**: the replay harness wires the REAL
     CAS-backed checkpoint store and a checkpoint-recording probe tool, so
     the revert/unrevert/diff steps run against durable state rather than
     refusing with "snapshots unavailable". The probe writes a per-process
     gitignored scratch file (`target/compat-replay-<pid>/probe.txt`,
     removed before every run) through the workspace handle and records the
     real missing→existing transition — no fabricated state; the CAS blobs
     are the bytes actually written. The scratch path is handed to the
     driver as `probePath` and golden-normalized to `@string`, so
     concurrent test processes in one checkout never share or race it.

### What the unmodified client still cannot drive (honest)

- **Auth gate + provider OAuth (locked by design).** The unauthenticated
  `auth.missing` probe asserts the daemon's auth gate (401
  `{error:{code,message,retryable}}` before any handler effect); the
  vendored types declare no 401 for `global.health`, so there is no
  SDK-declared shape it could match and the gate is deliberate. The SDK's
  `auth.set` is provider OAuth at `PUT /auth/{providerID}`; the daemon has
  no provider-credential store, so the route is honestly 404 — an exact
  pass would require OAuth persistence outside the allowed compat surface.
- **`provider.list` metadata (re-audited, still locked).** The SDK's
  `Provider.models[].Model` requires `api{id,url,npm}`, per-token cost,
  limit, lifecycle `status` and `release_date`. The daemon catalog exposes
  real capabilities and a pricing STATE
  (`Known|ConservativeCeiling|Stale|LocalZero|Unknown`) but has no source
  for `api.url`, `api.npm`, `release_date` or `status` (it cannot tell
  alpha/beta/active apart), and the SDK type makes
  `cost.input/output/cache` numeric REQUIRED — there is no null/optional
  representation, and `PricingState::Unknown` rows must never be flattened
  to `0`. Emitting partial Model objects would violate the declared type;
  the honest 404 stays. Exact missing primitives: per-model API package id,
  base URL and npm package; a model release date; a lifecycle status; and a
  numeric cost representation for Unknown-pricing rows.
- **`global.event` SSE multiplex (re-audited, still locked).** The
  transport is exact (GET `/global/event`, auth, `200
  text/event-stream`), but the daemon's `GlobalEvent` envelope is the
  frozen Faktor journal projection (`type` names like `session_created` /
  `session_next_text_delta`, payload fields like `session_id`) while the
  SDK declares a different union (`session.created`, `properties`
  objects, string `directory`/`project`/`workspace`). Exact missing
  primitives: an SDK-union frame projector over the journal, plus a
  replayable SSE frame corpus (the replay compares no frame bodies today).
- **Streaming.** The SSE request (`GET /global/event`) is transport-locked
  and opens `200 text/event-stream`, but the replay does not golden-compare
  SSE frame bodies (an idle harness emits no event), and the daemon's
  journal/global event payload union is not the SDK's multiplexed
  `GlobalEvent` union (directory/project/workspace + payload). The
  divergence is note-locked rather than body-compared.
- **Rich message page omissions.** The page intentionally omits the SDK
  `time` object and part `sessionID` (fork-stable page contract, see fixed
  item 6); the send response carries both. `cost`/`tokens` are zeros
  because the frozen surface does not project per-message usage.
- **Unreachable by this client/env**: project/workspace lifecycle routes
  (`/project`, `/experimental/workspace`) have no daemon counterpart.

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
      "modelID": "qwen3.8",
      "id": "3",
      "agent": "default",
      "parentID": "2",
      "mode": "default",
      "path": { "cwd": ".", "root": "." },
      "cost": 0.0,
      "tokens": { "input": 0, "output": 0, "reasoning": 0,
                  "cache": { "read": 0, "write": 0 } }
    },
    "parts": [ { "id": "3:0", "messageID": "3", "type": "text", "text": "pong" } ]
  },
  {
    "info": { "sessionID": "1", "messageID": "2", "role": "user",
              "createdMs": 1750000001000,
              "providerID": "ollama", "modelID": "qwen3.8",
              "id": "2", "agent": "default",
              "model": { "providerID": "ollama", "modelID": "qwen3.8" } },
    "parts": [ { "id": "2:0", "messageID": "2", "type": "text", "text": "fix it" } ]
  }
]
```

Paging signal: `x-has-more: true|false` response header (the daemon entry
DTO rejects unknown fields, so paging cannot ride it). Prompt (user)
messages appear with their parts: user rows are stored as `{text, files}`
message data, projected as their wire text part. The rich fields are
additive; the page omits the SDK `time` object and the part `sessionID`
because a forked session re-times rows and re-points part session ids while
the page contract keeps the fork projection structurally equal to its
source (tested). **Exact pass** for non-empty pages modulo those two
documented omissions.

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
    "modelID": "qwen3.8",
    "id": "3",
    "agent": "default",
    "time": { "created": 1750000002000 },
    "parentID": "2",
    "mode": "default",
    "path": { "cwd": ".", "root": "." },
    "cost": 0.0,
    "tokens": { "input": 0, "output": 0, "reasoning": 0,
                "cache": { "read": 0, "write": 0 } }
  },
  "parts": [ { "id": "3:0", "sessionID": "1", "messageID": "3",
               "type": "text", "text": "pong" } ]
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
above); the RESPONSE is an **exact pass** for the rich SDK shape (`cost`/
`tokens` are the documented zeros; the daemon has one agent/mode).

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
unified diff text (`diff`) plus the SDK-required `additions`/`deletions`
counts, all resolved through the CAS and counted from the same diff
(pre-after-blob rows are refused honestly with 409, exactly like the
snapshot `diff_latest`):

```json
[
  { "path": "f.txt", "status": "modified",
    "diff": " line1\n line2\n-old\n+new\n line6",
    "additions": 1, "deletions": 1 }
]
```

Without `?full` no CAS reads happen and the frozen `{path,status}` shape
stays byte-identical. This is an **exact client pass**: `path`/`diff` ride
additively while the declared `SnapshotFileDiff` fields (`additions`,
`deletions`, `status`, optional `file`/`patch`) are satisfied (the trace
freezes the exact key set).

Filters: `?message=<seq>` / the SDK's `?messageID=<seq>` limits the
projection to ONE checkpoint — the newest checkpoint recorded at-or-before
that message's `created_ms`. An unknown or malformed message is the
SDK-declared `400 BadRequestError` (`{name:"BadRequest",data:{message,kind:"Query"}}`)
— `SessionDiffErrors` declares 400 and no other class. `?file=<rel
path>` keeps only the entries whose recorded path equals the relative path
(exact match; no filesystem access). No checkpoints → `[]`.

## Operation manifest

This table inventories the DAEMON's compat operations and their own tests.
Per-surface unmodified-client status lives in `sdk-traces/` (see above);
rows here do not imply v7.5.6-client shape compatibility.

| operation | route | method | shape | status |
|---|---|---|---|---|
| session.create | `/session` | POST | rich `Session3` fields + frozen aliases `{sessionID,title,createdMs}` | implemented, tested; **exact client pass** (rich fields additive) |
| session.list | `/session` | GET | bare rich `Session1[]` (newest first; `limit` honored, bounded to 500) | implemented, tested; **exact client pass** (the SDK type is the contract; the old `{sessions:[…]}` envelope was removed and its consumers updated) |
| session.get | `/session/{sessionID}` | GET | rich `Session2` fields + frozen aliases | implemented, tested; **exact client pass** |
| session.update | `/session/{sessionID}` | POST, PATCH | POST: `{sessionID,title,updatedMs}` (frozen alias). PATCH (the SDK method): rich `Session4`; title is the one durable session-row field the daemon owns (control chars stripped, bounded 1..=200 chars); `metadata`/`permission`/archive → 400 `InvalidRequestError`, never silently dropped | implemented, tested; **exact client pass** for PATCH |
| session.status | `/session/status`, `/session/status?session_id=` | GET | without query: `{ [sessionID]: {type:"idle"\|"busy"} }` (whole map); with query: the single `SessionState` projection | implemented, tested; **exact client pass** for the SDK's map call |
| session.fork | `/session/{sessionID}/fork` | POST | rich `Session5` fields + frozen aliases (`<title> (fork)`) | implemented, tested (rows+parts copied in order; fork independent); **exact client pass** |
| session.summarize | `/session/{sessionID}/summarize` | POST | SDK-declared bare `boolean` `true`; the bounded 4 KiB digest of the newest messages is still computed server-side (the SDK type has no text field) | implemented, tested; **exact client pass** |
| session.delete | `/session/{sessionID}` | DELETE | SDK-declared bare `boolean` `true`; durable end = `SessionEnded` journal + `lifecycle=Closed`; lingering queued prompts cancelled; registries closed. A PARKED `ReadyForNextTurn` session (no active turn record) is closed through the daemon's own end path (the session layer's active-state predicate covers the parked machine); a genuinely mid-turn session still refuses 409. Residual gap: rows are retained (no store row-drop API), so a deleted session reads as Completed/Closed and refuses prompts | implemented, tested; **exact client pass** |
| session.deleteMessage | `/session/{sessionID}/message/{messageID}` | DELETE | SDK-declared bare `boolean` `true` — durable one-transaction removal of the message row + its parts; sequences stay stable (paging skips the hole); 404 unknown message; 409 tool-result dependencies; 409 while in-flight newest | implemented, tested; **exact client pass** |
| session.message (page) | `/session/{sessionID}/message` | GET | rich entry `{info,parts}` (above; no `time`, no part `sessionID`) | implemented, tested; **exact client pass** for the fork-stable projection |
| session.message (send) | `/session/{sessionID}/message` | POST | rich `{info: AssistantMessage, parts: Part[]}` (above) | implemented, tested (200 done, 202 queued, 502 no-reply); **exact client pass** |
| session.abort | `/session/{sessionID}/abort` | POST | SDK-declared bare `boolean`: `true` iff at least one operation was actually cancelled (a fully idle session answers `false`) | implemented, tested; **exact client pass** |
| session.diff | `/session/{sessionID}/diff` | GET | `{path,status,diff?,additions?,deletions?}[]` (above); unknown/malformed `messageID` → SDK-declared 400 `BadRequestError` | implemented, tested; accepts SDK `messageID`/`full`; **exact client pass** for both the unknown-`messageID` error branch and the `?full` content projection (real checkpoint, real counts) |
| session.revert / unrevert | `/session/{sessionID}/revert`, `/unrevert` | POST | SDK `Session8`/`Session9` rich session; revert carries `revert: {messageID, workspace:"restored"}`; unrevert reads the durable `restored_ms` marker and answers with no `revert` object | implemented, tested; **exact client pass** against a real checkpoint |
| session.state | `/session/{sessionID}/state` | GET | `SessionState` | implemented, tested; not an SDK method |
| permission.list | `/permission/list?session_id=`, `/permission` | GET | legacy: `{permissions:[{id,session_id,capability,detail}]}`; SDK alias: bare `PermissionRequest[]` (capability → permission, target → patterns, payload → metadata, `always:[]`) | implemented, tested (real pending requests); **exact client pass** for the SDK alias (empty and non-empty projections tested) |
| permission.reply | `/permission/reply`, `/api/perm/{id}/resolve` | POST | `{ok:true}` | implemented, tested; SDK calls `/permission/{requestID}/reply` (not registered) |
| question.list | `/question/list?session_id=`, `/question` | GET | legacy: `{questions:[...]}`; SDK alias: bare `QuestionRequest[]` — truthfully empty (no structured-question subsystem in this slice; question-class asks ride `/permission`) | implemented, tested; **exact client pass** for the SDK alias |
| question.reply | `/question/reply` | POST | `{ok:true}` | implemented, tested; SDK calls `/question/{requestID}/reply` (not registered) |
| question.reject | `/question/reject` | POST | `{ok:true}` | implemented, tested; SDK calls `/question/{requestID}/reject` (not registered) |
| network.list | `/network/list?session_id=`, `/network` | GET | legacy: `{networks:[...]}`; SDK alias: bare `SessionNetworkWait[]` — truthfully empty (no reconnect-wait subsystem; network-class asks ride `/permission`) | implemented, tested; **exact client pass** for the SDK alias |
| network.reply | `/network/reply` | POST | `{ok:true}` | implemented, tested; SDK calls `/network/{requestID}/reply` (not registered) |
| network.reject | `/network/reject` | POST | `{ok:true}` | implemented, tested; SDK calls `/network/{requestID}/reject` (not registered) |
| config.get | `/config/get`, `/config` | GET | legacy `/config/get`: `{config}` envelope; SDK `/config`: the BARE config object | implemented, tested; **exact client pass** for the SDK alias |
| config.update | `/config/update` | POST | `{ok:true}` — only `model`/`compact_at_usage`/`instructions` are daemon-editable; any other key → 400 with the allowlist | implemented, tested; the SDK updates via PATCH `/config` (not registered) |
| config.overlay | `/config/overlay` | GET, POST | GET (SDK read): one-layer projection — runtime object as the single source (`kind:"runtime"`), `effective == global`, `project:{}`, file targets `exists:false` with empty paths/revisions, real per-key `fields`; POST: `{ok:true}` bounded full replace | implemented, tested; **exact client pass** for the SDK GET |
| config.overlayUpdate | `/config/overlayUpdate` | POST | `{ok:true}` (bounded shallow merge) | implemented, tested; the SDK PATCHes `/config/overlay` (not registered) |
| config.warnings | `/config/warnings` | GET | SDK-declared bare `Array<{path,message,detail?}>`; `path` is the documented literal `runtime` source label (no file layer exists) | implemented, tested; **exact client pass** |
| config.set | `/config/set` | POST | `{ok:true}` (legacy full replace, kept) | implemented, tested |
| pty.create/remove | `/pty/create`, `/pty`, `/pty/remove`, `/pty/{ptyID}` | POST, DELETE | legacy `/pty/create` spawn `{ok:true,pty_id,pid}`; SDK `/pty` answers the `Pty` object (real id/pid/status, requested command/args/cwd/title echoed, `exitCode`/`sessionID` omitted — no source); SDK `DELETE /pty/{ptyID}` kills the real child tree and answers the declared boolean (unknown id → `404 PtyNotFoundError`) | implemented, tested; **exact client pass** for SDK create+remove; legacy update/remove keep `{ok:true}` |
| global.dispose | `/global/dispose` | POST | SDK-declared bare `boolean` `true` after every live session is durably ended (idempotent) | implemented, tested; **exact client pass** |
| instance.dispose | `/instance/dispose` | POST | same handler, bare `boolean` `true` | implemented, tested; **exact client pass** |
| instance.reload | `/instance/reload` | POST | bare `boolean` `true` after re-running the idempotent `recover()` sweep | implemented, tested; **exact client pass** |
| auth.set | `/auth/set` | POST | `{ok:true,password}` — rotates the server password; old credentials 401 immediately | implemented, tested; the SDK's `auth.set` is provider OAuth at `PUT /auth/{providerID}` (404) — a different operation |
| auth.remove | `/auth/remove` | POST | `{ok:true}` — back to the startup env password | implemented, tested; SDK `DELETE /auth/{providerID}` is provider OAuth (404) |

## Residual gaps (need backend subsystems outside this round's allowed files)

1. **Revert projection completeness** — revert/unrevert now run against the
   real checkpoint store and answer the SDK `Session8`/`Session9` shapes
   (above). Residual: the durable marker records WHICH checkpoint is
   reverted, not the client's target message id, so read projections
   (`session.get`/`list`) cannot name `revert.messageID` and omit `revert`
   (optional in the SDK type) rather than fabricate one; page `time`/part
   `sessionID` cannot ride the fork-equal page contract; per-message
   cost/tokens are not modeled by the frozen surface.
2. **Missing client routes** — `GET /provider`, `GET /pty` (list),
   `GET/PATCH /pty/{ptyID}` (terminal metadata the registry deliberately
   does not keep), `/pty/{ptyID}/connect-token`, `PUT/DELETE
   /auth/{providerID}`, `PATCH /config`, `PATCH /config/overlay`, and the
   project/workspace lifecycle routes are not registered. `GET /config`,
   `GET /config/overlay`, `GET /permission`, `GET /question`, `GET
   /network`, `PATCH /session/{sessionID}` and `POST/DELETE /pty` now ARE.
3. **SSE event union** — the daemon's journal/global frames are not the
   SDK `GlobalEvent` union; an idle replay emits no frame, so the gap is
   note-locked (no body golden). An SDK-union projector plus a frame corpus
   are the exact missing primitives.
4. **Provider Model metadata** — the SDK `Model` requires
   `api{id,url,npm}`/numeric cost/limit/status/release_date; the daemon
   cannot source `api.url`/`api.npm`/`release_date`/lifecycle `status`, and
   `PricingState::Unknown` rows have no honest numeric cost representation
   in the required type (unknown pricing is never faked as 0).
5. **PTY metadata routes** — the real PTY registry keeps live processes,
   not command/args/cwd/title rows, so `GET /pty`, `GET /pty/{ptyID}` and
   `PATCH /pty/{ptyID}` (the SDK's list/get/update shape) stay
   unregistered; create/remove are exact passes. On Windows the POSIX-`sh`
   trace command fails honestly at spawn (the backend is ConPTY); the
   compat replay lane is Linux, and the golden is platform-normalized
   (`pid` is an `@int` template; the rest of the shape is POSIX-stable).
6. **Node-gated replay** — the unmodified-client replay needs Node >= 22.15
   (the upstream client is TypeScript). The required PR/trusted
   `kilo-compat` lane runs it with `FAKTOR_COMPAT_REQUIRE_NODE=1`, so a
   missing Node fails the lane AND records `status: failed` in the report;
   the offline manifest + corpus checks always run.
7. **session.delete row removal** — delete durably ends the session
   (journaled `SessionEnded` + lifecycle Closed, queued prompts cancelled,
   registries closed) but retains the row: a store-level `remove_session`
   SQL does not exist in this slice.
8. **Per-request `tools` map** — the SDK body's `tools` name→bool map is
   accepted (the v2 input union) but NOT enforced: tool availability is the
   daemon's session bundle + `CapabilitySet`, so the map is currently a
   no-op. The replay's checkpoint probe is a harness fixture scripted
   through the provider; it does not claim per-request enablement.
   Enforcing the map needs a per-turn tool filter in the runtime, outside
   the compat surface.
