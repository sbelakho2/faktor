# Edition 8 acceptance contract

Source: `Faktor_IDE_UI_UX_3.pdf` (Revision 8, 17 October 2026), extracted with
`pdftotext -layout` to `/tmp/faktor-ui3.txt` (22,249 lines, 449 pages).
Scope of this file: acceptance, test records, field-level verification,
forbidden fake states, and host/platform applicability. It does **not** restate
the control catalog or the screen-by-screen design; those belong to the sibling
edition-8 files.

**Status stated by the document: acceptance is defined, not executed.** Part 3
opens "Status: defined, not yet executed" (p. 182) and 3.6 records "Task flows
in both IDEs ... Not run. Requires implementation builds" (p. 196). Section 4.8
says "These are required integration scenarios; no product-runtime
certification is claimed by this design artifact" (p. 343). Do not convert any
row in this file into a pass without host evidence.

## 1. Count ledger (verified from the extracted text)

| Item | Printed count | Verification |
|---|---:|---|
| Acceptance scenarios `A01`–`A40` | **40** | Section 4.8, p. 343; IDs printed in one sequence |
| End-to-end workflows `W01`–`W16` | **16** | Section 3.2, pp. 183–186 |
| Control contracts | **200** | Section 4.5, pp. 257–294; 15 letter groups |
| Screens with a behaviour contract | **72** | Part 2, pp. 22–180 |
| Host visual contracts | **144** | 72 screens × 2 hosts |
| Reference theme vectors | **288** | 72 screens × 4 host themes (p. 188, retained R7 note) |
| Per-screen interaction fixture classes | **8** | first-read, empty/no-match, ready, refreshing/stale, error/refusal, action pending, acknowledged result, scope switch in flight |
| Restored validation assertions | **28** | Section 4.6, p. 332 (submission 4, recovery 4, proof 3, money 4, approvals 4, host UI 5, delivery claims 4 — sentence-level assertions counted from the printed paragraphs) |

The 40 acceptance IDs are exactly:

```
A01 A02 A03 A04 A05 A06 A07 A08 A09 A10
A11 A12 A13 A14 A15 A16 A17 A18 A19 A20
A21 A22 A23 A24 A25 A26 A27 A28 A29 A30
A31 A32 A33 A34 A35 A36 A37 A38 A39 A40
```

The 16 workflow IDs are exactly:

```
W01 W02 W03 W04 W05 W06 W07 W08 W09 W10 W11 W12 W13 W14 W15 W16
```

There are **no other printed acceptance IDs**. `AC-01`–`AC-03` are sample
acceptance criteria inside product fixtures, not test IDs. `T01`–`T15`,
`D01`–`D15`, etc. are control IDs, not scenarios.

## 2. Reading rules for this file

- **Printed** means the sentence is in section 4.8 (p. 343) verbatim.
- **Derived** means an executable precondition, step or field check inferred
  from the named screen contract and control IDs. Section 4.8 prints only a
  one-sentence assertion for each ID: it prints no steps, no preconditions and
  no field selectors. Every derived item is labelled so it is never mistaken
  for new spec text.
- "Both hosts" means VS Code and JetBrains must each produce the same
  behavioural result; a pass in one host is not a pass.
- Numeric values quoted below are the document's examples. They are not
  universal defaults unless the control contract says so.

## 3. Acceptance scenarios A01–A40

### A01 — Provider/model atomicity
**Printed requirement (4.8, pp. 343–345):** "An unavailable provider/model pair is
rejected; neither default changes and the draft survives."
**Preconditions (derived):** S02 provider/model picker has a configured default
pair; at least one listed pair is unavailable or capability-incompatible; a
non-empty draft exists in S01/S36.
**Steps (derived):** 1. Open S02 from the draft. 2. Choose the unavailable pair
and confirm. 3. Inspect the provider and model fields, the draft text and the
next-call receipt.
**Expected visible / field checks:** S02 `M-01`–`M-03`, `M-06`–`M-09`; the
unavailable pair is offered as unavailable with a typed reason, not silently
mapped; both default fields retain their previous exact values; the draft and
its attachments remain; no "Saved" state appears without an acknowledged
config revision.
**Forbidden fake states:** silent fallback to another endpoint/model; optimistic
"Saved"/"Ready"; clearing the draft as if the change succeeded.
**Applies:** both hosts; S02, S05, S01/S36.

### A02 — Capability authority
**Printed requirement (4.8, pp. 343–345):** "Unknown model authority disables gated
tools/vision/schema; manual labels do not certify support."
**Preconditions (derived):** S05 shows a model whose capability record is
unknown or incomplete; a model-configuration surface permits a manual display
label separate from capability authority; gated tools exist.
**Steps (derived):** 1. Select the unknown-authority model. 2. Open the
capability details and attempt to enable a gated tool/vision/schema. 3. Type a
manual label claiming support. 4. Return to the composer and inspect what is
offered.
**Expected visible / field checks:** S05 `G-01`–`G-14`, S02 `M-07`; unknown
capabilities show "Unknown"/"Not verified" and gated controls stay disabled
with a reason; a manual label changes only the label, never the authority
badge or dispatch; unsupported parameters cannot enter a strict DTO.
**Forbidden fake states:** "Supported" from a typed label; a green capability
chip; enabling a tool because the model name contains a hint.
**Applies:** both hosts; S02, S05, S13.

### A03 — Generation compatibility
**Printed requirement (4.8, pp. 343–345):** "Changing model revalidates all overrides;
unsupported parameters cannot leak into strict DTOs."
**Preconditions (derived):** S05 has non-default overrides (for example
temperature, top-p, thinking budget) staged for model A; model B lacks one or
more of those parameters.
**Steps (derived):** 1. Stage overrides on A. 2. Switch to B in the same form.
3. Inspect each override, then submit a test call. 4. Inspect the effective
request preview and the executed-call receipt.
**Expected visible / field checks:** S05 `G-01`–`G-14`; retained values remain
visible but unsupported ones become invalid/staged with a message; the
effective-request preview lists only supported fields; the receipt records the
actual route and effective parameters separately from the draft preference.
**Forbidden fake states:** dropping an override without saying so; sending an
unknown field to a strict provider; rewriting an in-flight call.
**Applies:** both hosts; S02, S05, S01.

### A04 — Unset versus zero
**Printed requirement (4.8, pp. 343–345):** "Reset removes the tuning override;
temperature 0 and absent temperature remain distinct."
**Preconditions (derived):** S05 has an inherited default and an explicit
override; a reset control exists.
**Steps (derived):** 1. Set temperature to `0`, apply. 2. Capture the effective
request. 3. Reset the override. 4. Capture again. 5. Re-open the form.
**Expected visible / field checks:** S05 `G-04`–`G-08`; temperature `0` renders
as `0`, not blank; reset removes the value and shows the inherited value as a
placeholder ("Model default"), not `0`; the effective request omits the field
after reset and includes `0` before reset; exact values are preserved for the
backend while display uses locale grouping.
**Forbidden fake states:** treating blank as `0`; showing `0` for an absent
value; "Reset" that saves a new zero-valued override.
**Applies:** both hosts; S05.

