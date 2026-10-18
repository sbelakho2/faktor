# Edition 5 end-to-end workflows (W01–W16)

Source: `Faktor_IDE_UI_UX_2.pdf`, "End-to-end workflows" pp.014–021. All workflows are normative design targets: reuse existing operations where supported, and build missing native/backend contracts before enabling their effects. Every flow includes capture, effect acknowledgement and recovery, not only the successful path (p.014).

Status vocabulary used below is the PDF's: Draft → Validating → Admitted → Queued/Active/Waiting → Stopping → Cancelled/Failed/Effect unknown, with engineering outcome Verified/Unverified/Pending kept independent of conversation state (p.012). "Completed" chat text cannot resolve missing evidence.

---

## W01 — Connect and validate a custom provider

**Screen path:** S03 → S02 → S05 (p.014)  
**Entry:** Providers category → Add connection, or an existing connection's Edit/Test action.  
**Preconditions:** A daemon host is known; the operator can reach the endpoint; a credential reference can be bound at the machine boundary; no active task depends on the prior connection remaining unchanged.

**Steps.**
1. Open the provider connection editor (S03). Choose the supported API family and execution host.
2. Enter the endpoint and an environment/vault reference. Never type a secret value into shared config.
3. Test reachability, authentication, model catalog and the exact model capability separately. Each stage has a bounded deadline and its own result (p.028).
4. Inspect the capability sheet: context, output, tools, parallel tools, reasoning, thinking, vision, documents, streaming, structured output, authority and cost/limit provenance.
5. Save the staged draft only after the secure binding is acknowledged. The save is one validated patch; a failure writes nothing.
6. Pick the model in S02 and tune only adapter-offered parameters in S05. Unsupported parameters must be rejected before dispatch.

**State transitions.** Staged draft → validated draft → saved revision (saved-but-not-applied is first-class) → runtime acknowledgement (effective) → model selection committed. A probe is an observation, not a benchmark or a release claim.

**Failure paths.**
- 401: offer Rotate credential; keep the staged form and the last valid connection.
- Blocked destination: show endpoint policy; do not render a bypass.
- Discovery failure: keep manually declared model IDs; do not falsely validate tool calling.
- Unknown capability: keep gated tools/vision/schema disabled; manual labels never certify support.
- Save conflict: keep the draft and open compare/rebase; never overwrite another editor's changes silently.

**Expected observable results.** The provider/model pair is committed only after acknowledgement; the connection's instance ID, family, endpoint and credential reference are visible; the prior default is preserved on failure; the exact model's capability authority and unavailable reasons are visible; no secret value appears in shared config, snapshots, logs or exports.

**Forbidden fake states.** Fake "Connected" from config save; fake probe success; fake capability authority from a model marketing name; fake effective state from a green toast; secret values in project files.  
**Complete when:** provider/model pair and supported normalized tuning are captured only after acknowledgement (p.014).  
**Traceability:** p.014; controls P-01..P-06, P-10..P-14, M-01..M-09, G-01..G-14; acceptance A01, A02, A03, A06.

---

## W02 — Set up a local model without false locality

**Screen path:** S04 → S31 → S08 (p.014)  
**Entry:** Providers → Local runtime, or the local model setup route; the daemon host and execution location are shown first.

**Steps.**
1. Inspect the daemon host and the meaning of loopback (local to the daemon, not necessarily the laptop).
2. Choose the configured local adapter (for example Ollama-compatible) and test the exact endpoint from the daemon host.
3. Compare declared and observed context allocation; show the runtime observation source, never infer from the model name.
4. Review the data boundary before configuring an embedder. An absent embedder keeps lexical/symbol retrieval.
5. Choose the retrieval fallback and capture a context preview in S08.
6. Offer allocation/keep-alive/unload controls only where the adapter owns and can acknowledge them.

**State transitions.** Draft local route → validated adapter → endpoint observed → allocation observed/unavailable → configured route → captured task preview. Observed usage remains unavailable until a durable read exists.

