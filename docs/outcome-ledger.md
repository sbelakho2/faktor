# Engineering Outcome Ledger (economic audit item 7)

Normative description of the durable completed-run record, its
exactly-once persistence, the economic settlement lifecycle that carries its
money, the first-class metrics derived from both, and the surfaces that
expose them.

## Purpose

Every completed run leaves ONE durable engineering-outcome record. The
ledger answers, without re-reading transcripts or trusting model-authored
claims:

- what changed (task identity, candidate/final snapshots, changed-code
  scale) and whether it was accepted and integrated;
- how much it cost (actual model spend, verification spend, rework spend,
  avoided hosted inference) and how long it took;
- how it was produced (provider/model sequence with its durable execution
  class, repair attempts, frontier escalations, human interventions);
- how well it was verified (first-verification-pass, proof coverage,
  final verification basis) and what went wrong (rollback/recovery
  events, risk class).

Engineering truth and financial finality are SPLIT (P1 economic-truth):

- the **engineering record** (`TerminalEngineeringOutcome`) is immutable
  from the moment the run reaches its terminal state and carries NO
  monetary fact — reservations can still move money after the terminal
  transition, and an immutable row must never freeze an inexact monetary
  fact;
- the **economic settlement lifecycle** (`EconomicSettlement`, append-only)
  carries the money with a typed `money_state`:
  `complete` | `pending` | `unavailable`. The latest event is the settled
  financial projection every read derives from; monetary metrics fold only
  `complete` rows and report the exclusion count.

## The engineering record

The payload is written and read by `faktor-agent`'s `outcome_ledger`
module (`TerminalEngineeringOutcome`, schema version 2, strict
`deny_unknown_fields` decoding; legacy v1 payloads decode compatibly):

| Field | Meaning |
| --- | --- |
| `run_id` | Durable run identity (ASCII, ≤ 200 bytes); the append-only key |
| `session_id`, `task_id` | Durable scope of the run |
| `task_class`, `risk_class` | The outcome-learning task class and the semantic risk class |
| `candidate_snapshot`, `final_snapshot` | The candidate digest verification bound and the final (landed) digest; candidate is omitted when the run verified the live root |
| `accepted`, `integrated` | The completion was accepted / landed on the owner root |
| `verified_first_verification_pass` | The run's FIRST verification attempt passed: the certifying deterministic pass is the run's earliest verification attempt (no failed verification attempt precedes it). A verification-attempt measure — NOT an implementation-attempt claim (see below) |
| `repair_attempts` | Verification attempts that failed before the certifying pass (0 = the first verification attempt passed; each failed verification sends the run back to repair) |
| `model_sequence` | Ordered provider/model steps, each with its durable `execution_class` (`local` / hosted tiers / `unknown`); no money |
| `wall_time_ms` | Run start → terminal outcome |
| `human_interventions` | Permission decisions, manual steers/edits |
| `proof` | `checks_passed` / `checks_required` (invariant: passed ≤ required) |
| `change_scale` | Files, added lines, removed lines |
| `rollback_events`, `recovery_events` | Rollback and crash-recovery events of the run |
| `final_basis` | `deterministic_checks` \| `deterministic_checks_and_review` \| `review_only` \| `unverified` |
| `frontier_escalations` | Escalations to a hosted frontier-tier model |
| `completed_at_ms` | Terminal timestamp (ms since the Unix epoch) |
| `verified_samples` | The explicit per-call verified samples the outcome-learning router learns from |

A run is a **verified change** iff `accepted && integrated && final_basis`
is a deterministic-checks basis. A first-verification-pass run MUST be a
verified change. A verified sample may carry rework ONLY as a failure
sample; a first-pass success with nonzero rework is refused.

### What the first-verification-pass flag measures — and what it does not