### A05 — Active-call capture
**Printed requirement (4.8, pp. 343–345):** "Editing effort/model/budget does not rewrite
an in-flight request or its historical receipt."
**Preconditions (derived):** a run is executing or awaiting a provider response;
S01/S33 exposes effort/model/budget; a receipt exists for the active request.
**Steps (derived):** 1. Start a task. 2. While it is in flight, change effort,
model and budget in the draft/options. 3. Inspect the active run and its
receipt. 4. Open the new-task draft.
**Expected visible / field checks:** S01/S33 `B-01`, `B-02`, `G-01`, `M-02`;
the in-flight request keeps its captured values and receipt; the next-task
draft shows the changed preference; the UI distinguishes "next call" from
"this call"; no historical field is mutated.
**Forbidden fake states:** retroactively changing the receipt; showing the new
budget as already applied to the running call; silently cancelling/restarting
the call.
**Applies:** both hosts; S01, S33, S32.

### A06 — Secret storage
**Printed requirement (4.8, pp. 343–345):** "Provider/MCP/hook key values are absent from
shared config, diagnostics, logs and exported bundles."
**Preconditions (derived):** a provider, MCP server and lifecycle hook each
reference a secret through an environment/vault key; a diagnostic export and a
log exist; a packaged extension/bundle can be inspected.
**Steps (derived):** 1. Save each secret reference. 2. Inspect the shared
project config and the effective settings. 3. Open S30 diagnostics, preview and
export. 4. Inspect logs and the exported bundle/artifact.
**Expected visible / field checks:** S03 `P-04`/`P-05`, S16 `T-05`, S18
`S-10`, S30 `O-03`; only a reference name (for example `TEAM_API_KEY`) is
visible; no secret value appears in config, diagnostics, logs or export; export
checkboxes for credentials/raw environment default off and their help text says
values are excluded.
**Forbidden fake states:** a masked bullet that still contains the value in
the DOM/export; "redacted" export that includes the key; claim of secret
storage when only a plaintext field was saved.
**Applies:** both hosts; S03, S16, S18, S30.

### A07 — Loopback versus LAN
**Printed requirement (4.8, pp. 343–345):** "Ollama default and custom-provider opt-in
differ; private LAN remains refused under current policy."
**Preconditions (derived):** S04 has the local/Ollama route and S03 has a
custom provider; a private LAN endpoint and a loopback endpoint are available.
**Steps (derived):** 1. Open S04 and inspect the default Ollama endpoint. 2.
Open S03 and enter a loopback endpoint, then a private LAN endpoint. 3.
Attempt each save/test.
**Expected visible / field checks:** S04 `P-08`/`P-09`, S03 `P-08`/`P-09`,
S57 `Q-10`/`Q-11`; the Ollama default and custom-provider opt-in are described
separately; loopback is allowed only where the daemon host resolves it; a
private LAN address is refused with the policy reason; `allow_loopback` is not
presented as a LAN bypass.
**Forbidden fake states:** labelling a private LAN URL "local" or "safe";
turning on a hidden bypass; reporting "Connected" after a refused save.
**Applies:** both hosts; S03, S04, S57.

### A08 — Remote locality
**Printed requirement (4.8, pp. 343–345):** "A loopback URL resolves on the daemon host;
UI, workspace, vault and model locations are explicit."
**Preconditions (derived):** a remote daemon/SSH/container workspace; a
loopback model URL configured on the daemon; S31 workspace binding available.
**Steps (derived):** 1. Attach the remote workspace. 2. Configure a loopback
provider URL. 3. Open S31 and the provider details. 4. Inspect where the URL,
workspace, vault and model actually resolve.
**Expected visible / field checks:** S31 `P-13`, `O-02`, `X-13`; the UI states
that loopback resolves on the daemon host, not the laptop; editor, execution,
model, vault and candidate locations are explicit; no local/hosted boundary is
blurred; credentials are not copied into the project.
**Forbidden fake states:** "localhost" presented as the user's machine;
showing the vault location as local when it is remote; a successful probe that
crossed the wrong host.
**Applies:** both hosts; S31, S30, S26.

### A09 — Ask enforcement
**Printed requirement (4.8, pp. 343–345):** "Ask cannot dispatch a write/effectful tool
even if prompt text requests it; prompt-only mode is labelled."
**Preconditions (derived):** S06 authority/Ask; S07 plan; a write or effectful
tool is registered; the prompt explicitly asks for the write.
**Steps (derived):** 1. Select the enforced Ask mode. 2. Submit a prompt that
requests a write/effectful command. 3. Inspect the tool offer and the run
result. 4. Repeat with the prompt-only label state.
**Expected visible / field checks:** S06 `A-01`/`A-02`, S07 `A-05`; Ask shows
enforced read-only authority; a write/effect is not dispatched and the refusal
names the authority boundary; prompt-only Ask is labelled as not enforced and
is not called read-only until dispatch enforcement exists.
**Forbidden fake states:** "read-only" green badge on prompt-only Ask; a write
executed because the prompt asked; presenting a denial as a tool failure.
**Applies:** both hosts; S06, S07, S01.

### A10 — Preset and policy
**Printed requirement (4.8, pp. 343–345):** "A custom agent, rule, skill or hook cannot
widen managed authority or edit owner checkout directly."
**Preconditions (derived):** a managed/organization policy is active; a custom
agent, instruction rule, skill and hook are installable; the owner checkout is
outside the candidate worktree.
**Steps (derived):** 1. Install/import each artifact. 2. Inspect its declared
capabilities. 3. Attempt to use it to widen authority or write the owner
checkout. 4. Inspect the effective policy and audit.
**Expected visible / field checks:** S07 `A-04`/`A-05`, S11 `I-*`, S17
`S-*`, S13 `Q-12`, S18; the managed cap remains visible and enforced; the
artifact is rejected/limited with its source named; no direct owner-checkout
write; the policy version is recorded.
**Forbidden fake states:** a preset that silently grants a capability; "Allowed"
when the policy denied; hiding the managing source.
**Applies:** both hosts; S07, S11, S13, S17, S18.

### A11 — Context null
**Printed requirement (4.8, pp. 343–345):** "contextUsage=null renders Unavailable, never
0%, complete or a fabricated green utilization bar."
**Preconditions (derived):** an executed call returns `contextUsage: null`;
S62 is reachable; a draft estimate exists separately.
**Steps (derived):** 1. Open S62 after the call. 2. Inspect last-call usage and
model capacity. 3. Open sources and the draft estimate. 4. Inspect any bar,
percentage or badge.
**Expected visible / field checks:** S62 `C-01`/`C-02`; last call shows
"Unavailable" and "Usage wasn't reported for this call"; no percentage, no `0%`,
no complete/full bar; model capacity may still show its known number; the draft
estimate is labelled "estimate" on a separate basis.
**Forbidden fake states:** `0%`; a full green bar; "complete"; inferring zero
from missing usage; merging the draft estimate into measured usage.
**Applies:** both hosts; S62, S08.

