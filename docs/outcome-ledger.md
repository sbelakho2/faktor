# Engineering Outcome Ledger (economic audit item 7)

Normative description of the durable completed-run record, its
exactly-once persistence, the first-class metrics derived from it, and the
surfaces that expose them.

## Purpose

Every completed run leaves ONE durable engineering-outcome record. The
ledger answers, without re-reading transcripts or trusting model-authored
claims:

- what changed (task identity, candidate/final snapshots, changed-code
  scale) and whether it was accepted and integrated;
- how much it cost (actual model spend, verification spend, rework spend,
  avoided hosted inference) and how long it took;
- how it was produced (provider/model sequence, local vs hosted, repair
  attempts, frontier escalations, human interventions);
- how well it was verified (first-pass verification, proof coverage,
  final verification basis) and what went wrong (rollback/recovery
  events, risk class).

## The record

The payload is written and read by `faktor-agent`'s `outcome_ledger`
module (`RunOutcomeRecord`, schema version 1, strict
`deny_unknown_fields` decoding):

| Field | Meaning |
| --- | --- |
| `run_id` | Durable run identity (ASCII, ≤ 200 bytes); the append-only key |
| `session_id`, `task_id` | Durable scope of the run |
| `task_class`, `risk_class` | The outcome-learning task class and the semantic risk class |
| `candidate_snapshot`, `final_snapshot` | The candidate digest verification bound and the final (landed) digest; candidate is omitted when the run verified the live root |
| `accepted`, `integrated` | The completion was accepted / landed on the owner root |
| `verified_first_attempt` | Deterministic verification passed on the FIRST implementation attempt |
| `repair_attempts` | Repair attempts before verifying (0 = first-pass) |
| `model_sequence` | Ordered provider/model steps, each with `local` and its exact `spend_micro` |
| `model_spend_micro` | Actual model spend; MUST equal the step-spend sum |
| `verification_spend_micro` | Check/review execution spend |
| `rework_spend_micro` | Spend consumed by repair attempts (≤ model spend) |
| `avoided_hosted_micro` | Recorded counterfactual hosted inference the local substitution avoided (0 = unmeasured) |
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
is a deterministic-checks basis. A first-attempt verified run MUST be a
verified change. A verified sample may carry rework ONLY as a failure
sample; a first-pass success with nonzero rework is refused.

## The producer (terminal run settlement)

The record is produced in production, not only in tests. Every terminal
run settlement appends exactly one record:

- the runtime's completion gate appends the record the moment the task row
  reaches `VerifiedComplete` (the terminal turn's explicit verified
  samples ride the record's atomic append — they are the run's ONE feed
  for that turn, never also the per-call feed);
- the task executor's post-run settlement (`settle_run`, both the
  in-session and the orchestrated arm) and both durable cancel paths
  append the record for a terminal task row (`VerifiedComplete`,
  `Failed`, `Cancelled`).

The record is DERIVED from durable, frozen terminal facts — never from
model-authored prose: the task row (identity, state, start/terminal
times), the verification records (repair attempts, first-pass, proof
coverage, review evidence, candidate snapshot), the settled spend rows
(provider/model sequence, zero-cost local steps, model spend, rework after
the first failed attempt), the durable integration record (snapshots), the
integration transaction (rollback), the resolved permission decisions
(human interventions) and the recovery events. Facts with no durable
authority stay honest zeros: `avoided_hosted_micro` (no counterfactual
recorder), `verification_spend_micro` (no process-spend ledger; review
model calls are inside model spend), `frontier_escalations` (no
frontier-tier authority; a hosted step is never inferred as frontier) and
`added_lines`/`removed_lines` (no durable line-count source). A run whose
facts are unreadable records NOTHING — a read failure is a typed refusal,
never a partial record.

Crash/replay semantics: the append is the store's ONE immediate
transaction; a crash before commit leaves no row and no router sample (the
next settlement pass re-derives and appends), a present record
short-circuits before any re-derivation (a replay is a no-op), and a
concurrent producer that lost the race is surfaced loudly with the
existing append-only record left authoritative. Neither a record nor a
router sample can be lost or doubled.