`verified_first_verification_pass` counts VERIFICATION attempts, never
implementation attempts. It is true iff the run is a verified change and
its certifying deterministic verification is the run's earliest durable
verification attempt (`verification_record` rows, in creation order — one
per completed verification run; a failed record means the run was sent
back to repair). A run whose first verification attempt failed and whose
later attempt passed is false, with `repair_attempts` naming the failures.

This is deliberate: the durable ledger carries NO
implementation-attempt identity, so a "verified on the FIRST
IMPLEMENTATION attempt" claim cannot be derived honestly. The durable
machinery that exists proves why:

- `provider_call.attempt_op_id` / `attempt_ordinal` and
  `cost_reservation.attempt_op_id` identify the PHYSICAL wire attempts of
  one logical model call (`faktor_core::op::ModelCallAttempt`; ordinal 0 =
  the original send, 1 = the first retry) — a network retry identity, not
  an implementation pass. One implementation pass contains many model
  calls (the tool loop), so counting them would misname tool roundtrips
  and transports as implementation attempts;
- `turn_record` rows are admitted logical turns (prompt cycles) with no
  implementation/verification classification and no task attribution; a
  turn may plan, answer, verify or implement;
- `verification_record` / `verification_attempt` rows exist only once
  verification ran, so several implementation/model attempts before the
  first verification are structurally invisible to them (the exact hole a
  `repair_attempts == 0` "first attempt" reading had);
- the task row's `revision` is an optimistic-lock token bumped by
  state/plan/budget mutations, not an attempt counter.

Therefore the metric and its wire field are named for what they measure.
The payload tag of a pre-rename record (`verified_first_attempt`, which
claimed the retired meaning) still decodes into this field under its
honest definition; new writes always carry the honest tag.

### Execution classification (P1)

Every step's `execution_class` is derived from DURABLE ROUTE-TIME
AUTHORITY only — never from a cost number:

- each `cost_reservation` row carries the class the route froze for its
  call (schema migration v34: `local` | `hosted_economy` |
  `hosted_balanced` | `frontier`; NULL = no authority, legacy rows);
- the daemon's routing policy derives that class AT DECISION TIME from the
  frozen price authority (locality — `LocalZero` is the only authority
  that establishes it) beside the configured routing mode (the hosted
  tier: Economy -> `hosted_economy`, Balanced -> `hosted_balanced`,
  MaximumQuality -> `frontier`; a pin establishes no tier); an unpriced
  path stays `unknown`, never a guessed tier;
- the reserve path persists it beside the frozen pricing snapshot, and the
  outcome derivation reads the reservation's durable class (a pre-v34 row
  whose class column is NULL is `local` IFF its frozen pricing authority
  is `PriceAuthority::LocalZero`);
- **a numeric zero is NEVER locality**: a hosted free tier, promotion or
  internal endpoint settles a real zero with a hosted price authority and
  is NOT local;
- `unknown` means no durable authority established the class and is
  counted as NEITHER local nor hosted by every metric;
- a durably-known class and the persisted `local` bit must agree — a
  disagreement is a typed corruption, and legacy payloads without the
  class key decode to `unknown` (never a guessed class).

## The economic settlement lifecycle

ONE append-only stream per run (`economic_settlement`,
`(run_id, seq) PRIMARY KEY`), written by `faktor-agent`'s
`OutcomeLedger`:

| Field | Meaning |
| --- | --- |
| `seq` | Event position (0 = initial; assigned by the store on later appends) |
| `money_state` | `complete` (every reservation settled and measured) \| `pending` (an open/dispatched/uncertain reservation remains) \| `unavailable` (the projection cannot be established: unreadable ledger, unmetered settlement, unknown status) |
| `reason` | Why the projection was classified so (bounded, stable) |
| `model_spend_micro`, `verification_spend_micro`, `rework_spend_micro` | Settled amounts, present IFF `complete` — every other state stores NULL (zero is only for a MEASURED zero) |
| `avoided_hosted` | Typed counterfactual measurement: `measured` with its exact value (a measured zero is legal) or `unavailable` with a NULL value. Production has no counterfactual authority, so it is `unavailable`, never `0` |
| `steps` | The settled per-step spend projection (identity + `execution_class` + `spend_micro`); empty unless `complete` |
| `settled_at_ms` | When the event settled |

