# Edition 5 acceptance and verification

Source: `Faktor_IDE_UI_UX_2.pdf`, "Acceptance and verification" pp.173–176, plus the testing/evidence requirements on pp.023–025, 036–037 and 177–183.

The PDF labels A01–A40 "specification-level scenarios" and states they were **not run against a Faktor product build in this document project** (p.173). The scenarios below therefore remain gated acceptance targets until executed against a build. The 40 scenarios cover the major failure boundaries; every control row additionally requires its own default, field validation, transactional apply, timing, ownership fence, unavailable state and recovery checks (p.176).

Notation: "visible outcome" means the UI state a reviewer can inspect, not an internal log. "Forbidden fake state" means a state the UI must never claim without the corresponding runtime acknowledgement.

---

## A01 — Provider/model atomicity

- **Source:** p.173.
- **Preconditions:** Two served providers/models; one provider/model pair is made unavailable; a draft exists.
- **Steps:** Select the unavailable pair in S02/S03; try Save/Select; inspect the prior default and the draft.
- **Expected visible outcomes:** The pair is rejected atomically; neither the default provider nor default model changes; the draft survives; the unavailable pair remains visible with a reason.
- **Forbidden fake states:** Saving only the provider; silently substituting another endpoint; clearing the draft; showing the new pair in effective state before acknowledgement.
- **Field-level checks:** `provider`, `model`, `capabilityAuthority`, `availability`, `draft`, `default provider`, `default model`; error names the pair and the safe recovery.

## A02 — Capability authority

- **Source:** p.173.
- **Preconditions:** A model with `capabilityAuthority = unknown` and a model with `known` authority.
- **Steps:** Open the capability sheet; attempt tools/vision/structured output on the unknown model; compare with the known model.
- **Expected visible outcomes:** Gated tools/vision/schema are disabled for unknown authority; the reason is visible; manual labels do not certify support; known authority exposes the supported set.
- **Forbidden fake states:** Treating a model marketing name or manual label as exact authority; showing tools as enabled because the provider family usually supports them.
- **Field-level checks:** `capabilitiesGated`, `tools`, `vision`, `structuredOutput`, `capabilityAuthority` (`known` / `declared_generic` / `unknown`), `source`.

## A03 — Generation compatibility

- **Source:** p.173.
- **Preconditions:** A model with a known parameter set and an override that the model does not support.
- **Steps:** Change the model in S02; carry the incompatible tuning into S05; attempt Preview/Apply.
- **Expected visible outcomes:** The complete tuning bundle is revalidated; unsupported parameters fail locally before admission; the incompatible edit remains staged for review; strict DTOs receive no unknown field.
- **Forbidden fake states:** Silently dropping an unsupported field; sending it and letting the provider ignore it; applying a partial compatible subset without disclosure.
- **Field-level checks:** parameter descriptor (`kind`, `min`, `max`, `choices`, `requires_exact_authority`), `effective request`, `requested` vs `effective`, apply timing, field error per parameter.

## A04 — Unset versus zero

- **Source:** p.173.
- **Preconditions:** Temperature supports an explicit zero and an unset/default state.
- **Steps:** Set temperature to 0; apply; then Reset; inspect the effective request.
- **Expected visible outcomes:** Reset removes the override; temperature 0 and absent temperature remain distinct in the preview and receipt; provider default is shown as inherited, not 0.
- **Forbidden fake states:** Rendering absent as 0; Reset writing 0; a UI "default" that actually sends 0.
- **Field-level checks:** `temperature` (`unset` vs `0`), winning source, reset action, effective request receipt.

## A05 — Active-call capture

- **Source:** p.173.
- **Preconditions:** A task call is in flight with a captured model/effort/budget.
- **Steps:** Edit model, effort and budget while the call is active; inspect the in-flight request and its historical receipt.
- **Expected visible outcomes:** The in-flight request and receipt are unchanged; the UI states when the edit applies (Next call/Next task/New conversation); the draft shows the new preference separately.
- **Forbidden fake states:** Rewriting the active call; showing the picker's new model as the executed model; mutating a historical receipt.
- **Field-level checks:** call `provider`, `model`, `parameters`, `startedAt`, `route reason`; draft values; apply timing.

## A06 — Secret storage