## Exactly-once durability

The record lands in the store crate's `engineering_outcome` table
(`faktor-store`, schema migration v32; `run_id` PRIMARY KEY):

- a new row is inserted and the record's verified samples are absorbed
  into the router's `model_outcome_stats` registry **in one immediate
  transaction** — a crash can neither lose the record nor double-feed the
  router;
- a replay of the byte-identical row is a `Replayed` no-op (no second
  record, no second sample);
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
denominator yields zero.

| Metric | Definition |
| --- | --- |
| Successful-Run Cost | model + verification spend over the successful (verified) runs ÷ verified changes, rounded up (`successfulRunCostMicro`) — the average spend of a run that DID succeed |
| Verified Cost per Successful Change (VCSC) | TOTAL eligible-cohort spend over ALL attempted work (failed and cancelled runs included) ÷ verified changes, rounded up (`verifiedCostPerSuccessfulChangeMicro`). Audit example: 9 failures at $10 + 1 verified change at $1 => $91 per successful change (never $1) |
| Verified First-Pass Rate | first-attempt verified changes ÷ verified changes (`verifiedFirstPassPpm`) |
| Rework Cost Ratio | rework spend ÷ (model + verification spend), all runs (`reworkCostRatioPpm`) |
| Local-Only Verified Rate | verified local-only runs ÷ local-only runs (`localOnlyVerifiedPpm`, with the `localOnlyRuns`/`localOnlyVerifiedChanges` counts): how often a purely local run verifies |
| Local-Final Verified Rate | verified local-final runs ÷ runs whose FINAL model step is local (`localFinalVerifiedPpm`, with the `localFinalRuns`/`localFinalVerifiedChanges` counts) |
| Local-First Success Rate | local-first runs that verified on their first attempt ÷ runs whose FIRST step is local (`localFirstSuccessPpm`) |
| Local-to-Frontier Fallback Rate | runs that started local and ENDED hosted ÷ runs whose first step is local (`localToFrontierFallbackPpm`); a local-then-hosted repair is a FALLBACK, never a local substitution |
| Frontier-Only Rate | runs with no local step at all ÷ all runs (`frontierOnlyPpm`) |
| Avoided Hosted Inference | sum of recorded `avoided_hosted_micro` (never fabricated) |
| Mean Time to Verified Change | wall time over verified changes ÷ verified changes, rounded up (`meanTimeToVerifiedChangeMs`) |
| Human Intervention Rate | runs with ≥ 1 intervention ÷ all runs (`humanInterventionPpm`) |
| Regression/Rollback Rate | runs with ≥ 1 rollback event ÷ all runs (`regressionRollbackPpm`) |
| Proof Coverage | passed checks ÷ required checks over runs that required any, clamped to 100% (`proofCoveragePpm`) |
| Frontier Escalation Rate | runs with ≥ 1 recorded frontier escalation ÷ all runs (`frontierEscalationPpm`) |

The fold scans at most `MAX_METRICS_RECORDS` (10 000) newest records; a
larger ledger reports `truncated: true` instead of pretending to be
exact.

## Surfaces

- `GET /native/outcomes` (native protocol v1, additive): `{metrics,
  runs, nextCursor}` with the full metrics aggregate, one bounded
  newest-first run page (`limit` 1..=200; `before_ms` + `before_run`
  cursor), money as decimal strings. See `docs/native-protocol.md`.
- `faktor doctor` prints one read-only line:
  `outcomes: N run(s), M verified, first-pass P%, successful-run cost C
  microUSD, all-in cost/verified change V microUSD, rework ratio R%,
  rollback B%` (or an explicit empty-ledger line). A ledger read failure
  is a doctor ISSUE.

## Bounds

Payload ≤ 32 KiB; run id ≤ 200 ASCII bytes; snapshot ≤ 128 bytes;
provider/model name ≤ 128 bytes; ≤ 16 model steps; ≤ 32 verified samples;
ledger page ≤ 500; metrics fold ≤ 10 000 records; every money/counter
column is an exact non-negative integer inside the signed 64-bit durable
domain (a larger `u64` value is refused typed before any write).