- The INITIAL event (`seq = 0`) lands in the SAME immediate transaction as
  the engineering row and the router feed: a crash can neither leave a
  terminal record without its money classification nor the classification
  without its record.
- Late **finalization**: when the runtime's best-effort task-completion
  accounting (`reconcile_uncertain` + conservative `finalize_uncertain` at
  the reserved estimate) did not land at the completion gate, the task
  executor's terminal settlement retries it (idempotent) and then
  re-derives the projection from durable rows; a moved projection is
  appended as the next event. A `complete` projection is never touched by
  late finalization — corrections require an explicit correction event.
- Store-enforced transition legality: `pending → complete | unavailable`;
  `unavailable → complete | pending` (a RECOVERED ledger read may establish
  that reservations are open again — a legal improvement, never a
  re-unmeasuring); `complete → complete` (corrections) only. Only
  `complete` is terminal and immutable-once-settled: `pending` and
  `unavailable` are non-final projections a later, better-informed
  derivation may legally improve, so an `unavailable → pending` move is a
  legal recovery, not a regression. A byte-identical replay of the latest
  event is a `Duplicate` no-op at any clock.
- `verification_spend_micro` is a measured scope zero: no separate paid
  verification process exists; review model calls are inside
  `model_spend_micro`. Every other monetary fact is NULL until measured.
- A settlement whose reservation ledger read is unreadable, contains a
  settled row with no measured amount (the documented Unknown spend), or
  exceeds the read bound is `unavailable` — never a partial sum presented
  as exact. Note that a settled reservation with an unknown cost basis
  also blocks `complete`, because the exact task spend cannot be proven.
- Legacy v1 engineering payloads (money lived inside the record) synthesize
  a `complete` projection from their recorded historic money; their
  historic `avoided_hosted_micro == 0` decodes as `unavailable` (the v1
  contract documented 0 as unmeasured).

`pending`/`unavailable` rows are typed, visible and counted — they are
NEVER silently zeroed, and monetary metrics never include them.

## Where the run is re-derived (terminal facts outbox)