### A12 — Context arithmetic
**Printed requirement (4.8, pp. 343–345):** "All estimate categories sum to one basis;
output/safety reserve are not counted as consumed input."
**Preconditions (derived):** S08/S62 has input, output reserve, safety reserve
and headroom values; the printed fixture uses 18,400 + 4,096 + 2,048 + 8,224 =
32,768.
**Steps (derived):** 1. Open the context breakdown. 2. Read each category. 3.
Change one estimate and re-read the total. 4. Compare with the model capacity.
**Expected visible / field checks:** S08 `C-02`; categories are explicitly
labelled; the four fixture values sum to 32,768; reserves are not added to
consumed input; the bar uses one basis; invalid/negative values are refused.
**Forbidden fake states:** double-counting reserves; a total that does not sum;
showing a percentage when the basis is unknown.
**Applies:** both hosts; S08, S62, S10.

### A13 — Source identity
**Printed requirement (4.8, pp. 343–345):** "Selection, diagnostic and file attachments
retain URI/range/version across worktree/session switches."
**Preconditions (derived):** S09/S37 has a selection, a diagnostic and a file
attachment; two sessions/worktrees exist.
**Steps (derived):** 1. Add each source. 2. Note URI/range/version. 3. Switch
workspace/session and back. 4. Re-open the source detail.
**Expected visible / field checks:** S37 `C-03`/`C-05`, S09
`C-03`/`C-05`/`C-13`; each source
keeps its exact URI, range and version; a stale version is flagged, not
silently refreshed; the source manifest is versioned and bound to the admitted
request.
**Forbidden fake states:** silently replacing a source with the current editor;
showing a new version under the old identity; dropping a source on switch.
**Applies:** both hosts; S09, S37, S20.

### A14 — Attachment failure
**Printed requirement (4.8, pp. 343–345):** "Unsupported/oversize/stale attachment remains
visible, blocks invalid admission and preserves text."
**Preconditions (derived):** an unsupported type, an oversize file and a stale
attachment are available; a draft with text exists in S36.
**Steps (derived):** 1. Attach each bad source. 2. Attempt to run/queue. 3. Fix
or remove one and retry. 4. Inspect the draft text.
**Expected visible / field checks:** S37 `C-05`/`C-06`, S36 `C-03`; each failed
source stays in the list with a typed reason; admission is blocked while an
invalid required source remains; the draft text and valid attachments survive;
a retry uses a new captured request if the body changed.
**Forbidden fake states:** silently dropping the attachment; "Ready" while an
invalid source remains; clearing the composer.
**Applies:** both hosts; S37, S36.

### A15 — Compaction continuity
**Printed requirement (4.8, pp. 343–345):** "Failed/minimal-reduction compaction does not
advance success; criteria and pending effects survive."
**Preconditions (derived):** S10 has a compaction plan and a measured before/
after; a forced failure and a ~1% reduction case are available; pending
approval/effect state exists.
**Steps (derived):** 1. Request compaction. 2. Force failure, then a minimal
reduction. 3. Inspect success state, criteria, approvals and effects. 4. Retry
with a valid reduction.
**Expected visible / field checks:** S10 `K-01`–`K-04`, S32 `V-07`; a failed or
inadequate reduction does not advance "Compacted"/success; the last coherent
state is retained; required criteria and pending effects remain reachable; a
successful compaction meets the configured minimum reduction, not ~1%.
**Forbidden fake states:** "Compacted" after a failed write; collapsing old UI
messages presented as compaction/deletion; dropping pending approvals.
**Applies:** both hosts; S10, S32, S08.

### A16 — Memory deletion
**Printed requirement (4.8, pp. 343–345):** "Future-use deletion is acknowledged; earlier
prompt/artifact copies are disclosed as retained."
**Preconditions (derived):** a memory item was used in an earlier prompt and
also copied into an artifact; S66 has the item.
**Steps (derived):** 1. Open S66. 2. Delete the memory item for future use. 3.
Inspect the acknowledgement and the disclosure. 4. Inspect earlier
prompt/artifact copies.
**Expected visible / field checks:** S66 `K-10`/`K-11`, S30 `O-09`; the
future-use deletion is acknowledged with scope; the UI discloses that earlier
prompts/artifacts may retain copies; no claim of universal erasure; no fake
"Deleted everywhere".
**Forbidden fake states:** "Deleted" without scope; erasing the audit trail;
claiming earlier copies were removed when they were not.
**Applies:** both hosts; S66, S30.

### A17 — Approval exactly once
**Printed requirement (4.8, pp. 343–345):** "Duplicate clicks cannot reply twice;
stale/wrong-session/generation waiters refuse visibly."
**Preconditions (derived):** S14 approval pending for a known session/
generation; a second window or stale handle exists; duplicate-click harness
available.
**Steps (derived):** 1. Open the approval. 2. Double-click Allow Once. 3.
Attempt a reply from a stale/wrong-session handle. 4. Inspect the transcript
and waiter state.
**Expected visible / field checks:** S14 `Q-06`, `Q-09`, `Q-13`; exactly one
reply is recorded; the duplicate is ignored/disabled, not sent; stale/wrong-
session/generation waiters refuse with a visible reason; the original request
facts remain.
**Forbidden fake states:** two approvals in the ledger; duplicate toast that
looks like two decisions; stale waiter silently accepted.
**Applies:** both hosts; S14, S22.

### A18 — Persistent rule boundary
**Printed requirement (4.8, pp. 343–345):** "Allow once cannot silently persist;
remembered rules require policy save/version acknowledgement."
**Preconditions (derived):** S14 has Allow Once and a separate "Create a Rule
for Future Requests…" action; S13 has a rule editor; policy persistence is
available.
**Steps (derived):** 1. Approve once. 2. Inspect rules. 3. Open the separate
rule draft, set scope/expiry, save. 4. Re-open the approval.
**Expected visible / field checks:** S14 `Q-07`, S13 `Q-02`; Allow Once creates
no persistent rule; the rule path is separate and requires a save/version
acknowledgement; the saved rule names scope, version and expiry; "Remember" is
unavailable without a real store.
**Forbidden fake states:** "Remembered" after Allow Once; a rule shown as saved
without an acknowledged revision; hidden scope widening.
**Applies:** both hosts; S14, S13, S60.

### A19 — Command normalization
**Printed requirement (4.8, pp. 343–345):** "Compound shell/interpreter/symlink targets
cannot exploit a decorative prefix/glob approval."
**Preconditions (derived):** S13/S14 can display a normalized command; test
fixtures include `bash -c`, `sh -c`, interpreter `-e`, symlinks and compound
commands; an approval rule uses a prefix/glob.
**Steps (derived):** 1. Present each adversarial command. 2. Inspect the
normalized target and cwd. 3. Attempt approval under the narrow rule. 4.
Execute in the sandbox test fixture.
**Expected visible / field checks:** S13 `Q-04`/`Q-14`, S14, S19 `X-12`; the
approval shows the effective executable/argv, cwd, symlink resolution and
effect; a decorative prefix cannot authorize the real target; the rule does
not match a different command; the denial names the mismatch.
**Forbidden fake states:** approving on the displayed prefix while executing a
different resolved target; "Allowed" for a glob that matches a symlink escape.
**Applies:** both hosts; S13, S14, S19.

