// Acceptance metadata for Revision 8 (A01–A40). The executable checks live
// in scripts/edition8-acceptance.mjs; this module only carries the frozen
// spec labels so the harness cannot drift from the printed requirements.

export interface E8AcceptanceCase {
  readonly id: string;
  readonly title: string;
  readonly printed: string;
  readonly applies: string;
}

export const E8_ACCEPTANCE: readonly E8AcceptanceCase[] = [
  {
    "id": "A01",
    "title": "Provider/model atomicity",
    "printed": "An unavailable provider/model pair is rejected; neither default changes and the draft survives.",
    "applies": "both hosts; S02, S05, S01/S36."
  },
  {
    "id": "A02",
    "title": "Capability authority",
    "printed": "Unknown model authority disables gated tools/vision/schema; manual labels do not certify support.",
    "applies": "both hosts; S02, S05, S13."
  },
  {
    "id": "A03",
    "title": "Generation compatibility",
    "printed": "Changing model revalidates all overrides; unsupported parameters cannot leak into strict DTOs.",
    "applies": "both hosts; S02, S05, S01."
  },
  {
    "id": "A04",
    "title": "Unset versus zero",
    "printed": "Reset removes the tuning override; temperature 0 and absent temperature remain distinct.",
    "applies": "both hosts; S05."
  },
  {
    "id": "A05",
    "title": "Active-call capture",
    "printed": "Editing effort/model/budget does not rewrite an in-flight request or its historical receipt.",
    "applies": "both hosts; S01, S33, S32."
  },
  {
    "id": "A06",
    "title": "Secret storage",
    "printed": "Provider/MCP/hook key values are absent from shared config, diagnostics, logs and exported bundles.",
    "applies": "both hosts; S03, S16, S18, S30."
  },
  {
    "id": "A07",
    "title": "Loopback versus LAN",
    "printed": "Ollama default and custom-provider opt-in differ; private LAN remains refused under current policy.",
    "applies": "both hosts; S03, S04, S57."
  },
  {
    "id": "A08",
    "title": "Remote locality",
    "printed": "A loopback URL resolves on the daemon host; UI, workspace, vault and model locations are explicit.",
    "applies": "both hosts; S31, S30, S26."
  },
  {
    "id": "A09",
    "title": "Ask enforcement",
    "printed": "Ask cannot dispatch a write/effectful tool even if prompt text requests it; prompt-only mode is labelled.",
    "applies": "both hosts; S06, S07, S01."
  },
  {
    "id": "A10",
    "title": "Preset and policy",
    "printed": "A custom agent, rule, skill or hook cannot widen managed authority or edit owner checkout directly.",
    "applies": "both hosts; S07, S11, S13, S17, S18."
  },
  {
    "id": "A11",
    "title": "Context null",
    "printed": "contextUsage=null renders Unavailable, never 0%, complete or a fabricated green utilization bar.",
    "applies": "both hosts; S62, S08."
  },
  {
    "id": "A12",
    "title": "Context arithmetic",
    "printed": "All estimate categories sum to one basis; output/safety reserve are not counted as consumed input.",
    "applies": "both hosts; S08, S62, S10."
  },
  {
    "id": "A13",
    "title": "Source identity",
    "printed": "Selection, diagnostic and file attachments retain URI/range/version across worktree/session switches.",
    "applies": "both hosts; S09, S37, S20."
  },
  {
    "id": "A14",
    "title": "Attachment failure",
    "printed": "Unsupported/oversize/stale attachment remains visible, blocks invalid admission and preserves text.",
    "applies": "both hosts; S37, S36."
  },
  {
    "id": "A15",
    "title": "Compaction continuity",
    "printed": "Failed/minimal-reduction compaction does not advance success; criteria and pending effects survive.",
    "applies": "both hosts; S10, S32, S08."
  },
  {
    "id": "A16",
    "title": "Memory deletion",
    "printed": "Future-use deletion is acknowledged; earlier prompt/artifact copies are disclosed as retained.",
    "applies": "both hosts; S66, S30."
  },
  {
    "id": "A17",
    "title": "Approval exactly once",
    "printed": "Duplicate clicks cannot reply twice; stale/wrong-session/generation waiters refuse visibly.",
    "applies": "both hosts; S14, S22."
  },
  {
    "id": "A18",
    "title": "Persistent rule boundary",
    "printed": "Allow once cannot silently persist; remembered rules require policy save/version acknowledgement.",
    "applies": "both hosts; S14, S13, S60."
  },
  {
    "id": "A19",
    "title": "Command normalization",
    "printed": "Compound shell/interpreter/symlink targets cannot exploit a decorative prefix/glob approval.",
    "applies": "both hosts; S13, S14, S19."
  },
  {
    "id": "A20",
    "title": "Sandbox truth",
    "printed": "Failed setup blocks affected actions; fallback network access is not labelled complete isolation.",
    "applies": "both hosts; S57, S19."
  },
  {
    "id": "A21",
    "title": "MCP bounds",
    "printed": "Server count and UTF-8 byte/argument limits fail before persistence; prior valid config stays intact.",
    "applies": "both hosts; S15, S16."
  },
  {
    "id": "A22",
    "title": "MCP transport/auth",
    "printed": "Reviewed stdio config never appears as implemented remote/OAuth; cancelled auth is not Connected.",
    "applies": "both hosts; S68, S15, S16."
  },
  {
    "id": "A23",
    "title": "Project executable trust",
    "printed": "Opening a repository or skill manifest does not execute MCP servers, hooks, setup or installs.",
    "applies": "both hosts; S35, S16, S17, S18."
  },
  {
    "id": "A24",
    "title": "Hooks and verdict",
    "printed": "Deadline/nonzero/malformed output differ; partial stdout after timeout cannot authorize a tool.",
    "applies": "both hosts; S18, S67, S30."
  },
  {
    "id": "A25",
    "title": "Child budgets",
    "printed": "All child reservations share parent envelope; saturation is bounded and ownership contention is visible.",
    "applies": "both hosts; S23, S07."
  },
  {
    "id": "A26",
    "title": "Steer bounds",
    "printed": "Empty and >500-character steer are rejected without truncation; stale child controls are fenced.",
    "applies": "both hosts; S50, S23."
  },
  {
    "id": "A27",
    "title": "Owned process",
    "printed": "Stale PID cannot authorize input/kill; process stop, task stop and session abort remain separate.",
    "applies": "both hosts; S19, S70, S26."
  },
  {
    "id": "A28",
    "title": "Unknown effect",
    "printed": "Disconnect/timeout does not mark an external effect cancelled or offer blind automatic replay.",
    "applies": "both hosts; S39, S56, S22, S49."
  },
  {
    "id": "A29",
    "title": "Admission idempotence",
    "printed": "Same UUID + same captured body recovers receipt; altered body conflicts and needs a new request.",
    "applies": "both hosts; S36, S38, S39."
  },
  {
    "id": "A30",
    "title": "Budget exactness",
    "printed": "USD 5.00 becomes exactly 5000000 micro-USD; negatives/fractions/overflow fail token/micro-unit input.",
    "applies": "both hosts; S21."
  },
  {
    "id": "A31",
    "title": "Unknown pricing",
    "printed": "Missing rate/quota stays unavailable; cancelled/failed attempted spend remains in eligible accounting.",
    "applies": "both hosts; S54, S27, S53."
  },
  {
    "id": "A32",
    "title": "Retry policy",
    "printed": "3 total attempts is not 3 retries; range/backoff validation and effect-state replay rules hold.",
    "applies": "both hosts; S21."
  },
  {
    "id": "A33",
    "title": "Checkpoint coverage",
    "printed": "Missing after blobs disable redo/diff; uncovered shell/network/SCM effects are named before restore.",
    "applies": "both hosts; S25, S44."
  },
  {
    "id": "A34",
    "title": "Manual-edit CAS",
    "printed": "Restore/integration refuses changed preconditions and preserves unrelated user edits and prior artifacts.",
    "applies": "both hosts; S44, S25."
  },
  {
    "id": "A35",
    "title": "Engineering completion",
    "printed": "Missing required acceptance/claims/matching landing/finish remains pending despite Completed chat.",
    "applies": "both hosts; S32, S45, S48."
  },
  {
    "id": "A36",
    "title": "VCSC denominator",
    "printed": "All eligible attempted spend / verified landed changes; zero verified denominator is unavailable.",
    "applies": "both hosts; S53, S27."
  },
  {
    "id": "A37",
    "title": "Atomic settings",
    "printed": "Invalid patch writes nothing; revision conflict retains draft; Reset and Defaults have native semantics.",
    "applies": "both hosts; S29, S60."
  },
  {
    "id": "A38",
    "title": "Session/host fencing",
    "printed": "Late callbacks and old handles never populate or control a newly selected session/worktree/daemon.",
    "applies": "both hosts; S72, S26."
  },
  {
    "id": "A39",
    "title": "Accessible compact view",
    "printed": "240 px + 200% text, keyboard, IME, high contrast and screen reader retain all essential controls.",
    "applies": "both hosts; S58; see \u00a75 for the full host matrix."
  },
  {
    "id": "A40",
    "title": "Long stream and full search",
    "printed": "Windowing/coalescing preserve event order/scroll; Find reaches unloaded history and logs stay bounded.",
    "applies": "both hosts; S26, S71, S19."
  }
] as const;

export const E8_ACCEPTANCE_BY_ID: ReadonlyMap<string, E8AcceptanceCase> = new Map(
  E8_ACCEPTANCE.map((entry) => [entry.id, entry]),
);