Both production producers — the runtime's completion gate and the task
executor's settlement (`settle_run`, both arms, and both durable cancel
paths) — append the ONE record, and they must agree for every legal
interleaving. The non-derivable terminal-turn facts (risk class + the
terminal turn's explicit verified samples) are therefore persisted FIRST
into the store's durable `terminal_run_facts` outbox by the agent path
BEFORE the task row can become terminal; every producer derives the
record from the SAME durable facts:

- a completion claim the task row refuses corrects the facts (this turn's
  calls did not verify) before any later settling turn overwrites them;
- the facts freeze once the record exists (`terminal_run_facts_upsert` is
  refused typed afterwards);
- a producer that lost the append race observes the existing record; the
  runtime feed switches to telemetry-only only after confirming the
  durable record carries exactly those samples.

Everything else in the record is derived from durable, frozen terminal
facts — never from model-authored prose: the task row (identity, state,
start/terminal times), the verification records (verification attempts,
first verification pass, repair count, proof coverage, review evidence,
candidate snapshot), the settled spend rows (provider/model sequence,
execution class, model spend, rework after the first failed attempt), the
durable integration record (snapshots), the integration transaction
(rollback), the resolved permission decisions (human interventions) and
the recovery events. Facts with no durable authority stay honest zeros or
typed absences: `avoided_hosted` (typed `unavailable`, never 0),
`frontier_escalations` (no frontier-tier authority; a hosted step is never
inferred as frontier) and `added_lines`/`removed_lines` (no durable
line-count source). A run whose engineering facts are unreadable records
NOTHING — a read failure is a typed refusal, never a partial record; an
unreadable MONEY fact does not block the engineering record, it types the
settlement `unavailable`.

Crash/replay semantics: the append is the store's ONE immediate
transaction; a crash before commit leaves no row and no router sample (the
next settlement pass re-derives and appends), a present record
short-circuits before any re-derivation (a replay is a no-op), and a
concurrent producer that lost the race is surfaced loudly with the
existing append-only record left authoritative. Neither a record nor a
router sample can be lost or doubled.

## Exactly-once durability

The engineering record lands in the store crate's `engineering_outcome`
table (`faktor-store`, schema migration v32; rebuilt by v33 WITHOUT money
columns; `run_id` PRIMARY KEY) and the settlement stream in
`economic_settlement`:

- a new row is inserted, its `seq = 0` settlement event is appended and
  the record's verified samples are absorbed into the router's
  `model_outcome_stats` registry **in one immediate transaction** — a
  crash can neither lose the record nor double-feed the router;
- a replay of the byte-identical row is a `Replayed` no-op (no second
  record, no second sample, no second event);
- a re-append under the same `run_id` with a DIFFERENT value is a typed
  `Conflict`; the append-only identity is never overwritten.

Reads validate every column fallibly and cross-check the payload against
its projection columns: a hostile or drifted row is a typed corruption,
never a fabricated record or metric.

## The router feed

`verified_samples` carry exactly the explicit verified attribution of the
existing per-call feed (`faktor-router`'s `OutcomeSample` semantics):
deterministic gate sites record success/failure and measured rework;
"the model said done" is not a sample. Recording a run feeds each sample
into the durable per-key projection keyed by `(provider, model, phase,
task_class, risk_class)`, so every `RouterService` wired over
`StoreOutcomeStore` consults the learned history exactly as before. The
terminal turn's samples ride the run record's transaction (fed exactly
once); samples of non-terminal gates keep the per-call feed, and a
turn settling more samples than one record carries (`MAX_RUN_VERIFIED_SAMPLES`)
splits at the cap — the record feeds the first samples and the remainder
feeds per call, never dropped and never doubled. The ledger adds no second
learning mechanism; it makes the run-level record that feeds the existing
one.

## Metrics

`OutcomeLedgerMetrics::compute` is pure integer math (saturating,
`u128` intermediates; never floating point). Rates are floor ppm
("at least"); money/time averages round UP (never understate); an empty
denominator yields zero. ENGINEERING metrics fold every row; MONETARY
metrics fold ONLY `complete` settlements:

| Metric | Definition |
| --- | --- |
| Successful-Run Cost | model + verification spend over the money-complete successful (verified) runs ÷ verified money-complete changes, rounded up (`successfulRunCostMicro`) |
| Verified Cost per Successful Change (VCSC) | TOTAL eligible-cohort spend over ALL attempted money-complete work (failed and cancelled runs included) ÷ verified money-complete changes, rounded up (`verifiedCostPerSuccessfulChangeMicro`). Incomplete rows NEVER participate: `moneyIncompleteExcluded` counts them and `verifiedChangesMoneyComplete` is the denominator. Audit example: 9 failures at $10 + 1 verified change at $1 => $91 per successful change (never $1) |
| Verified First Verification Pass Rate | verified changes whose FIRST verification attempt passed ÷ verified changes (`verifiedFirstVerificationPassPpm`, with the `verifiedFirstVerificationPassChanges` count). A verification-attempt measure — never an implementation-attempt claim (see above) |
| Rework Cost Ratio | rework spend ÷ (model + verification spend), over the money-complete cohort (`reworkCostRatioPpm`) |
| Local-Only Verified Rate | verified runs whose every step is durably local ÷ runs whose every step is durably local (`localOnlyVerifiedPpm`, with the `localOnlyRuns`/`localOnlyVerifiedChanges` counts): how often a purely local run verifies |
| Local-Final Verified Rate | verified runs whose FINAL model step is durably local ÷ runs whose FINAL model step is durably local (`localFinalVerifiedPpm`, with the `localFinalRuns`/`localFinalVerifiedChanges` counts) |
| Local-First Success Rate | local-first runs (FIRST step durably local) whose FINAL step is ALSO durably local and whose FIRST verification attempt passed ÷ runs whose FIRST step is durably local (`localFirstSuccessPpm`); the same verification-attempt measure as the Verified First Verification Pass Rate |
| Local-to-Hosted Fallback Rate | runs that started local and ENDED on a durably hosted step ÷ runs whose first step is durably local (`localToHostedFallbackPpm`); a local-then-hosted repair is a FALLBACK, never a local substitution |
| Hosted-Only Rate | runs whose every step is durably HOSTED ÷ runs with steps (`hostedOnlyPpm`); an `unknown` execution class counts as neither local nor hosted |
| Avoided Hosted Inference | sum of MEASURED counterfactuals (`avoidedHostedInferenceMicro`); `null` with `avoidedHostedInferenceMeasurement = "unavailable"` when no run carried a measurement — never a fabricated zero |
| Mean Time to Verified Change | wall time over verified changes ÷ verified changes, rounded up (`meanTimeToVerifiedChangeMs`) |
| Human Intervention Rate | runs with ≥ 1 intervention ÷ all runs (`humanInterventionPpm`) |
| Regression/Rollback Rate | runs with ≥ 1 rollback event ÷ all runs (`regressionRollbackPpm`) |
| Proof Coverage | passed checks ÷ required checks over runs that required any, clamped to 100% (`proofCoveragePpm`) |
| Frontier Escalation Rate | runs with ≥ 1 recorded frontier escalation ÷ all runs (`frontierEscalationPpm`); `null` — never a fabricated 0% — unless durable execution-class authority exists, with `frontierEscalationMeasurement` = `measured` \| `partial` \| `unmeasured` |

The fold scans at most `MAX_METRICS_RECORDS` (10 000) newest records; a
larger ledger reports `truncated: true` instead of pretending to be
exact. The fold reads each row's LATEST settlement event, so VCSC and
every other money metric recompute as late settlements land.

## Surfaces

- `GET /native/outcomes` (native protocol v1, additive): `{metrics,
  runs, nextCursor}` with the full metrics aggregate, one bounded
  newest-first run page (`limit` 1..=200; `before_ms` + `before_run`
  cursor). Money is decimal-string (`*Micro`) only when the run's
  `moneyState` is `complete`; otherwise the field is `null` and
  `moneyState`/`avoidedHostedMeasurement` carry the typed state. See
  `docs/native-protocol.md`.
- `faktor doctor` prints one read-only line:
  `outcomes: N run(s), M verified, first-verification-pass P%, successful-run cost C
  microUSD, all-in cost/verified change V microUSD, rework ratio R%,
  rollback B%` — money figures over the money-complete cohort, with
  `K money-incomplete excluded (...)` appended when the exclusion count is
  nonzero. Each figure is gated on its OWN served denominator, exactly like
  the IDE panels: the two cost figures on `verifiedChangesMoneyComplete`,
  the rework ratio on the money-complete cohort's total spend, and the
  first-verification-pass rate on verified changes — an empty denominator
  prints `unavailable`, never a fabricated zero (a measured zero still
  prints as an exact `0`). A ledger read failure is a doctor ISSUE.

## Bounds

Payload ≤ 32 KiB; run id ≤ 200 ASCII bytes; snapshot ≤ 128 bytes;
provider/model name ≤ 128 bytes; ≤ 16 model steps; ≤ 32 verified samples;
ledger page ≤ 500; metrics fold ≤ 10 000 records; terminal-facts payload
≤ 16 KiB; settlement reason ≤ 512 bytes; settlement steps JSON ≤ 16 KiB,
≤ 64 steps, ≤ 128 bytes per provider/model label; batched settlement reads
≤ 400 run ids per query; every money/counter column is an exact
non-negative integer inside the signed 64-bit durable domain (a larger
`u64` value is refused typed before any write).