### A20 — Sandbox truth
**Printed requirement (4.8, pp. 343–345):** "Failed setup blocks affected actions;
fallback network access is not labelled complete isolation."
**Preconditions (derived):** S57 has a supported isolation mode and a forced
setup failure; a fallback network-capable mode exists.
**Steps (derived):** 1. Choose the isolation mode. 2. Force setup failure. 3.
Attempt a shell/network action. 4. Switch to the fallback and inspect labels.
**Expected visible / field checks:** S57 `Q-10`/`Q-11`, S19; failed setup
blocks affected actions with the reason; fallback is labelled "Network-capable"
/ weaker isolation, never "complete isolation"; platform-established
guarantees are shown as facts, not inferred.
**Forbidden fake states:** "Isolated" green badge after failed setup; a
fallback presented as equivalent; hiding the network allowance.
**Applies:** both hosts; S57, S19.

### A21 — MCP bounds
**Printed requirement (4.8, pp. 343–345):** "Server count and UTF-8 byte/argument limits
fail before persistence; prior valid config stays intact."
**Preconditions (derived):** S16 has a valid saved MCP config; fixtures for 8
servers, 128-byte names, 512-byte args and 32 × 512-byte arguments; a config
file is inspectable.
**Steps (derived):** 1. Add servers beyond the bound. 2. Enter an oversized
name/argument set. 3. Save. 4. Inspect the persisted config and the form.
**Expected visible / field checks:** S15 `T-03`, S16; validation fails before
persistence with the exact bound; the previous valid config remains intact;
the form preserves the entered data for correction; no partial server is
written.
**Forbidden fake states:** truncating silently; saving a partial config;
"Saved" with an invalid server.
**Applies:** both hosts; S15, S16.

### A22 — MCP transport/auth
**Printed requirement (4.8, pp. 343–345):** "Reviewed stdio config never appears as
implemented remote/OAuth; cancelled auth is not Connected."
**Preconditions (derived):** S16 supports reviewed stdio only; remote/OAuth
labels may appear in legacy copy; an auth flow can be cancelled.
**Steps (derived):** 1. Open S16. 2. Inspect transport choices. 3. Start and
cancel auth for any offered remote path. 4. Return to health.
**Expected visible / field checks:** S68 `T-04`/`T-06`, S15; stdio is shown as
the implemented transport; remote/OAuth are not offered as implemented if they
are not; a cancelled auth is "Cancelled"/"Not connected", never "Connected";
the form survives cancellation.
**Forbidden fake states:** an OAuth "Connected" badge after cancel; a remote
option that cannot work; treating disconnect as effect cancellation.
**Applies:** both hosts; S68, S15, S16.

### A23 — Project executable trust
**Printed requirement (4.8, pp. 343–345):** "Opening a repository or skill manifest does
not execute MCP servers, hooks, setup or installs."
**Preconditions (derived):** a repository contains an MCP server config, a hook,
a setup script and a skill manifest that would execute; S35 trust state is
unset.
**Steps (derived):** 1. Open the repository. 2. Inspect the project artifacts.
3. Do not confirm trust; observe execution. 4. Confirm only the executable path
where intended.
**Expected visible / field checks:** S35 `O-04`/`O-05`, S17 `S-03`/`S-14`,
S16 `T-14`, S18; opening/reading executes nothing; the executable trust dialog
names the exact path and separates checksum from publisher signature; an
unrecorded path needs deliberate trust; a checksum match does not claim a
publisher signature.
**Forbidden fake states:** auto-running project setup; "Verified publisher"
from a checksum; "Trusted" for the whole repository when only one file was
confirmed.
**Applies:** both hosts; S35, S16, S17, S18.

### A24 — Hooks and verdict
**Printed requirement (4.8, pp. 343–345):** "Deadline/nonzero/malformed output differ;
partial stdout after timeout cannot authorize a tool."
**Preconditions (derived):** S18 has a hook with a deadline and a failure
policy; fixtures produce timeout, nonzero exit, malformed verdict and partial
stdout.
**Steps (derived):** 1. Run each fixture. 2. Inspect verdict, exit, output and
audit. 3. Attempt to authorize a tool from the timeout partial output.
**Expected visible / field checks:** S18 `S-11`, `S-12`, `S-13`, S67; timeout,
nonzero and malformed are distinct typed outcomes; partial stdout after
deadline is not treated as a verdict; fail-open is explicit and requires an
explicit choice; the audit cites the spec version, event and envelope.
**Forbidden fake states:** "Passed" on timeout; using partial output as an
allow; hiding the fail-open choice.
**Applies:** both hosts; S18, S67, S30.

### A25 — Child budgets
**Printed requirement (4.8, pp. 343–345):** "All child reservations share parent
envelope; saturation is bounded and ownership contention is visible."
**Preconditions (derived):** S23 has a parent run and multiple children; a
saturation/contention fixture exists; S07 can spawn a child.
**Steps (derived):** 1. Spawn children within the parent envelope. 2. Saturate
the reservation. 3. Attempt another child. 4. Inspect the ledger and
ownership.
**Expected visible / field checks:** S23 `A-08`/`A-09`/`A-11`; child
reservations are bounded by the parent envelope; saturation refuses/queues with
a visible reason; ownership contention is shown; no child can exceed the
shared budget.
**Forbidden fake states:** a child with an independent unlimited budget;
"Running" after admission refusal; hidden reservation.
**Applies:** both hosts; S23, S07.

### A26 — Steer bounds
**Printed requirement (4.8, pp. 343–345):** "Empty and >500-character steer are rejected
without truncation; stale child controls are fenced."
**Preconditions (derived):** S50 has an active child and a steer field; S23
exposes child status; a stale child handle can be simulated.
**Steps (derived):** 1. Submit empty steer. 2. Submit 501 characters. 3.
Submit a valid steer. 4. Use a stale child control.
**Expected visible / field checks:** S50 `A-10`; empty and >500-character steer
are rejected with the limit and the full text preserved (no truncation); a
valid steer is accepted once; stale controls refuse visibly and do not affect
the new child.
**Forbidden fake states:** silently truncating to 500; sending empty steer as
success; a stale control changing the new run.
**Applies:** both hosts; S50, S23.

### A27 — Owned process
**Printed requirement (4.8, pp. 343–345):** "Stale PID cannot authorize input/kill;
process stop, task stop and session abort remain separate."
**Preconditions (derived):** S19 has a running owned process; S70/S22 expose
stop scopes; a stale PID/handle fixture exists.
**Steps (derived):** 1. Start a process. 2. Attempt input/kill via a stale PID.
3. Use process stop, task stop and session abort in turn. 4. Inspect the
process/run state.
**Expected visible / field checks:** S19 `X-01`/`X-02`/`X-05`/`X-12`,
S70 `A-10`; stale PID is refused; the three stop scopes are labelled and
separate; stopping a process does not silently abort the task/session; a
closed tab is not cancellation.
**Forbidden fake states:** killing a PID solely because it matches; "Stopped"
for the task when only the process stopped; treating tab close as abort.
**Applies:** both hosts; S19, S70, S26.