- **Source:** p.173.
- **Preconditions:** Provider/MCP/hook credentials are configured by reference.
- **Steps:** Save a connection; export diagnostics; inspect logs/config exports and the shared settings snapshot.
- **Expected visible outcomes:** Credential values are absent from shared config, diagnostics, logs and exported bundles; only an opaque reference/verification state is visible; local deletion is distinct from remote revocation.
- **Forbidden fake states:** A secret value in a snapshot or export; a "revoked" claim from local deletion; an environment value echoed by an error.
- **Field-level checks:** `credential reference`, `secret classification`, `vault binding generation`, diagnostic omitted categories, log redaction.

## A07 — Loopback versus LAN

- **Source:** p.173.
- **Preconditions:** An Ollama-compatible local adapter and a custom OpenAI-compatible adapter.
- **Steps:** Test the Ollama default endpoint; test a custom adapter without opt-in; attempt a private-LAN endpoint; inspect the policy states.
- **Expected visible outcomes:** Ollama default and custom-provider opt-in differ; private LAN remains refused under current policy; `allow_loopback` does not authorize LAN; the refusal is typed and inline.
- **Forbidden fake states:** Private LAN shown as allowed; a global "allow local" switch; loopback opt-in treated as a LAN bypass.
- **Field-level checks:** `allow_loopback`, `sandbox.network`, endpoint host class, `network_guarantee`, refusal reason.

## A08 — Remote locality

- **Source:** p.173.
- **Preconditions:** A remote workspace/daemon binding with a loopback model endpoint.
- **Steps:** Inspect S31; resolve the loopback URL; inspect editor, daemon/shell, model endpoint, embedding endpoint, vault and worker locations.
- **Expected visible outcomes:** The loopback URL resolves on the daemon host, not the laptop; all locations are explicit; a host change invalidates old handles; drafts survive.
- **Forbidden fake states:** "Local" derived from the laptop's perspective; a copied credential in project config; replaying an old request on the new host.
- **Field-level checks:** `editor host`, `workspace URI`, `daemon/shell host`, `model endpoint`, `vault location`, `worker location`, `binding generation`.

## A09 — Ask enforcement

- **Source:** p.173.
- **Preconditions:** Ask preset; a prompt that asks for a write/effectful tool.
- **Steps:** Run Ask; inspect offered tools and dispatch; inspect the label.
- **Expected visible outcomes:** Ask cannot dispatch a write/effectful tool even if the prompt requests it; the offered set is filtered; a prompt-only route is labelled as such until enforcement exists.
- **Forbidden fake states:** Calling a plain prompt "read-only"; showing a tool as available but denying only after execution; prompt text changing policy.
- **Field-level checks:** offered tool set, dispatch decision, preset label, `allowed_tools`, denial reason, task envelope.

## A10 — Preset and policy

- **Source:** p.173.
- **Preconditions:** A custom agent/preset, a managed deny, and a project rule/skill.
- **Steps:** Create a preset; add instructions/skills; attempt to widen tools or edit the owner checkout; inspect the effective envelope.
- **Expected visible outcomes:** A custom agent, rule, skill or hook cannot widen managed authority or edit the owner checkout directly; the preset shows its resolved permissions; managed denies remain authoritative.
- **Forbidden fake states:** Preset text changing grants; "full auto" shown as a normal mode; a project file overriding a managed deny.
- **Field-level checks:** `allowed_tools`, `child_roles`, `maximum_children`, managed deny list, `mutation_mode`, owner-checkout mode, effective envelope.

## A11 — Context null

- **Source:** p.174.
- **Preconditions:** The daemon projection returns `contextUsage = null`.
- **Steps:** Open S01/S08/S27; inspect the context meter and any percentage/bar.
- **Expected visible outcomes:** `contextUsage=null` renders **Unavailable**, never 0%, complete, or a fabricated green utilization bar.
- **Forbidden fake states:** A 0% bar; a "context full" claim; a measured label on a draft estimate.
- **Field-level checks:** `contextUsage` (null), label `Unavailable`, basis, `estimated` vs `measured`, no fabricated percentage.

## A12 — Context arithmetic

- **Source:** p.174.
- **Preconditions:** A captured context preview with categories and reserves.
- **Steps:** Inspect S08; sum categories; compare input + output reserve + safety reserve with capacity.
- **Expected visible outcomes:** All estimate categories share one basis and sum correctly; output/safety reserve are not counted as consumed input; overflow is rejected before admission; the sample math 18,400 + 4,096 + 2,048 + 8,224 = 32,768 is consistent.
- **Forbidden fake states:** Mixed tokenizer/basis categories summed; reserves shown as consumed; silent truncation to fit.
- **Field-level checks:** `capacity`, `input`, `output reserve`, `safety reserve`, `headroom`, category sums, `basis` (estimated/observed/unavailable).

