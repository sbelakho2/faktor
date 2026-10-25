# Testing separation policy

The system under test (SUT) is the production daemon, agent runtime, provider
adapters, UI hosts, and every code path a release can execute. A test is an
untrusted observer of that software, never a second authority inside it. This
policy makes the separation structural instead of a review convention.

## Rules

1. **No self-scored SUT.** The SUT may return data, typed refusals and
   observable effects. It may not return a test verdict, expected value,
   metric boolean, confidence score, "verified" flag, or an acceptance API
   that the test uses to decide pass/fail.
2. **Independent oracle.** Expected values are owned by the test/oracle side:
   a reviewed fixture, an independent reference implementation, a property
   invariant, a frozen contract, or a planted-mutation expectation. A test may
   import an opaque SUT result type to read the observation; it may not import
   a SUT metric/result constructor to compute the expectation.
3. **Fixture -> opaque output -> evaluator.** A stateful test drives the SUT
   from a fixture, captures its opaque output as data (JSON, rows, files,
   frames), and hands only that data to an independent evaluator. The
   evaluator never calls the SUT to obtain the expected value.
4. **Independent process and state.** Integration, UI and mutation harnesses
   run outside the daemon process. Tests that touch durable state use their
   own temporary/isolated data directory or worktree; they never read or write
   the operator's real data directory. SUT artifacts are consumed read-only.
   Mutation gates run in an isolated copy/worktree, never in the trusted tree.
5. **No shared global state.** A test does not mutate process-global state
   (`current_dir`, data-dir environment variables, global registries) that the
   main agent could also observe. Configuration is passed explicitly.
6. **No in-daemon harness.** Harness code, scenario drivers, reporters and
   pass/fail logic do not compile into the daemon, agent or UI host. They live
   in a separate test crate/process (`crates/*/tests`, `tests/*`, host test
   scripts) and drive a public or protocol boundary.
7. **New tests declare.** Every new test file declares its oracle type and
   isolation in a machine-readable header:
   `TEST-SEPARATION: oracle=<type> isolation=<type>[,<type>...]`.
   A new test function added to a baseline file also requires the inline
   declaration; the checker compares each file's test-entry count with the
   recorded baseline and rejects undeclared growth. The checker rejects an
   undeclared new file with a typed error naming the missing declaration.

## Oracle types

| Type | Meaning |
| --- | --- |
| `external-fixture` | Values live in a reviewed fixture outside the SUT. |
| `independent-reference` | A separate evaluator implements the rule. |
| `property` | A universal invariant over generated/hostile inputs. |
| `contract` | A frozen wire/contract artifact owns the expectation. |
| `mutation` | A planted production edit must make the gate fail. |
| `differential` | Two implementations are compared over a fixed corpus. |

## Isolation types

| Type | Meaning |
| --- | --- |
| `separate-process` | The harness runs in its own process. |
| `isolated-data-dir` | Temporary data dir/worktree, never the operator's. |
| `isolated-worktree` | Mutation/UI work happens in a disposable tree. |
| `isolated-browser-profile` | A fresh browser profile per harness run. |
| `read-only-artifacts` | SUT artifacts are opened read-only. |
| `pure-unit` | No durable state, filesystem, network or global state. |

`pure-unit` is valid only for a test that touches no durable/global state; a
stateful test declared `pure-unit` is a violation.

## Static gate

`node scripts/check-test-separation.mjs --check` scans Rust test modules and
integration tests, the Node/Kotlin UI harnesses, the certification scripts and
the mutation specs. It reports a typed violation for each class:

| Class | Code | Detection |
| --- | --- | --- |
| a | `self-oracle` | An `expected*` value is derived from the SUT result/outcome. |
| b | `self-attestation` | A test uses a metric setter (`with_score`, `with_pass`, ...), `is_verified*`/`self_attested` as its verdict, or a SUT self-check. |
| c | `shared-state` | A data-dir/home env var, `set_current_dir`, or an unisolated global store is used. |
| d | `same-function-oracle` | The same call computes both sides of an assertion. |
| e | `in-daemon-harness` | Harness/scenario code lives in a daemon/agent/UI host source tree. |
| - | `undeclared` | A new test file has no `TEST-SEPARATION` declaration. |

New candidates are rejected; existing candidates are recorded in
`tests/test-separation/baseline.json` so the gate is enforceable immediately
without grandfather-blanket exemptions. A baseline entry is load-bearing: if
the file disappears the checker reports a stale entry.

`--selftest` runs the same analyzer over the planted fixtures under
`tests/test-separation/fixtures/` and proves every class fires and a clean
external-oracle fixture passes. `node scripts/mutation-run.mjs --check-specs`
is the matching gate for mutation specs: each must declare `oracle` and
`isolation`, and the isolation must include an isolated worktree/copy plus a
per-spec data dir. `--write-baseline` is a reviewed maintenance operation, not
part of CI.

## Applied suites

Payments, supply/engineering traps, compaction, child handoffs, the VS Code /
JetBrains UI harnesses, the mutation runner/specs, fault campaigns and the
general Rust/Node suites are all in the scan set. Representative refactors
replace self-derived expectations with fixture/reference oracles:

- **Payments** (`crates/payments/tests/financial_races.rs`,
  `lifecycle.rs`): the fake provider's committed ledger owns expected refund
  status/keys/totals; the SUT's accepted outcomes and persisted rows are the
  observation. A wrong status or over-refund fails the explicit expectations.
- **Engineering traps** (`tests/supply-engineering-traps`): the oracle owns
  numerical windows/refusal codes; `negative_controls_fail_the_oracle_for_every_case`
  feeds deliberately wrong SUT answers and proves every one is rejected.
- **Compaction** (`crates/agent/src/runtime/compaction_tests.rs`): goal,
  required-check token and verified state are fixture constants; the
  `INV-COMPACTION-CRITERIA-TRUTH` mutation replaces the check with the goal
  and must fail the oracle.
- **Handoffs** (`crates/orchestrator/src/runtime_tests.rs`): an independent
  estimator, not `approx_tokens`, bounds the rendered handoff.
- **UI** (`apps/vscode/scripts/edition8-acceptance.mjs`, JetBrains smoke):
  scenario pass/fail lives in the external harness against fixture-owned
  labels, never in the view/controller.
- **Mutation runner/specs**: every spec declares `oracle` + `isolation`; a
  missing/non-isolated declaration fails `--check-specs` before any edit.

## CI

The checker runs in the PR programme-selection step and the trusted
certification step, after its selftest. The static-authority suite remains the
production-source authority; this checker is the test-tree authority.