### A28 — Unknown effect
**Printed requirement (4.8, pp. 343–345):** "Disconnect/timeout does not mark an external
effect cancelled or offer blind automatic replay."
**Preconditions (derived):** S39/S56 can show an ambiguous admission/effect;
S22 activity exists; a disconnect/timeout fixture is available.
**Steps (derived):** 1. Trigger a disconnect/timeout during an external effect.
2. Inspect the activity and recovery. 3. Attempt replay/retry. 4. Reconcile.
**Expected visible / field checks:** S39 `B-12`, S56 `O-04`/`D-13`, S22
`X-05`, S49 `D-07`; the effect is marked "not confirmed; may have happened", not
cancelled/failed; no blind automatic replay; reconciliation is required; the
last coherent snapshot/cursor is retained.
**Forbidden fake states:** "Cancelled" after timeout; automatic re-run of an
unknown effect; "Nothing happened" without evidence.
**Applies:** both hosts; S39, S56, S22, S49.

### A29 — Admission idempotence
**Printed requirement (4.8, pp. 343–345):** "Same UUID + same captured body recovers
receipt; altered body conflicts and needs a new request."
**Preconditions (derived):** S36/S38 has a captured submission; the same UUID
and an altered body can be replayed; a receipt lookup exists.
**Steps (derived):** 1. Submit once, interrupt the receipt. 2. Retry with the
same UUID/body. 3. Retry with the same UUID and altered body. 4. Inspect the
receipt/identity.
**Expected visible / field checks:** S36/S38/S39 `B-12`, `D-05`; the same
UUID + same body recovers one receipt (no duplicate run); an altered body
conflicts and requires a new request identity; the pending state names the
operation/owner.
**Forbidden fake states:** two runs for one UUID; silently accepting an altered
body; "Failed" when the receipt was recovered.
**Applies:** both hosts; S36, S38, S39.

### A30 — Budget exactness
**Printed requirement (4.8, pp. 343–345):** "USD 5.00 becomes exactly 5000000 micro-USD;
negatives/fractions/overflow fail token/micro-unit input."
**Preconditions (derived):** S21 has token and spending limits; `5.00` is the
printed example; malformed numeric fixtures exist.
**Steps (derived):** 1. Enter `5.00` USD and `120,000` tokens. 2. Inspect the
stored value. 3. Enter negative, fractional micro-unit and overflow values. 4.
Submit.
**Expected visible / field checks:** S21 `B-01`/`B-02`; `5.00` becomes exactly
`5000000` micro-USD; `120000` displays as `120,000`; negatives, fractional
micro-units and overflow fail with a typed validation message; `0` means no
limit only where the backend uses that convention; a money cap decrease cannot
erase settled/reserved spend.
**Forbidden fake states:** floating-point rounding that changes the cap;
treating `0` as unlimited for tokens when the contract says otherwise; accepting
`5.0000001` silently.
**Applies:** both hosts; S21.

### A31 — Unknown pricing
**Printed requirement (4.8, pp. 343–345):** "Missing rate/quota stays unavailable;
cancelled/failed attempted spend remains in eligible accounting."
**Preconditions (derived):** S54/S27 has a provider with no price/quota; a
cancelled/failed call has attempted spend; the ledger is inspectable.
**Steps (derived):** 1. Open usage. 2. Inspect price/quota. 3. Cancel/fail an
attempted call. 4. Reconcile the ledger.
**Expected visible / field checks:** S54 `O-06`/`O-07`, S27 `M-13`/`O-06`,
S21 `B-03`, S53 `K-12`; missing price/quota is "Unavailable"/"Not measured", never `$0`;
cancelled/failed attempted spend remains in eligible accounting; the VCSC
denominator is explicit.
**Forbidden fake states:** `$0.00` for an unknown price; dropping failed spend;
a VCSC computed with a zero denominator.
**Applies:** both hosts; S54, S27, S53.

### A32 — Retry policy
**Printed requirement (4.8, pp. 343–345):** "3 total attempts is not 3 retries; range/backoff
validation and effect-state replay rules hold."
**Preconditions (derived):** S21 retry controls show `3` total attempts, base
delay `250 ms`, maximum `5,000 ms`, jitter `0.2`; an effectful call can fail.
**Steps (derived):** 1. Inspect the labels and values. 2. Set out-of-range
attempts/backoff. 3. Trigger a transient and an effect-unknown failure. 4.
Inspect the retry count and replay.
**Expected visible / field checks:** S21 `B-06`–`B-08`; `3` is labelled total
attempts, not retries; ranges are validated; `250 ms`/`5,000 ms`/jitter `0.2`
are shown as the fixture values; effect-unknown is reconciled before any
retry; no blind replay.
**Forbidden fake states:** "3 retries" for 3 attempts; retrying an unknown
effect; backoff lower than base or above maximum.
**Applies:** both hosts; S21.

### A33 — Checkpoint coverage
**Printed requirement (4.8, pp. 343–345):** "Missing after blobs disable redo/diff;
uncovered shell/network/SCM effects are named before restore."
**Preconditions (derived):** S25 has a checkpoint with a missing after-blob;
a restore/fork preview includes shell/network/SCM effects; S44 can show diff.
**Steps (derived):** 1. Open the checkpoint. 2. Attempt redo/diff with the
missing blob. 3. Open restore preview. 4. Inspect uncovered effects.
**Expected visible / field checks:** S25 `V-04`/`V-05`/`V-06`, S44 `V-12`;
missing blobs disable redo/diff with a typed reason; the preview names
uncovered shell/network/SCM effects before restore; file preconditions are
shown; no claim of universal rewind.
**Forbidden fake states:** "Restored" without covering the effects; a diff
built from a missing blob; hiding uncovered effects.
**Applies:** both hosts; S25, S44.

### A34 — Manual-edit CAS
**Printed requirement (4.8, pp. 343–345):** "Restore/integration refuses changed
preconditions and preserves unrelated user edits and prior artifacts."
**Preconditions (derived):** a checkpoint/candidate with a changed file
precondition; unrelated user edits exist; S44/S25 can attempt restore/
integration.
**Steps (derived):** 1. Change a precondition file. 2. Attempt restore/
integration. 3. Inspect the refusal. 4. Reconcile and retry.
**Expected visible / field checks:** S44 `V-12`, S25 `V-05`; the operation
refuses on changed preconditions with the exact file; unrelated user edits and
prior artifacts are preserved; a retry uses a new transaction/preview.
**Forbidden fake states:** overwriting user edits; "Restored" despite a CAS
conflict; deleting the prior artifact.
**Applies:** both hosts; S44, S25.

### A35 — Engineering completion
**Printed requirement (4.8, pp. 343–345):** "Missing required acceptance/claims/matching
landing/finish remains pending despite Completed chat."
**Preconditions (derived):** S32 has required criteria, claims and a finish
level; one required check/claim/landing is missing; the chat is marked
Completed.
**Steps (derived):** 1. Complete the conversation without one required fact.
2. Open S32 proof. 3. Inspect status. 4. Add the missing fact and re-evaluate.
**Expected visible / field checks:** S32 `V-09`, S45, S48, S65; the engineering
outcome stays Pending/Incomplete; "Session Completed" is not sufficient;
missing acceptance/claim/landing/finish is named; only matching facts turn
Verified.
**Forbidden fake states:** green "Complete" from chat status; a quick check
substituting for a missing required result; "Verified" with a mismatched
landing.
**Applies:** both hosts; S32, S45, S48.