## A13 — Source identity

- **Source:** p.174.
- **Preconditions:** Attachments from a selection, a diagnostic and a file reference; two worktrees/sessions.
- **Steps:** Attach sources; switch worktree/session; reopen the preview and manifest.
- **Expected visible outcomes:** Selection, diagnostic and file attachments retain URI/range/version across switches; a reference resolves in its own worktree; the manifest identifies each source.
- **Forbidden fake states:** A filename alone shown as content; a path resolving against a global current directory; stale version silently reused.
- **Field-level checks:** `uri`, `range`, `version`, `content hash`, `worktree`, `inclusion reason`, `source id`.

## A14 — Attachment failure

- **Source:** p.174.
- **Preconditions:** One unsupported, one oversize and one stale attachment.
- **Steps:** Stage them; attempt Send; inspect the attachment list and composer text.
- **Expected visible outcomes:** Failed items remain visible with a reason; invalid admission is blocked; the text is preserved; Remove/Retry is available.
- **Forbidden fake states:** Silent omission on Send; truncated attachment shown as complete; text cleared by the error.
- **Field-level checks:** mime, size, model gate (`vision` / `documentCapable`), `maxUploadBytes`, admission error, draft text.

## A15 — Compaction continuity

- **Source:** p.174.
- **Preconditions:** A compaction plan with a configured minimum reduction; a pending approval and an unknown external effect.
- **Steps:** Run compaction; force a failed/minimal-reduction result; inspect retained facts and obligations.
- **Expected visible outcomes:** Failed/minimal-reduction compaction does not advance success; criteria and pending effects survive; the prior valid basis remains; a successful result records measured before/after and a summary version.
- **Forbidden fake states:** A 1% reduction accepted as success; pending approval erased by summary; UI folding labelled as runtime compaction.
- **Field-level checks:** `expected revision`, `retained facts`, `measured before/after`, `minimum reduction`, `candidate identity`, `pending operations`, `effect status`.

## A16 — Memory deletion

- **Source:** p.174.
- **Preconditions:** A saved memory record used in an earlier call; a deletion request.
- **Steps:** Delete the record; inspect the deletion receipt and prior prompt/artifact copies.
- **Expected visible outcomes:** Future-use deletion is acknowledged; earlier prompt/artifact copies are disclosed as retained; the exact record and scope are shown; a project cannot redirect global memory storage.
- **Forbidden fake states:** Claiming prior copies are deleted; deleting a broader scope than named; hiding the record without a deletion receipt.
- **Field-level checks:** record id, scope (user/project/session), `stopped_future_use_at_revision`, retained prior artifact refs, deletion receipt.

## A17 — Approval exactly once

- **Source:** p.174.
- **Preconditions:** One live permission waiter; duplicate clicks; a stale/wrong-session waiter.
- **Steps:** Click Allow twice; reply with a stale/wrong-session generation; inspect the waiter and dispatch.
- **Expected visible outcomes:** Duplicate clicks cannot reply twice; stale/wrong-session/generation waiters refuse visibly; exactly one effect is dispatched; the card disables after receipt.
- **Forbidden fake states:** Two effects from two clicks; a stale reply accepted; a wrong-session reply shown as resolved.
- **Field-level checks:** `session`, `generation`, `request id`, `action fingerprint`, `consumed`, `expired`, reply result, dispatch receipt.

## A18 — Persistent rule boundary

- **Source:** p.174.
- **Preconditions:** Allow once; a request to remember; a persistent policy store present/absent.
- **Steps:** Allow once; attempt Remember; inspect the pending request and future policy.
- **Expected visible outcomes:** Allow once cannot silently persist; remembered rules require policy save/version acknowledgement; with no store, Remember is unavailable; a saved rule has scope, version and expiry.
- **Forbidden fake states:** A remembered rule created by Allow once; a rule shown active without a store; a future rule auto-replying to another pending request.
- **Field-level checks:** decision, persistence flag, rule scope/version/expiry, policy revision, pending request generation.

## A19 — Command normalization

- **Source:** p.174.
- **Preconditions:** Compound shell, interpreter and symlink targets; a prefix/glob rule.
- **Steps:** Preview the command; attempt approval using a deceptive prefix/glob; inspect the normalized target.
- **Expected visible outcomes:** Compound shell/interpreter/symlink targets cannot exploit a decorative prefix/glob approval; the normalized command and effect are visible; changed args require a new decision.
- **Forbidden fake states:** Raw string prefix shown as safe; symlink target hidden; interpreter payload approved by the wrapper name alone.
- **Field-level checks:** normalized executable/argv, canonical target, cwd, symlink resolution, matched rule, effect class.