**Failure paths.**
- Private LAN endpoint: typed refusal under current provider policy; `allow_loopback` is not a bypass.
- Loopback on a remote host: a laptop-local URL is not the daemon loopback.
- Unsupported allocation/unload: controls stay unavailable with the missing adapter capability.
- Embedding failure: stay on lexical/symbol fallback; never fabricate semantic retrieval.
- Endpoint change after a task capture: invalidate the captured preview; do not rewrite the in-flight call.

**Expected observable results.** A ready local route is explicit; the daemon host, endpoint, loopback policy, declared/observed allocation and embedding boundary are visible; no remote model is silently substituted; unsupported runtime controls are disabled with reasons.

**Forbidden fake states.** "Local" inferred from a URL on the laptop; observed allocation fabricated from the model name; a private-LAN route shown as enabled; semantic index claimed without an embedder.  
**Complete when:** a ready local route is explicit and observed usage remains unavailable until a durable read exists (p.014).  
**Traceability:** p.014; controls P-07..P-09, P-13, P-15, C-09..C-11, M-09; acceptance A07, A08, A11.

---

## W03 — Tune a model and change it mid-session

**Screen path:** S02 → S05 → S01 (p.015)  
**Entry:** Model picker in Work, or Model tuning in Settings.

**Steps.**
1. Open the model picker; inspect capability authority, availability and limits.
2. Choose the exact provider/model pair.
3. Review reasoning and generation compatibility; remove invalid overrides.
4. Read the apply timing: Next task or New conversation. Preserve the current draft.
5. Submit a new captured request. The executed call records its actual route and effective parameters.
6. If the change requires a new session, offer Create new conversation with the draft preserved.

**State transitions.** Draft preference → staged tuning → validated bundle → captured at admission → executed call receipt. A running call is never rewritten; the displayed preference is not the executed model.

**Failure paths.**
- Incompatible tuning: stays staged; Apply is disabled with the exact incompatibility.
- Unavailable model: remains visible with a reason; never silently becomes another endpoint.
- Mid-session change unsupported: offer a new session; explain when the new value actually applies.
- Unsupported parameter: reject before dispatch; strict DTOs must not receive unknown fields.
- Model change after a call was admitted: the in-flight request and its historical receipt are unchanged.

**Expected observable results.** Each executed call records actual route and effective parameters separately from the current draft preference; capability authority and unavailable parameters are visible; the draft survives; apply timing is explicit.

**Forbidden fake states.** Showing the picker's model as the executed model; rewriting an in-flight call; treating a reasoning enum as a writable API; silently dropping an unsupported parameter.  
**Complete when:** each executed call records actual route and effective parameters, separately from current draft preference (p.015).  
**Traceability:** p.015; controls M-01, M-02, M-05..M-12, G-01..G-14; acceptance A02, A03, A05.

---

## W04 — Move from Ask to Plan to Code

**Screen path:** S06 → S07 → S01 (p.015)  
**Entry:** Agent preset picker / Ask in Work, then Plan, then Code.

**Steps.**
1. Choose enforced Ask. Explore read-only; the offered tools must be filtered and effectful writes denied at dispatch.
2. Create a Plan task with a named artifact scope. The plan file and review comments are the artifact.
3. Review the frozen plan and comments. Approval does not broaden tool policy.
4. Start a new Code task with an explicit contract, permissions, model and finish level.
5. The Code admission binds the approved plan version and execution envelope.

**State transitions.** Ask draft → admitted Ask (read-only envelope) → Plan task → frozen plan version → approved plan → new Code admission. The preset, model, permission policy and finish level remain independently inspectable.

**Failure paths.**
- Prompt-only Ask: label it as such until read-only dispatch enforcement exists; do not call it read-only.
- Plan approval: does not grant writes or widen managed denies.
- New Code admission: refuses if the plan version is stale or the envelope is invalid.
- Preset conflict: invalid capability/model combinations stay in the draft and cannot be applied.