### A36 — VCSC denominator
**Printed requirement (4.8, pp. 343–345):** "All eligible attempted spend / verified
landed changes; zero verified denominator is unavailable."
**Preconditions (derived):** S53/S27 has eligible attempted spend and verified
landed changes; a zero-denominator fixture exists.
**Steps (derived):** 1. Open cost per verified change. 2. Inspect numerator and
denominator. 3. Set the denominator to zero. 4. Inspect the presentation.
**Expected visible / field checks:** S53 `K-12`, S27 `O-06`; the ratio is
"all eligible attempted spend / verified landed changes"; zero denominator is
"Unavailable", not `0`/infinity; failed/cancelled attempted spend remains in
the numerator.
**Forbidden fake states:** dividing by zero and showing `0`; excluding failed
attempted spend; a fabricated benchmark.
**Applies:** both hosts; S53, S27.

### A37 — Atomic settings
**Printed requirement (4.8, pp. 343–345):** "Invalid patch writes nothing; revision
conflict retains draft; Reset and Defaults have native semantics."
**Preconditions (derived):** S29/S60 has a valid config revision and a staged
invalid patch; a concurrent revision change exists.
**Steps (derived):** 1. Stage an invalid patch and apply. 2. Stage a valid patch
against an old revision. 3. Reset. 4. Restore defaults.
**Expected visible / field checks:** S29/S60 `O-01`; invalid patch writes
nothing; a revision conflict retains the draft and opens compare/rebase; Reset
restores applied values; Defaults stages a reset until Apply; VS Code saves
each valid value natively; JetBrains shows the first invalid field on Apply.
**Forbidden fake states:** partial save; "Saved" on conflict; Reset that
applies immediately on JetBrains; Defaults that erase the draft without an
explicit apply.
**Applies:** both hosts; S29, S60.

### A38 — Session/host fencing
**Printed requirement (4.8, pp. 343–345):** "Late callbacks and old handles never
populate or control a newly selected session/worktree/daemon."
**Preconditions (derived):** two sessions/worktrees/daemon generations; a late
callback and an old handle can be simulated; S72 switch conversation plus a daemon generation change exists.
**Steps (derived):** 1. Select session A and start a read. 2. Switch to B. 3.
Deliver A's late callback. 4. Use an old handle against B/new daemon.
**Expected visible / field checks:** S72 `D-01`/`D-15`/`X-13`; late
callbacks are dropped/fenced and cannot populate B; old handles refuse
visibly; the new session/worktree/daemon keeps its own facts; drafts survive.
**Forbidden fake states:** A's data shown in B; an old handle stopping B;
"Connected" to the old daemon after a generation change.
**Applies:** both hosts; S72, S26.

### A39 — Accessible compact view
**Printed requirement (4.8, pp. 343–345):** "240 px + 200% text, keyboard, IME, high
contrast and screen reader retain all essential controls."
**Preconditions (derived):** S58 at 240 px and 320 px; host zoom/text scale at
200%; high-contrast theme; NVDA/VoiceOver/Orca; IME composition fixture.
**Steps (derived):** 1. Open the compact Work panel. 2. Reach header, one
detail, Stop, composer and send by keyboard. 3. Compose with IME. 4. Repeat in
high contrast with a screen reader.
**Expected visible / field checks:** S58 `D-08`/`D-09`/`D-14`; primary, Deny
and scoped Stop remain reachable without horizontal scrolling; labels stack;
text wraps/elides with full value copyable; no clipped titles; focus visible,
not trapped; new approvals announce once without stealing typing focus; IME
composition never submits.
**Forbidden fake states:** shrinking text to fit; hiding Stop in overflow;
focus trap; color-only state; announcing streaming tokens one by one.
**Applies:** both hosts; S58; see §5 for the full host matrix.

### A40 — Long stream and full search
**Printed requirement (4.8, pp. 343–345):** "Windowing/coalescing preserve event order/scroll;
Find reaches unloaded history and logs stay bounded."
**Preconditions (derived):** a long transcript with unloaded history; high-rate
stream deltas; a full-history Find; bounded terminal/log buffers.
**Steps (derived):** 1. Stream while the reader is scrolled away and while
pinned. 2. Use Find for a term only in unloaded history. 3. Open the terminal/
log with a large output. 4. Scroll and interrupt.
**Expected visible / field checks:** S26 `D-13`/`D-03`, S71, S19 `X-01`;
render windowing/coalescing preserves event order and manual scroll; Find
reaches unloaded history and reports exact matches; terminal/log buffers are
bounded with older output linked as an artifact; no whole-form rerender erases
draft input; stream loss marks stale, not idle.
**Forbidden fake states:** dropping frames/events to look smooth; Find only in
the loaded DOM; unbounded log growth; "Idle" after stream loss.
**Applies:** both hosts; S26, S71, S19.

## 4. End-to-end workflows W01–W16

The document says: "Run each of the 16 workflows (3.2) end to end in both
hosts, once with a pointer and once keyboard-only" and "a workflow passes only
when every cell passes in both hosts" (p. 182). Every workflow row prints five
columns — Pointer, Keyboard, Screen reader, 200% / 320 px, Recovery — for each
host. The source cells are blank checkboxes in the PDF because none was run.