## A20 — Sandbox truth

- **Source:** p.174.
- **Preconditions:** A configured sandbox with filesystem/shell/network guarantees; a failed setup; a network-capable fallback.
- **Steps:** Attempt an affected action; inspect sandbox status and fallback; compare guarantees.
- **Expected visible outcomes:** Failed setup blocks affected actions; fallback network access is not labelled complete isolation; filesystem, shell and network guarantees are separate.
- **Forbidden fake states:** A network-capable fallback shown as fully isolated; sandbox failure hidden; "sandboxed" claimed without observed guarantees.
- **Field-level checks:** `filesystem`, `shell`, `network`, `network_guarantee`, `home`, setup state, refusal reason.

## A21 — MCP bounds

- **Source:** p.175.
- **Preconditions:** Existing MCP servers; a new entry exceeding count/byte/arg limits.
- **Steps:** Add a server over the limits; save; inspect the previous valid config.
- **Expected visible outcomes:** Server count and UTF-8 byte/argument limits fail before persistence; the prior valid config stays intact; the error names the exact bound.
- **Forbidden fake states:** Saving an over-bound entry; truncating silently; dropping the prior config.
- **Field-level checks:** server count ≤ 8; name ≤ 128 UTF-8 bytes; command ≤ 512; args ≤ 32; each arg ≤ 512; prior config revision.

## A22 — MCP transport/auth

- **Source:** p.175.
- **Preconditions:** A reviewed stdio config; a remote/OAuth capability that is not implemented.
- **Steps:** Open S16; attempt to select HTTP/OAuth; cancel auth; inspect Connected state.
- **Expected visible outcomes:** Reviewed stdio config never appears as implemented remote/OAuth; cancelled auth is not Connected; unsupported transport is labelled, not offered as idle.
- **Forbidden fake states:** stdio config shown as HTTP/OAuth; cancelled auth shown as Connected; remote effect shown as cancelled on disconnect.
- **Field-level checks:** transport kind, auth state, account/server binding, capability negotiation, health state.

## A23 — Project executable trust

- **Source:** p.175.
- **Preconditions:** A repository containing MCP/hook/skill configuration; a manifest with setup.
- **Steps:** Open the repository; view the manifest; attempt to run setup/install/start.
- **Expected visible outcomes:** Opening a repository or skill manifest does not execute MCP servers, hooks, setup or installs; a project-supplied executable requires deliberate trust; the previous valid server/version remains.
- **Forbidden fake states:** Auto-start from project content; viewing a manifest running setup; installation treated as trust.
- **Field-level checks:** executable source/host, trust state, project vs machine scope, start action, version, prior valid config.

## A24 — Hooks and verdict

- **Source:** p.175.
- **Preconditions:** A hook with a deadline, output caps and failure policy; timeout/nonzero/malformed test cases.
- **Steps:** Run the test for each case; inspect verdict, exit state and output.
- **Expected visible outcomes:** Deadline/nonzero/malformed output differ; partial stdout after timeout cannot authorize a tool; the audit artifact is bounded and cites the spec version.
- **Forbidden fake states:** Timeout shown as Allow; partial output used as verdict; malformed verdict treated as pass; fail-open hidden.
- **Field-level checks:** `HookOutcome` (Verdict/TimedOut/NonZero/Malformed/Cancelled/EffectUnknown), deadline, stdout/stderr caps, failure policy, spec hash.

## A25 — Child budgets

- **Source:** p.175.
- **Preconditions:** A parent budget; multiple children; a saturated envelope.
- **Steps:** Spawn children; reserve child budgets; attempt to exceed the parent envelope; inspect ownership contention.
- **Expected visible outcomes:** All child reservations share the parent envelope; saturation is bounded and visible; ownership contention is shown; no child gets an independent unlimited wallet.
- **Forbidden fake states:** Unbounded child budgets; parent budget silently exceeded; contention hidden.
- **Field-level checks:** parent cap, child reservation, open/settled/uncertain, ownership, queue bound, refusal reason.

## A26 — Steer bounds