**Expected observable results.** Ask cannot dispatch a write/effectful tool even if prompt text requests it; Plan produces a named, versioned artifact; Code admission cites the plan version and explicit contract; preset ≠ model ≠ permission policy ≠ finish level.

**Forbidden fake states.** Calling a plain prompt "read-only"; approving a plan and silently broadening authority; treating a preset label as a permission grant.  
**Complete when:** a new Code admission binds the approved plan version and execution envelope (p.015).  
**Traceability:** p.015; controls A-01..A-12; acceptance A09, A10.

---

## W05 — Assemble context and resolve overflow

**Screen path:** S09 → S08 → S10 (p.016)  
**Entry:** Composer `+ Context` / mention picker; the context inspector opens for the draft.

**Steps.**
1. Mention exact sources (files, selections, diagnostics, terminal excerpts, diffs, URLs, prior chats).
2. Inspect URI, range, version, byte size and inclusion reason for each source.
3. Attach supported files; review automatic chips and remove anything not intended.
4. Pin required facts; compare estimated input and reserves against capacity.
5. Remove, compact or choose a capable model before capture.
6. Admission captures a versioned source manifest and the capacity calculation.

**State transitions.** Draft source set → validated attachments → preview manifest → capacity check → captured manifest at admission. Failed/oversize attachments remain visible until removed or retried.

**Failure paths.**
- Oversize/unsupported/stale attachment: visible with reason and Remove/Retry; Send is blocked for invalid admission.
- Missing durable usage: measured usage reads Unavailable; estimates are labelled with method and basis.
- Overflow: offer remove, shorten, compact or a capable model; never silently truncate criteria or pending effects.
- Session/worktree switch: source identity is preserved; a reference resolves in its own worktree, never a global current directory.

**Expected observable results.** The admitted request has a versioned source manifest and a valid capacity calculation; every inclusion has a source URI/range/version and reason; automatic inclusions are individually removable before capture.

**Forbidden fake states.** A filename or tab title shown as attached content; a composed context number labelled measured; silent omission of a failed attachment; hidden truncation of criteria.  
**Complete when:** the admitted request has a versioned source manifest and valid capacity calculation (p.016).  
**Traceability:** p.016; controls C-01..C-14; acceptance A11, A12, A13, A14.

---

## W06 — Compact and inspect continuity

**Screen path:** S10 → S08 → S32 (p.016)  
**Entry:** Context inspector → compaction plan, or the automatic threshold's Review action.

**Steps.**
1. Open the compaction plan. Inspect retained facts: contract version, candidate identity, policy version, pending operations and evidence refs.
2. Inspect the summary model route, egress and cost implications.
3. Request a new durable compact operation with an expected context/task revision.
4. Preserve pending approvals and unknown external-effect state through compaction.
5. After commit, inspect before/after basis, measured reduction and the new summary version.
6. Keep required contract, candidate identity and unresolved obligations reachable and active.

**State transitions.** Prior valid basis → compaction plan → owned operation → Applied (new revision + measured result) / Refused / Cancelled / Failed / Effect unknown. Failure or insufficient reduction leaves the prior basis.

**Failure paths.**
- Failure or minimal reduction: do not advance success; keep the last valid state.
- Pending approval/unknown effect: cannot be erased by summarization.
- Cancelled operation: prior basis remains; no partial commit.
- Missing durable usage: mark estimates; do not claim measured reduction.
- Re-expanding UI text: not compaction and not deletion.

**Expected observable results.** A successful compaction achieves the configured minimum reduction and records a measured result; retained facts, summary version and stale-evidence flags are visible; pending obligations survive.

**Forbidden fake states.** A compacted transcript view shown as runtime context reduction; a 1% summary accepted as success; deletion implied by compaction; measured reduction fabricated from an estimate.  
**Complete when:** required contract, candidate identity and unresolved obligations remain reachable and active (p.016).  
**Traceability:** p.016; controls K-01..K-04, C-12, I-11, V-07; acceptance A15.

---

## W07 — Approve once or create a narrow rule