| ID | Screens | Printed flow (condensed) | Recovery / pass condition |
|---|---|---|---|
| W01 | S03, S02, S05 | Connect and validate a custom provider: API family, host, endpoint, vault reference; test auth/catalog/model; inspect capabilities/cost; save; pick model; tune offered parameters. | Failed probe preserves staged connection and prior default; secrets never enter shared config; unknown capability gated. Complete when the pair and normalized tuning are captured only after acknowledgement. |
| W02 | S04, S31, S08 | Local model without false locality: inspect daemon host/loopback; choose adapter; test exact endpoint; compare declared/observed allocation; embedder only after data-boundary review; retrieval fallback; context preview. | Private LAN refused; `allow_loopback` is not a bypass; allocation controls only when the adapter owns them. Complete when a ready local route is explicit; usage unavailable until a durable read. |
| W03 | S02, S05, S01 | Tune a model and change it mid-session: picker, capability authority, exact pair, reasoning/generation compatibility, remove invalid overrides, next-task timing, preserve draft, submit. | No in-flight rewrite; incompatible tuning stays staged; unavailable model never silently becomes another endpoint. Complete when each call records actual route/effective parameters separately. |
| W04 | S06, S07, S01 | Ask → Plan → Code: enforced Ask, read-only exploration, Plan with artifact scope, frozen plan/comments, new Code task with contract/permissions/model/finish. | Prompt-only Ask is not called read-only until dispatch enforcement; approving a plan does not broaden tool policy. Complete when Code admission binds the approved plan version/envelope. |
| W05 | S09, S08, S10 | Assemble context and resolve overflow: exact sources with URI/range/version, attachments, chips, pinning, estimate vs capacity, remove/compact/choose capable model. | Failed/oversize attachments visible; missing usage unavailable; estimates labelled; no silent truncation of criteria/effects. Complete when the admitted request has a versioned manifest and valid capacity. |
| W06 | S10, S08, S32 | Compact and inspect continuity: plan, retained facts, summary model/egress, durable operation, preserve approvals/effects, inspect before/after measured reduction. | Failure/inadequate reduction preserves last valid state; collapsing UI messages is not compaction/deletion. Complete when contract, candidate identity and obligations remain reachable. |
| W07 | S14, S13, S22 | Approve once or create a narrow rule: owner/generation, normalized args, cwd, target, effect; Allow once or Deny; exact reply; separate rule draft; match test; apply after ack. | Expired/wrong-session waiters refuse; Remember unavailable without a real store; new args need a new decision. Complete when one approval resolves once and a persisted rule has scope/version/expiry. |
| W08 | S16, S15, S13 | Add MCP with trust and recover auth: transport, bounded spec, executable source/host/env, trust, save, explicit start, health, namespaces/schemas, narrow permissions. | Remote/OAuth not offered by stdio config; cancelled auth keeps form; disconnect does not prove effect cancellation. Complete when only discovered/healthy/enabled/authorized tools are offered. |
| W09 | S17, S11, S13 | Install/invoke a skill safely: manifest/files/source/capabilities, version diff, manual/agent-selectable scope, validated install, template variables, draft/permission preview, deliberate invoke. | Viewing never runs setup; failed update retains known-good; hidden/disabled cannot remain agent-invocable. Complete when instructions are attributed and execution uses effective grants. |
| W10 | S18, S30, S22 | Author/test a lifecycle hook: native event, executable/argv, allowlisted env, deadline, caps, failure policy, input preview, bounded test under real capabilities, verdict/exit/output, save validated version. | Timeout, malformed verdict and nonzero exit distinct; fail-open explicit; partial output cannot authorize after deadline. Complete when the run cites spec version, event, envelope, bounded audit artifact. |
| W11 | S07, S23, S14 | Delegate and steer a bounded child: role within parent envelope, concurrency/budget reservation, candidate ownership, actual model/status, ≤500-character steer or supported control, child approval, typed handoff. | Queue saturation bounded; stop/revoke does not undo issued external effects; child failure preserves artifacts/parent obligations. Complete when shared budget/ownership hold; child "done" does not establish parent verification. |
| W12 | S22, S19, S26 | Queue/background/interrupt: Queue next rather than implicit interrupt, edit/remove before admission, owned process/output, deliberately stop the correct process/task/session, per-session drafts. | Closed tab is not cancellation; unknown admission/effect reconciled before replay; stream loss marks stale. Complete when each item/process/control retains owner, generation, durable result. |
| W13 | S21, S22, S27 | Rate limit/cap/unknown spend: typed 429/quota/saturation, known retry delay/total attempts, Stop available, open/settled/uncertain ledger, supported allowance revision or end. | Never guess reset time or zero price; cap reduction cannot erase spend/reservations; effectful replay needs proven safety. Complete when the final ledger retains every eligible attempted charge. |
| W14 | S24, S25, S26 | Review/restore/fork: native candidate diff, mapped criteria, recorded checkpoint, content coverage/file preconditions, supported scope, preview unrelated edits/uncovered effects, separate transaction. | Hash-only missing blobs disable redo/diff; CAS conflict preserves edits; shell/network/SCM not universally rewound. Complete when a changed candidate invalidates stale proof; historical verified artifacts stay historical. |
| W15 | S01, S33, S24, S32 | Finish with complete proof: criteria and finish level, implement, claim artifacts, required checks, proof-to-candidate match, integrate with matching identity, authorized finish, receipts/outcome. | Missing SCM/failed check/empty proof/mismatched landing stays pending/unverified; chat Completed insufficient. Complete when acceptance, claims, landing equality and finish all have matching facts. |
| W16 | S31, S30, S26 | Recover remote host/runtime change: detect new generation, stale handles, preserve drafts/prior projection, execution/model/vault map, runtime readiness/protocol, rebind/reconcile, resume supported work. | Local laptop URL is not daemon loopback; do not copy credentials into project files or replay old requests on a new host. Complete when callbacks/controls are fenced to the new binding; unknown effects stay visible. |

Workflow count check: 16 rows, IDs `W01`–`W16`, screens match Part 2.

**Printed ID conflict:** 3.1 says to repeat "the approval (W06), recovery
(W11) and history (W12) flows". In 3.2, W06 is compaction, W11 is delegation
and W12 is queue/background; approval is W07, recovery is W16, history is W14.
Follow the named flow, not the parenthetical ID, and record the conflict.

## 5. Host and platform applicability

From 3.1 (p. 182) and 3.4/3.6:

| Dimension | Required coverage |
|---|---|
| VS Code | Stable release current at test time; Windows 11, macOS, Ubuntu |
| JetBrains | IntelliJ IDEA and one other IDE on the same platform version; New UI |
| Themes | VS Code Dark Modern, Light Modern, High Contrast Dark, High Contrast Light; JetBrains Light, Dark, High Contrast |
| Layouts | VS Code sidebar 240, 320, 480 px, secondary sidebar, panel; JetBrains tool window docked right/bottom, floating, 320 px |
| Screen reader | NVDA on Windows; VoiceOver on macOS; Orca on Ubuntu (VS Code) |
| Keyboard | no pointer for the whole flow; host keymaps unchanged |
| Zoom/text | host zoom 100%, 150%, 200%; OS text scaling where supported |
| Locale | English + German (long text) + Arabic (RTL) |
| Adverse content | long workspace names/paths/model IDs/multi-line errors; 50% text expansion |
| Optical | at least 30 buttons per host and theme, cap-centre method of 3.5 |
| Contrast | rendered UI in each theme; resolve the host-token findings in 1.11 |
| Participants | ≥5 developers per host who use an agent weekly + ≥2 screen-reader users |

Universal result rule: "a workflow passes only when every cell passes in both
hosts" (p. 182). No host may inherit another host's pass.

## 6. Field-level acceptance checks (printed values)

These are the exact numeric/textual fixtures printed in the document. Use them
as assertions where the named control is exercised.

- `5.00` USD → exactly `5000000` micro-USD (B-02, A30).
- `120000` tokens → displayed `120,000` (S21/B-01).
- `18,400 + 4,096 + 2,048 + 8,224 = 32,768` context basis (S08/C-02, A12).
- `3` total attempts, `250 ms` base delay, up to `5,000 ms`, jitter `0.2`
  (B-06/B-07, A32).
- `≤500` characters for a child steer (A-10, A26).
- MCP: `8` servers maximum, `128` bytes name/argument, `512` bytes per
  argument, `32 × 512` bytes total (T-03, A21).
- `240`, `320`, `480` px panel widths; `200%` text; `50%` text expansion;
  `30` buttons per host/theme (3.1/3.4/3.5, A39).