- **Source:** p.175.
- **Preconditions:** A live child; empty and >500-character steer inputs; a stale child control.
- **Steps:** Send empty, 500-character and 501-character steer; use a stale control.
- **Expected visible outcomes:** Empty and >500-character steer are rejected without truncation; the 500-character input is accepted; stale child controls are fenced; the delivery point is stated.
- **Forbidden fake states:** Silent truncation; stale control acting on a new generation; steer shown delivered without acknowledgement.
- **Field-level checks:** steer length/character count, child session/generation, delivery point, control ack.

## A27 — Owned process

- **Source:** p.175.
- **Preconditions:** A stale PID; a live owned process; separate task and session controls.
- **Steps:** Attempt input/kill with a stale PID; inspect process stop, task stop and session abort.
- **Expected visible outcomes:** A stale PID cannot authorize input/kill; process stop, task stop and session abort remain separate; a closed tab does not stop work.
- **Forbidden fake states:** Killing by PID without a live owned handle; closing a tab shown as cancellation; process stop shown as session abort.
- **Field-level checks:** process id, handle generation, binding, live flag, owner, control scope, effect receipt.

## A28 — Unknown effect

- **Source:** p.175.
- **Preconditions:** A disconnect/timeout during an external effect.
- **Steps:** Disconnect; inspect effect state; attempt replay.
- **Expected visible outcomes:** Disconnect/timeout does not mark an external effect cancelled or offer blind automatic replay; unknown state persists until reconciliation; the UI offers a safe reconcile action.
- **Forbidden fake states:** Cancelled shown on disconnect; blind retry button; effect assumed rolled back.
- **Field-level checks:** `effect_status` (unknown), operation id, admission receipt, retry eligibility, reconciliation result.

## A29 — Admission idempotence

- **Source:** p.175.
- **Preconditions:** A submission UUID and captured body; a repeated request; an altered body.
- **Steps:** Submit the same UUID/body twice; submit the same UUID with an altered body.
- **Expected visible outcomes:** Same UUID + same captured body recovers the receipt; altered body conflicts and needs a new request; no duplicate work.
- **Forbidden fake states:** Two runs from one UUID/body; altered body accepted; receipt not returned on retry.
- **Field-level checks:** `submission_id`, body hash, receipt `run_id`, `Admission` (`New` / `Existing` / `Conflict`).

## A30 — Budget exactness

- **Source:** p.175.
- **Preconditions:** USD input and token caps; negative, fractional and overflow cases.
- **Steps:** Enter `5.00`; enter negative/fractional/overflow values; inspect the ledger.
- **Expected visible outcomes:** USD 5.00 becomes exactly 5,000,000 micro-USD; negatives/fractions/overflow fail token/micro-unit input; no floating-point rounding.
- **Forbidden fake states:** 4.999999 from float; negative accepted; overflow wrapped; unknown price treated as zero.
- **Field-level checks:** `budgetCostMicro` exact integer, token cap integer, parse errors, ledger reservation.

## A31 — Unknown pricing

- **Source:** p.176.
- **Preconditions:** A provider/model with no price/quota; failed/cancelled attempted spend.
- **Steps:** Inspect cost/limit; run a cancelled/failed attempt; inspect the ledger.
- **Expected visible outcomes:** Missing rate/quota stays Unavailable; cancelled/failed attempted spend remains in eligible accounting; provider quota, local estimate and observed charge are separate.
- **Forbidden fake states:** Missing rate shown as 0; cancelled work omitted; local estimate labelled provider charge.
- **Field-level checks:** list/declared/observed price, `unknown` state, quota, eligible attempted spend, settled/open/uncertain.

## A32 — Retry policy

- **Source:** p.176.
- **Preconditions:** `max_attempts = 3`; a 429/5xx; an effectful request with durable journal state.
- **Steps:** Trigger retries; inspect attempt count and backoff; inspect replay behavior.
- **Expected visible outcomes:** 3 total attempts is not 3 retries; range/backoff validation holds; effect-state replay rules hold; Stop remains available.
- **Forbidden fake states:** Off-by-one retry count; replaying an effectful request after durable journaling; guessed reset time.
- **Field-level checks:** `max_attempts`, base/max delay, jitter, retry class, attempt count, effect state, countdown.

## A33 — Checkpoint coverage

- **Source:** p.176.
- **Preconditions:** A hash-only checkpoint without after-content blobs; a content-aware checkpoint; uncovered shell/network/SCM effects.
- **Steps:** Open the checkpoint timeline; attempt redo/diff; inspect restore coverage.
- **Expected visible outcomes:** Missing after blobs disable redo/diff; uncovered effects are named before restore; the supported scope is explicit; hash-only entries do not promise universal undo.
- **Forbidden fake states:** Redo enabled without blobs; restore claiming coverage of external effects; hash-only entry shown as content-complete.
- **Field-level checks:** before/after hash, `target_content_ref`, coverage, uncovered effects, redo/diff availability.