**Screen path:** S14 → S13 → S22 (p.017)  
**Entry:** Attention card in Work/Inspect/History or the host attention badge.

**Steps.**
1. Open the approval dock. Inspect owner/session/child, generation, normalized tool arguments, cwd, target and effect boundary.
2. Choose Allow once or Deny. The reply addresses one exact waiter generation.
3. Await the exact reply acknowledgement; the card disables duplicate replies.
4. Optionally open a separate persistent-rule draft in S13. Test the match against a sample operation.
5. Apply future policy only after daemon acknowledgement; the rule gets a named scope, version and expiry.

**State transitions.** Permission requested → pending waiter → exact reply → dispatched → confirmed/unknown. A saved future rule never replies to another pending request automatically.

**Failure paths.**
- Expired/already-replied/wrong-session waiter: refuse visibly; offer Refresh.
- Duplicate click: must not create a second effect.
- Remember unavailable: no policy store, so no Remember rule.
- New/changed args: require a new decision; a saved rule cannot bless unrelated pending work.
- Timeout: preserve unknown state until reconciled; no blind replay.

**Expected observable results.** One exact approval resolves once; a persisted rule has named scope, version and expiry; permission requested, allowed, dispatched and confirmed remain distinct; the pending card stays in the attention rail when scrolled away.

**Forbidden fake states.** Persisting Allow once automatically; a green "Allowed" toast without dispatch; a rule shown active without a policy store; duplicate reply generating a second effect.  
**Complete when:** one exact approval resolves once; a persisted rule has named scope, version and expiry (p.017).  
**Traceability:** p.017; controls Q-01..Q-09, Q-13, Q-14, X-12, B-12, D-12; acceptance A17, A18, A19.

---

## W08 — Add MCP with trust and recover auth

**Screen path:** S16 → S15 → S13 (p.017)  
**Entry:** Tools & MCP → Add server, or an existing server's Add/Edit.

**Steps.**
1. Choose a supported transport. Current config is stdio; remote transport is offered only after capability negotiation.
2. Enter a bounded spec: name, command, args, environment references.
3. Review executable source, host, working directory and environment references; review trust.
4. Save; explicitly start the trusted server. A config file alone never starts an executable.
5. Observe health: stopped, connecting, ready, authentication required, failed or stale. Ready requires an observed handshake.
6. Inspect tool namespaces/schemas and set narrow permissions per server/tool.

**State transitions.** Draft spec → validated (bounds) → saved → explicit start → connecting → ready/failed/auth-required → discovered tools → policy-resolved availability.

**Failure paths.**
- Invalid args/start failure: preserve the previous valid server.
- Repository-supplied executable: never auto-start; require trust.
- Auth cancellation: keep the form; not Connected.
- Disconnect: does not prove remote effect cancellation; unknown effects stay visible.
- Remote/OAuth unavailable: label as unsupported, do not render a fake transport.

**Expected observable results.** Only discovered, healthy, enabled and authorized tools are offered to the correct task; server health and namespace are visible; bounds are shown in bytes, not character assumptions; prior valid config survives failure.

**Forbidden fake states.** Config save shown as Connected; Ready without handshake; remote/OAuth shown from stdio config; repository content starting a server; disconnect treated as Cancelled.  
**Complete when:** only discovered, healthy, enabled and authorized tools are offered to the correct task (p.017).  
**Traceability:** p.017; controls T-01..T-15; acceptance A21, A22, A23.

---

## W09 — Install and invoke a skill safely

**Screen path:** S17 → S11 → S13 (p.018)  
**Entry:** Skills & hooks → skill library → Install/Inspect, or a command palette entry.

**Steps.**
1. Inspect the manifest, files, source, version and required capabilities.
2. Preview a version diff.
3. Choose manual or agent-selectable invocation scope.
4. Install a validated version into the trusted store.
5. Resolve template variables and preview the draft and permission envelope.
6. Invoke deliberately; execution still uses effective grants.

**State transitions.** Discovered → inspected (read-only) → validated install → installed/versioned → invoked (manual or agent-selectable). Viewing never runs setup; installation never grants capability.