- `60 s` quick checks / `600 s` unit / full background verification are the
  verification-policy fixture (V-08), not a UI performance SLO.
- `1,497` plate segments, `5,973` strings, `838` changes, `1,036` labels,
  `80` contrast pairs, `9` failing host defaults (3.5/3.6/1.11).

## 6.5 Restored acceptance requirements (4.6, p. 332)

These are printed separately from A01–A40 and are required integration checks;
the document says "no product-runtime certification is claimed by this design
artifact." Counted as 28 sentence-level assertions.

**Submission (4)**

- Double click/chord race admits one logical run.
- Captured draft survives ambiguous response and retry uses same identity.
- Typed invalid input refuses without silently dropping contract/attachments.
- Session switch cannot redirect old draft.

**Recovery (4)**

- Transient retry differs from stable protocol block.
- Cursor never passes malformed durable event.
- Restart preserves authority scope correctly and stale proof/cost is not reused.
- No blind repeat of unknown external action.

**Proof (3)**

- No verified complete with missing required check, unavailable claim, stale
  snapshot or incomplete finish.
- A zero-required-check Pass reducer does not automatically create proof.
- Failed proof read replaces stale verified presentation with unavailable.

**Money (4)**

- Known zero, absent value, unavailable read and no denominator are visually
  distinct.
- VCSC includes failed/cancelled eligible attempt spend.
- Local-to-hosted fallback never counted as purely local success.
- Large microUSD strings remain exact; display no scientific notation.

**Approvals (4)**

- Every Allow/Deny targets exact session/request.
- Duplicate reply is gated.
- Expired/wrong-session refusal remains visible and is not retried implicitly.
- No fabricated purpose or persistent policy control.

**Host UI (5)**

- Both hosts have light/dark/high-contrast and large-font specimens.
- 240/320/420/720+ widths keep all decisions reachable.
- Keyboard/IME/focus restoration and screen-reader text states are meaningful.
- Native diff opens proper immutable basis; unavailable basis never opens
  misleading comparison.
- Opening tool window does not launch the service.

**Delivery claims (4)**

- Each new control labeled as proposed design unless actually present.
- Current-source facts pinned and prior material preserved with dated addendum.
- Generated/prototype IDE scenes are not described as runtime screenshots or
  certification.
- Manual mobile hardware testing is not reintroduced.

## 7. Forbidden fake states (explicit or directly implied)

| Forbidden state | Printed source |
|---|---|
| `0%` / complete / full green bar when `contextUsage=null` | A11, p. 343 |
| `$0` for missing rate/quota/price | A31; 4.2 "unknown price is not zero" |
| `Measured` for an unmeasured cost/usage; `Unavailable` converted to `0` | A11/A31; S54 "Not measured" |
| `Saved` without an acknowledged applied revision | A01/A37; 4.1 state rules |
| `Connected` after cancelled auth | A22; 4.2 authentication rule |
| `Verified` with stale/missing/mismatched proof | A35; W15; 4.2 completion |
| `Cancelled` after disconnect/timeout of an external effect | A28; 4.2 unknown effect |
| `Idle` for a lost stream | A40; 4.2 stream-loss rule |
| `Restored` with uncovered shell/network/SCM effects | A33/A34 |
| `Compacted` after failed/minimal reduction | A15; 4.2 compaction invariant |
| `Deleted everywhere` for a future-use memory deletion | A16 |
| `Read-only` for prompt-only Ask | A09/W04 |
| `Isolated` after failed sandbox setup or fallback network | A20 |
| `Remembered` from Allow Once | A18 |
| `Running` after admission refusal/saturation | A25 |
| `Complete` from chat status alone | A35; 4.2 completion |
| Fabricated percentages, tool steps, optimistic completion | S58 contract; 4.2 |
| Fabricated usage, unsupported setting active, invented marketplace inventory | 4.2 product rules |
| Invented performance certifications/figures | 4.2 p. 224; 4.9 p. 346 |
| Publisher signature verified from a checksum match | A23/S35 |
| `Healthy` from a saved config instead of an observed probe | 4.2 connection rules |
| `Tool ready` from installation alone | A23/W08 |
| `Started` from opening a view | S30/S34 contracts |

## 8. Control-level and per-screen obligations

Section 4.5 prints **200** control contracts. Each contract has Default,
Validation, Timing/state, Hosts and Verification sections. The restored
acceptance requirements (4.6, p. 332) add: every control needs default,
validation, atomic apply, timing, ownership-fence and recovery checks. A
control is not "covered" by a click that merely finds its handler.

Each of the 72 screen contracts prints the same fixture instruction:

> "Exercise first-read, empty/no-match, ready, refreshing/stale,
> error/refusal, action pending and acknowledged result. Also switch scope
> while the request is in flight."

That is 8 fixture classes per screen (72 × 8 = 576 nominal exercises). They are
not separate acceptance IDs; they are required screen-level verification.

## 9. Ambiguities and cautions

1. **No printed steps/preconditions.** 4.8 prints only one-sentence
   assertions. All steps above are derived; a reviewer must not cite them as
   verbatim spec.
2. **40 vs 16.** The 40 acceptance scenarios and 16 workflows are separate
   sets. The document never prints an A→W mapping; do not equate A01 with W01.
   The 3.1 parenthetical W IDs also conflict with 3.2 (approval W06 vs W07,
   recovery W11 vs W16, history W12 vs W14); follow the flow names.
3. **"4 host themes" vs five contrast columns.** The build requirement says
   four host themes (p. 188), while 1.11 prints Dark Modern, Light Modern, JB
   Light, JB Dark and High Contrast. High contrast is additionally required in
   the IDE (p. 193); treat it as an extra mandatory pass, not a replacement.
4. **288 vs 576.** 288 is the reference-vector count (72 × 4 themes), not the
   host-render count. Two hosts × 4 themes × 72 screens = 576 host-theme
   renderings if every host gets every theme.
5. **240 vs 320 vs 420.** A39 requires 240 px; Part 3's screenshot matrix
   and 3.4 emphasize 320 px; the restored acceptance plan (p. 332) adds
   `240/320/420/720+` content widths while 3.1/4.1 use `240/320/480`. Both
   420 and 480 appear; run the union 240/320/420/480/720+ until the spec owner
   resolves it. 240 px is the harder reflow case.
6. **Find over unloaded history.** A40 requires it; 4.6 says indexed
   cross-history search only after a contract. Treat the full-history claim as
   conditional on the search contract being implemented.
7. **Timing values are not SLOs.** `60 s`/`600 s`, `250 ms`/`5,000 ms` and
   `3 attempts` are verification/retry fixtures. The document states no UI
   latency, frame-rate or throughput SLO.
8. **No CI/dogfood program is printed.** The PDF never uses "CI", "dogfood",
   "pipeline" or "SLO"; see `ci-dogfood.md` for what is actually required and
   what is a repository-side proposal.
9. **"At least 30 buttons per host and theme"** does not say how many themes
   per host or whether high contrast counts separately. The conservative
   reading is all host themes including high contrast.
10. **Platform versions are "current at test time"**, not pinned in the PDF.
    The harness must record exact build/OS/font versions.