## A34 — Manual-edit CAS

- **Source:** p.176.
- **Preconditions:** A candidate with restore/integration preconditions; an unrelated manual edit; a changed expected hash.
- **Steps:** Edit a file after preview; attempt restore/integration; inspect the conflict.
- **Expected visible outcomes:** Restore/integration refuses changed preconditions; unrelated user edits and prior artifacts are preserved; the conflict offers rebase/compare.
- **Forbidden fake states:** Overwriting manual edits; silently rebasing; integrating a stale candidate.
- **Field-level checks:** `expected_current_hash`, observed hash, target hash, conflict result, preserved edits, prior artifact refs.

## A35 — Engineering completion

- **Source:** p.176.
- **Preconditions:** A completed chat; missing required acceptance/claims/matching landing/finish.
- **Steps:** Inspect S32; attempt finish; inspect each blocker.
- **Expected visible outcomes:** Missing required acceptance/claims/matching landing/finish remains Pending despite Completed chat; each blocker is named; the outcome does not turn green.
- **Forbidden fake states:** Chat completion shown as verified; empty required set as success; finish receipt assumed.
- **Field-level checks:** required total/passed, claims complete, verified candidate, landed candidate, finish receipt match, engineering outcome.

## A36 — VCSC denominator

- **Source:** p.176.
- **Preconditions:** Attempted spend and verified landed changes; a zero-verified-denominator cohort.
- **Steps:** Open S27; inspect VCSC, cohort and period; select a zero-denominator cohort.
- **Expected visible outcomes:** VCSC = all eligible attempted spend / verified landed changes; zero verified denominator is Unavailable, not zero; the denominator/cohort/period are explicit.
- **Forbidden fake states:** Zero for unavailable; successful-run cost mislabeled VCSC; denominator hidden.
- **Field-level checks:** attempted spend numerator, verified landed changes denominator, VCSC, cohort, period, measurement state.

## A37 — Atomic settings

- **Source:** p.176.
- **Preconditions:** A settings draft with one invalid field; a concurrent revision conflict.
- **Steps:** Apply the invalid patch; apply against a stale revision; use Reset and Restore defaults.
- **Expected visible outcomes:** Invalid patch writes nothing; revision conflict retains the draft and opens compare/rebase; Reset restores applied values; Restore defaults stages removal until Apply; no partial persistence.
- **Forbidden fake states:** Partial save; toast shown as applied; Reset applying immediately; Defaults pretending to apply.
- **Field-level checks:** expected revision, saved revision, runtime revision, field states (`Effective`/`NextCall`/`NextTask`/`NewSession`/`RestartRequired`/`Failed`), draft.

## A38 — Session/host fencing

- **Source:** p.176.
- **Preconditions:** Two sessions/worktrees/daemon generations; late callbacks and old handles.
- **Steps:** Switch session/worktree/host; deliver a late callback from the old binding; use an old handle.
- **Expected visible outcomes:** Late callbacks and old handles never populate or control the newly selected session/worktree/daemon; drafts are preserved; stale state is marked; controls are fenced.
- **Forbidden fake states:** Old callback rendering in the new session; old handle controlling new work; merged events across sessions.
- **Field-level checks:** binding (`daemon`, `workspace`, `worktree`, `session`, `generation`), request epoch, callback rejection, draft isolation, handle live/generation.

## A39 — Accessible compact view

- **Source:** p.176.
- **Preconditions:** 240 px panel, 200% text, keyboard-only, IME, high contrast, screen reader.
- **Steps:** Exercise every essential control; use Tab/Shift+Tab; type with IME; inspect names/focus/contrast.
- **Expected visible outcomes:** 240 px + 200% text, keyboard, IME, high contrast and screen reader retain all essential controls; focus is visible; status is not color-only; Send/Stop and attention remain visible.
- **Forbidden fake states:** Hidden critical status behind an unlabeled icon; clipped labels; tooltip-only errors; keyboard trap.
- **Field-level checks:** accessible name/role/state, focus order, selected state, error announcement, contrast tokens, wrap/stack behavior at 240/320/480.

## A40 — Long stream and full search