**Failure paths.**
- Failed update: retain the known-good version.
- Unavailable dependency: keep the previous version; do not mark installed.
- Hidden/disabled skill: cannot remain silently agent-invocable.
- Unresolved template variables: block invocation; keep the draft.
- Missing required capability: refuse invocation; do not widen grants from instruction text.

**Expected observable results.** Versioned instructions are attributed; source/version/invocation policy/required capabilities are visible; installation is distinct from permission; invocation uses the effective envelope.

**Forbidden fake states.** Viewing a package executing setup; installation shown as authorization; hidden skill still invocable; instruction text widening grants.  
**Complete when:** versioned instructions are attributed and execution still uses effective grants (p.018).  
**Traceability:** p.018; controls S-01..S-06, S-14, I-06; acceptance A10, A23.

---

## W10 — Author and test a lifecycle hook

**Screen path:** S18 → S30 → S22 (p.018)  
**Entry:** Skills & hooks → Hooks → New/Edit.

**Steps.**
1. Choose a native event from the 17-value HookEvent set. Do not copy foreign event names.
2. Configure executable/argv, allowlisted environment, deadline, stdout/stderr caps and failure policy.
3. Preview the normalized input.
4. Admit a bounded test under the real capability/deadline envelope; no production event is emitted merely to preview input.
5. Inspect verdict, exit state and bounded output.
6. Save only a validated version for future events; cite the exact spec version, event, envelope and audit artifact.

**State transitions.** Draft hook → validated spec → explicit test admission → test outcome (Allow/Deny/Warn/metadata, TimedOut, NonZero, Malformed, Cancelled, EffectUnknown) → saved version for future events.

**Failure paths.**
- Timeout: owned process tree is stopped; partial stdout cannot authorize an Allow verdict.
- Nonzero exit, malformed verdict and timeout are distinct outcomes.
- FailOpen is an explicit policy choice, never an error swallowed by the UI.
- Invalid matcher: richer matching is a new schema feature, not a silently ignored field.
- Test with missing capability: refused before any child is spawned.

**Expected observable results.** A hook run cites exact spec version, event, envelope and bounded audit artifact; test results are distinct; saving does not auto-enable execution without the validated version.

**Forbidden fake states.** Partial output after timeout shown as Allow; test run emitting a real production lifecycle event; malformed verdict treated as pass; fail-open hidden.  
**Complete when:** a hook run cites exact spec version, event, envelope and bounded audit artifact (p.018).  
**Traceability:** p.018; controls S-07..S-14; acceptance A24.

---

## W11 — Delegate and steer a bounded child

**Screen path:** S07 → S23 → S14 (p.019)  
**Entry:** Agents tree / custom agent preset → delegate, or an active parent starts a child.

**Steps.**
1. Admit a role within the parent envelope; show concurrency and budget reservation.
2. Bind candidate ownership (worktree/edit ownership) explicitly.
3. Inspect the child's actual model, role and status.
4. Send a ≤500-character steer or use a supported model control.
5. Resolve a child-scoped approval in the approval dock.
6. Inspect the typed handoff. Child output is evidence, not automatic parent success.

**State transitions.** Parent admitted → child role admitted within parent budget → child queued/active/waiting → steer/pause/model/stop → child completed/failed → handoff evidence; parent obligations remain.

**Failure paths.**
- Budget saturation: bounded queue or typed refusal; never unbounded parallel wallets.
- Ownership contention: show owner, conflict and handling; no silent overwrite.
- Stale child controls: fenced by session/generation; refuse.
- Empty or >500-character steer: reject without truncation.
- Child failure: preserve artifacts and parent remaining obligations.

**Expected observable results.** Shared budget and ownership hold; child actual model/status and handoff are visible; the steer bound is enforced; a child saying done does not establish parent verification.

**Forbidden fake states.** Child "done" shown as parent verified; unbounded child wallets; stale controls acting on a new generation; steer silently truncated.  
**Complete when:** shared budget and ownership hold; a child saying done does not establish parent verification (p.019).  
**Traceability:** p.019; controls A-06..A-12, M-11, B-04, Q-06; acceptance A25, A26.

