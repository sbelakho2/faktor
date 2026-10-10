# Faktor — Autonomous Engineering Specification

Normative product specification: what Faktor is, what it must produce, and
what it must never own. Where this document and an implementation happen to
disagree, this document states the intended direction; the implementation
deviations are tracked in the gap analysis at the end.

## Purpose

Faktor is the autonomous engineering and orchestration layer.

Its job is to answer:

**What must change, why must it change, in what order should it change, and
what evidence will be required before the task can be considered complete?**

Faktor must not be its own source of system truth and must not certify its
own work.

## Core responsibility

Faktor converts user intent into a structured engineering objective and
safely drives that objective to completion.

Its task model is:

```text
Intent
↓
Engineering Objective
↓
Acceptance Criteria
↓
System Invariants
↓
Risk Hypotheses
↓
Proof Obligations
↓
Change DAG
↓
Execution
↓
Kiwi Verification
↓
Replan or Finish
```

## Required task representation

Every significant task must contain:

```yaml
objective:
  what must be achieved

acceptance_criteria:
  observable conditions defining success

constraints:
  conditions that cannot be violated

invariants:
  system properties that must remain true

risks:
  likely ways the change could fail

proof_obligations:
  claims Kiwi must verify

change_plan:
  ordered dependency-aware engineering steps
```

Example:

```yaml
objective:
  migrate authentication to rotating refresh tokens

constraints:
  - existing sessions remain valid
  - existing API clients cannot break

invariants:
  - revoked sessions cannot authenticate
  - refresh tokens cannot be replayed

risks:
  - race conditions
  - migration corruption
  - legacy client incompatibility

proof_obligations:
  - concurrent refresh produces one valid result
  - replayed refresh tokens fail
  - legacy sessions survive the migration
```

## Required planning engine

Faktor must create a dependency-aware Change DAG representing:

- prerequisites
- independent workstreams
- blocked tasks
- migration order
- testing gates
- deployment gates
- rollback points

Faktor must support parallel work when safe.

## Required capabilities

Faktor must add:

- long-horizon task planning
- sub-agent delegation
- dependency-aware execution
- checkpointing
- replanning
- rollback strategies
- migration planning
- cross-repository coordination
- uncertainty tracking
- explicit risk hypotheses
- explicit proof obligations
- partial completion recovery
- interrupted-task resumption

## Tangerine integration

Before planning a significant change, Faktor must query Tangerine for:

- relevant architecture
- affected components
- system invariants
- contracts
- blast radius
- historical failures
- hidden dependencies
- uncertainty

Faktor must not maintain a separate competing system model.

## Kiwi integration

Faktor must send Kiwi:

- engineering objective
- acceptance criteria
- invariants
- risks
- proof obligations

Faktor must not tell Kiwi what results to expect beyond the required
behavior.

## Replanning loop

The central control loop is:

```text
Faktor plans
↓
Faktor executes part of the plan
↓
Kiwi verifies or attacks the result
↓
Kiwi returns evidence or counterexample
↓
Faktor updates its hypothesis
↓
Faktor replans
```

A Kiwi failure is not simply a failed build: it is new information Faktor
must reason about.

## Completion rules

Faktor must never declare completion merely because:

- code compiles
- existing tests pass
- the requested file changed
- an agent says the task is done

Completion requires:

- acceptance criteria satisfied
- required proof obligations passed
- relevant invariants preserved
- unresolved risk explicitly disclosed
- Kiwi evidence available

## Target workloads

Faktor is optimized particularly for:

- multi-service changes
- large refactors
- migrations
- dependency upgrades
- security remediation
- architectural changes
- large legacy repositories
- multi-repository changes
- long-running engineering tasks

Simple code editing remains supported but does not define the product.

## What Faktor must not do

Faktor must not:

- own a competing repository knowledge graph
- independently certify its own correctness
- own CI infrastructure
- hide uncertainty
- treat test execution as equivalent to verification
- duplicate Kiwi's verification logic

## Success criterion

Faktor succeeds when the user can provide an engineering objective and
receive:

**A review-ready, well-reasoned change whose important claims have been
independently verified.**