- **Source:** p.176.
- **Preconditions:** A long-running stream; unloaded history; bounded terminal/log buffers.
- **Steps:** Run a 10k-token stream at high rate; scroll up; use Find; inspect terminal/log buffers.
- **Expected visible outcomes:** Windowing/coalescing preserve event order and scroll; Find reaches unloaded history; logs stay bounded and link to full artifacts; no auto-scroll after the user scrolls up.
- **Forbidden fake states:** Dropped durable events; Find only searching rendered rows; unbounded logs in chat/context; follow-tail stealing user scroll.
- **Field-level checks:** event seq order, cursor resume, rendered window, scroll anchor, Find cursor/page, buffer caps, artifact ref.

---

# CI, dogfooding and testing expectations

## Acceptance harness (pp.024–025, 036–037, 176)

- Both hosts must pass the **same behavioral fixtures** for the delivered slice (p.024). Shared semantics belong in a common state/protocol contract; host presentation is adapter work (p.006).
- Every control row requires default, field validation, transactional apply, apply timing, ownership fence, unavailable/loading/error/stale state and recovery checks (p.176).
- Add focused pure-core validation/state-machine tests, adapter contract fixtures and host integration/screenshot cases for the delivered behavior (p.037).
- Reuse native code/tests; read current `AGENTS.md` and repository contribution commands before editing; extend typed DTOs/config/owners rather than duplicating policy (p.037).
- A mock must never become the production source of model usage, permissions, saved settings or completion (p.025).

## Screenshot and visual baseline policy (pp.023, 036)

- Review every screen with empty/large/invalid/stale data at **240/320/480/800 px**, 200% text, long names, keyboard/IME, both host themes/high contrast and a running stream.
- Capture reference snapshots at fixed fixtures; reject overlap, hidden actions, off-grid helper text or mismatched state before merge (p.036).
- The current repository already implements this gate:
  - VS Code: `apps/vscode/scripts/render-webview.mjs` renders the deterministic snapshot at 240/320/480/800 and checks focus/pointer suppression; baseline screenshots live under `apps/vscode/scripts/baselines/screenshots/`.
  - JetBrains: `apps/jetbrains/compile-and-smoke.sh` runs the offscreen render + component-tree digest against pinned linux/macos/windows records in `apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json`; `--write-baselines` re-pins deliberately. A missing platform record is `not_certified`, not a pass.
- The PDF does not prescribe a specific screenshot filename policy; the repository's pinned visual-baseline file is the current authority. Do not weaken it to accommodate a new screen.

## Worktree, dogfood and host flow

- The PDF does **not** define a separate "dogfood worktree" script or named dogfood command. The closest normative requirements are:
  - session/worktree/daemon generation binding and per-session drafts (D-15, A-11, W16, pp.019–021, 031);
  - worktree/edit ownership and isolated candidate mutation (A-11, p.036; current `tasks.mutation_mode = shadow`, `crates/cli/src/config/tasks.rs:21`);
  - both hosts sharing fixtures and the static authority gate (`tests/static-authority`, `AGENTS.md`);
  - implementation waves and "definition of ready" (p.024): both hosts pass the same behavioral fixtures; required controls have keyboard access, help, defaults, validation, source, apply timing, unavailable/loading/error/stale states and safe recovery.
- Current CI commands (`AGENTS.md`): `cargo fmt --check`, `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo run -p faktor-cli -- doctor --data-dir /tmp/faktor-ci`.
- Current host harnesses: VS Code `node apps/vscode/scripts/selftest.mjs` (native DTOs, stream resume, state, fence, webview, money, protocol rejection) and JetBrains `apps/jetbrains/compile-and-smoke.sh` (Backend, NativeBridge, Frontend, ControlPlaneCredential, JetBrainsParity, HostMatrix, EdtWork) plus `apps/jetbrains/ide-journey.sh`.
- **Dogfood gap:** no repository script currently drives a full W01–W16 dogfood pass across both hosts. This must be added in the final wave (see `backlog.md`); the PDF requires real effects and acknowledgement, not demo data.

## Performance expectations (pp.023, 036)

- 240–319 px: controls become a labelled vertical stack; Send/Stop and attention remain visible. 320–479 px: two compact rows wrap chips. ≥480 px: full configuration summary.
- Keep keyed windowed transcript rendering, anchored scroll, follow-tail state, paged older content and search beyond rendered rows.
- Coalesce high-frequency paint around host frame scheduling while preserving durable event order; cap terminal/log buffers and link artifacts; do not invent performance certifications (p.023).
- Current evidence: `apps/jetbrains/UI_PERF_AUDIT.md` records the EDT harness at 10k tokens/200 deltas/s, 500-message transcript, 1k rows, bounded queues and no fabricated rows; VS Code `refreshScheduler.ts` coalesces paint and `selftest.mjs` covers stream/state behavior.
- The PDF does not set numeric latency/SLO certification; any such number must come from a real product test, not this design document.