---

## W12 — Queue, background and interrupt work

**Screen path:** S22 → S19 → S26 (p.019)  
**Entry:** Live run (S22) while a task is active; terminal/background detail in S19; conversation switch in S26.

**Steps.**
1. While a task runs, choose Queue next rather than implicit interrupt.
2. Edit or remove the queued item before admission.
3. Inspect the owned background process and bounded output.
4. Deliberately stop the correct process/task/session; these are separate scopes.
5. Switch sessions with per-session drafts intact.
6. Lost stream marks the projection stale; reconnect from the durable cursor before enabling duplicate effects.

**State transitions.** Active → queued next → admitted next; process foreground/background → owned stop → accepted → settled/refused/unknown. A closed tab is host UI state only.

**Failure paths.**
- Queue saturation: bounded; typed refusal.
- Stop/revoke: does not undo already-issued external effects.
- Unknown effect or admission: reconcile before replay.
- Stream loss: stale, not idle.
- Session switch: late callbacks cannot populate the new session.

**Expected observable results.** Each queue item/process/control retains owner, generation and durable result; stop scope is visible (process vs task vs session); drafts survive switching; stale state is explicit.

**Forbidden fake states.** Closed tab shown as cancellation; lost stream shown as idle; stop shown as confirmed remote rollback; queue item silently discarded.  
**Complete when:** each queue item/process/control retains owner, generation and durable result (p.019).  
**Traceability:** p.019; controls D-05, D-07, D-08, D-12, D-13, X-01..X-05, B-10; acceptance A27, A28, A38.

---

## W13 — Handle rate limit, cap and unknown spend

**Screen path:** S21 → S22 → S27 (p.020)  
**Entry:** Budgets & reliability, or a typed rate-limit/cap event in the live run.

**Steps.**
1. Inspect the typed 429/quota/local-saturation cause.
2. Show known retry delay and total attempts; keep Stop available.
3. Inspect the ledger: open, settled and uncertain.
4. Request a supported allowance revision or end the task.
5. Reconcile any unknown admission before any replay.
6. Keep every eligible attempted charge, including failed/cancelled work.

**State transitions.** Active → rate-limited/retry-delay → retry/stop → settled; cap warning → admission refusal; unknown spend → uncertain → reconciled. A cap reduction cannot erase spend or reservations.

**Failure paths.**
- Unknown reset time: do not guess.
- Unknown price: unavailable, not zero.
- Cap reduction: cannot erase settled/open/uncertain obligations.
- Effectful replay: only after proven safety; otherwise keep unknown.
- Cancelled/failed work: remains in eligible spend.

**Expected observable results.** The final ledger retains every eligible attempted charge; retry delay/attempt count and stop are visible; unknown spend is a named state; provider quota, local estimate and observed charge are separate.

**Forbidden fake states.** Guessed reset time; zero for unknown price; cap decrease shown as deleting spend; cancelled work omitted from the eligible numerator.  
**Complete when:** the final ledger retains every eligible attempted charge, including failed/cancelled work (p.020).  
**Traceability:** p.020; controls B-01..B-12, O-06, M-13, K-12; acceptance A30, A31, A32.

---

## W14 — Review, restore or fork without overwriting edits

**Screen path:** S24 → S25 → S26 (p.020)  
**Entry:** Inspect → Changes/Review, or History → checkpoint/restore.

**Steps.**
1. Open the exact native candidate diff; review mapped criteria.
2. Choose a recorded checkpoint from the timeline.
3. Inspect content coverage and file preconditions.
4. Choose a supported restore or fork scope.
5. Preview unrelated edits and uncovered effects.
6. Perform a separate transaction if valid; recheck CAS preconditions inside the restore transaction.

**State transitions.** Review (read-only) → restore/fork preview → validated preconditions → separate restore/fork transaction → applied/refused/unknown. Conversation fork and file restore are separate operations.

