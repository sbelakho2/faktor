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
`StoreOutcomeStore` consults the learned history exactly as before — with
no change to the existing economics paths. The ledger adds no second
learning mechanism; it makes the run-level record that feeds the existing
one.

## Metrics

`OutcomeLedgerMetrics::compute` is pure integer math (saturating,
`u128` intermediates; never floating point). Rates are floor ppm
("at least"); money/time averages round UP (never understate); an empty
denominator yields zero.

| Metric | Definition |
| --- | --- |
| Verified Cost per Successful Change | (model + verification spend over verified changes) ÷ verified changes, rounded up (`verifiedCostPerChangeMicro`) |
| Verified First-Pass Rate | first-attempt verified changes ÷ verified changes (`verifiedFirstPassPpm`) |
| Rework Cost Ratio | rework spend ÷ (model + verification spend), all runs (`reworkCostRatioPpm`) |
| Local-Model Substitution Rate | runs with ≥ 1 local step ÷ all runs (`localSubstitutionPpm`) |
| Avoided Hosted Inference | sum of recorded `avoided_hosted_micro` (never fabricated) |
| Mean Time to Verified Change | wall time over verified changes ÷ verified changes, rounded up (`meanTimeToVerifiedChangeMs`) |
| Human Intervention Rate | runs with ≥ 1 intervention ÷ all runs (`humanInterventionPpm`) |
| Regression/Rollback Rate | runs with ≥ 1 rollback event ÷ all runs (`regressionRollbackPpm`) |
| Proof Coverage | passed checks ÷ required checks over runs that required any, clamped to 100% (`proofCoveragePpm`) |
| Frontier Escalation Rate | runs with ≥ 1 frontier escalation ÷ all runs (`frontierEscalationPpm`) |

The fold scans at most `MAX_METRICS_RECORDS` (10 000) newest records; a
larger ledger reports `truncated: true` instead of pretending to be
exact.

## Surfaces

- `GET /native/outcomes` (native protocol v1, additive): `{metrics,
  runs, nextCursor}` with the full metrics aggregate, one bounded
  newest-first run page (`limit` 1..=200; `before_ms` + `before_run`
  cursor), money as decimal strings. See `docs/native-protocol.md`.
- `faktor doctor` prints one read-only line:
  `outcomes: N run(s), M verified, first-pass P%, verified cost/change C
  microUSD, rework ratio R%, rollback B%` (or an explicit empty-ledger
  line). A ledger read failure is a doctor ISSUE.

## Bounds

Payload ≤ 32 KiB; run id ≤ 200 ASCII bytes; snapshot ≤ 128 bytes;
provider/model name ≤ 128 bytes; ≤ 16 model steps; ≤ 32 verified samples;
ledger page ≤ 500; metrics fold ≤ 10 000 records; every money/counter
column is an exact non-negative integer inside the signed 64-bit durable
domain (a larger `u64` value is refused typed before any write).