---

# F01–F40 evidence-reference index

The PDF distinguishes **F ids** (current-source evidence references, pp.177–180) from **A ids** (the 40 acceptance scenarios, pp.173–176). The F index is reproduced here so every control's `feature_refs` can be traced. File:line values are the PDF's pinned current-source references; they are not claims that the proposed behavior is implemented.

| ID | Reference | Pinned source (PDF pp.177–180) |
|---|---|---|
| F01 | VS Code settings and commands | `apps/vscode/package.json:117` |
| F02 | VS Code complete UI declarations | `apps/vscode/src/webview.ts:294` |
| F03 | VS Code native change review | `apps/vscode/src/changeReview.ts:33` |
| F04 | JetBrains task/ask, finish and work surface | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt:83` |
| F05 | JetBrains application-level settings | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorSettingsModel.kt:45` |
| F06 | JetBrains provider/model selection | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/SettingsPanel.kt:25` |
| F07 | Native API and current feature boundary | `docs/native-protocol.md:129` |
| F08 | Provider configuration and endpoint policy | `crates/cli/src/config/provider.rs:31` |
| F09 | Model capabilities, reasoning and pricing authority | `crates/core/src/model.rs:30` |
| F10 | Production efficiency flags | `crates/cli/src/config/efficiency.rs:74` |
| F11 | Core config, compaction and retries | `crates/cli/src/config/core.rs:10` |
| F12 | Current MCP configuration and limits | `crates/cli/src/config/mcp.rs:7` |
| F13 | Hooks and lifecycle contract | `crates/hooks/src/lib.rs:34` |
| F14 | Content-aware checkpoint boundary | `crates/session/src/checkpoints.rs:18` |
| F15 | Session-owned permission replies | `crates/server/src/permission.rs:58` |
| F16 | Verification policy and optional provider | `crates/cli/src/config/verification.rs:12` |
| F17 | Isolated mutation policy | `crates/cli/src/config/tasks.rs:21` |
| F18 | Semantic embedding configuration | `crates/cli/src/config/embedding.rs:14` |
| F19 | IDE usage and financial state validation | `apps/vscode/src/nativeClient.ts:2682` |
| F20 | Agent steering and bound | `apps/vscode/src/steer.ts:8` |
| F21 | IDE credentials and endpoint scope | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ControlPlaneCredentials.kt:18` |
| F22 | Optional enterprise settings | `crates/cli/src/config/enterprise.rs:27` |
| F23 | Typed attachments and validation | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/AttachmentsPanel.kt:742` |
| F24 | Durable title-filtered history | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/HistoryPanel.kt:73` |
| F25 | Session-switch ownership fencing | `apps/vscode/src/lifecycleFence.ts:9` |
| F26 | Bounded native terminal view | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TerminalPanel.kt:30` |
| F27 | Context allocation math | `crates/context/src/budget.rs:9` |
| F28 | Agents and contextual controls | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/AgentsPanel.kt:22` |
| F29 | Completion configuration | `crates/cli/src/config/completion.rs:21` |
| F30 | Platform settings integration | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorConfigurable.kt:58` |
| F31 | Shell and network policy | `crates/cli/src/config/sandbox.rs:49` |
| F32 | Streaming and rendering scheduler | `apps/vscode/src/refreshScheduler.ts:13` |
| F33 | IDE source authority and architectural constraints | `AGENTS.md:6` |
| F34 | Run admission and captured draft | `apps/vscode/src/taskStart.ts:37` |
| F35 | Updater configuration | `crates/cli/src/config/updater.rs:38` |
| F36 | Optional worker placement | `crates/cli/src/config/workers.rs:35` |
| F37 | Exact provider-header limits and secrecy | `crates/provider/src/config.rs:28` |
| F38 | Current client methods and strict DTOs | `apps/vscode/src/nativeClient.ts:4168` |
| F39 | Current native Kotlin protocol client | `apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt:2` |
| F40 | Current engineering-outcome presentation | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt:119` |

The PDF also defines B01–B29 as competitor/reference anchors (pp.181–183). They are cited per control in `controls.json` (`feature_refs`) but are not acceptance ids.