**Failure paths.**
- Hash-only checkpoint: redo/diff unavailable; disable it.
- Missing content: refuse restore; preserve edits.
- CAS conflict: preserve user edits and prior artifacts; rebase/compare.
- Uncovered shell/network/SCM effects: disclose; do not imply rollback.
- Changed candidate: invalidates stale proof; historical verified artifacts remain historical.

**Expected observable results.** A changed candidate invalidates stale proof; restore refuses changed preconditions; unrelated edits are preserved; coverage and uncovered effects are named; fork lineage is separate from file restore.

**Forbidden fake states.** Universal undo claimed from hashes; restore silently overwriting user edits; fork shown as file rollback; old proof reused for a new tree.  
**Complete when:** a changed candidate invalidates stale proof; historical verified artifacts remain historical (p.020).  
**Traceability:** p.020; controls V-01..V-06, V-12, D-06, Q-09; acceptance A33, A34.

---

## W15 — Finish a change with complete proof

**Screen path:** S01 → S24 → S32 (p.021)  
**Entry:** Task outcome / finish in Work or Inspect.

**Steps.**
1. Capture required criteria and the requested finish level: Local / Commit / Push / PR.
2. Implement in the isolated candidate.
3. Inspect claim artifacts.
4. Run required checks; match proof to the exact candidate identity.
5. Integrate with matching identity.
6. Perform the authorized finish; inspect receipts and outcome.

**State transitions.** Candidate → required checks pending → verified/unverified → landing/integration → finish receipt → engineering outcome. Conversation Completed is independent of engineering outcome.

**Failure paths.**
- Missing canonical SCM: named blocker; no PR claim.
- Failed unit check, empty proof or mismatched landing: pending/unverified.
- Session Completed: insufficient.
- Partial/hunk selection or changed candidate: needs matching new evidence.
- Finish receipt missing: requested finish pending.

**Expected observable results.** Required acceptance, claims, landing equality and requested finish all have explicit matching facts; blockers are named; each evidence receipt opens; the outcome stays Pending until all facts exist.

**Forbidden fake states.** Green finish from chat completion; proof reused across candidate identities; PR created without SCM; empty required set treated as success.  
**Complete when:** required acceptance, claims, landing equality and requested finish all have explicit matching facts (p.021).  
**Traceability:** p.021; controls V-07..V-12, O-06, B-04; acceptance A35, A36.

---

## W16 — Recover a remote host or runtime change

**Screen path:** S31 → S30 → S26 (p.021)  
**Entry:** Remote workspace binding, daemon reconnect, or a runtime/version change event.

**Steps.**
1. Detect the new daemon/workspace generation; mark old handles stale.
2. Preserve local drafts and the prior coherent projection.
3. Show the execution/model/vault map and the data boundary.
4. Verify runtime path, readiness and protocol compatibility.
5. Rebind the session and reconcile operations.
6. Resume only supported work; unknown effects stay visible.

**State transitions.** Old binding → stale handles → new binding → readiness verified → session rebound → operations reconciled → supported work resumes. Old callbacks/controls are refused.

**Failure paths.**
- Laptop-local URL vs daemon loopback: do not conflate.
- Checksum mismatch: refuse start.
- Unrecorded exact path: deliberate trust required.
- Protocol incompatible: refused, not silently adapted.
- Old request replay on new host: forbidden; preserve drafts and unknown effects.
- Credentials: never copied into project files.

**Expected observable results.** All callbacks and controls are fenced to the new binding; unknown effects remain visible; drafts survive; runtime path/readiness/protocol and execution/model/vault locations are explicit.

**Forbidden fake states.** Old handle controlling a new host; local URL shown as daemon loopback; copied credentials; replaying an old effectful request on a new host.  
**Complete when:** all callbacks and controls are fenced to the new binding; unknown effects stay visible (p.021).  
**Traceability:** p.021; controls X-13, P-13, O-02, O-07, O-11, D-01, D-15, A-11; acceptance A08, A28, A38.
