# Faktor — Autonomous Engineering Specification

Normative product specification: what Faktor is, what it must produce, and
what it must never own. This document is normative for the provider
boundaries: Faktor is standalone by definition, and Tangerine, Kiwi and any
other external product are OPTIONAL implementations of neutral interfaces —
never required dependencies. Where this document and an implementation
happen to disagree, this document states the intended direction; the current
implementation state is tracked in
[Implementation status](#implementation-status) and in the derived
capability axes.

## Purpose

Faktor is the autonomous engineering and orchestration layer.

Its job is to answer:

**What must change, why must it change, in what order should it change, and
what evidence will be required before the task can be considered complete?**

Faktor must not be its own source of truth and must not certify its own work.

## Standalone by definition

Faktor completes the full engineering loop with no external intelligence or
verification service configured. External systems plug in through neutral
provider interfaces and are always OPTIONAL:

```text
Faktor {
  Native Intelligence,
  Native Verification,
  optional Tangerine adapter,
  optional Kiwi adapter
}
```

- **Native Intelligence** is always available: Faktor's own repository
  evidence (filesystem, index, symbol, lexical search) at the intelligence
  tier it can honestly declare — never a tier it cannot deliver.
- **Native Verification** is always available: Faktor's own criterion
  proofs, checks, execution evidence and review.
- **Tangerine** is an optional richer implementation of
  `SystemIntelligenceProvider`; **Kiwi** is an optional deeper implementation
  of `VerificationProvider`. Configuring neither leaves a complete, honest
  product; configuring either enriches the same neutral surfaces.

## Core responsibility

Faktor converts user intent into a structured engineering objective and
safely drives that objective to completion.

Its task model is the neutral engineering spine:

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
Implementation
↓
Claim-bound verification via the registry
↓
Proof / Counterexample
↓
Replan or Finish
```

Every stage names a role, not a vendor: intelligence is consumed through
`SystemIntelligenceProvider` and verification through
`VerificationProvider`.

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
  claims the configured verification provider must adjudicate

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

## System intelligence (neutral)

Faktor consumes system intelligence through the generic
`SystemIntelligenceProvider` interface. Before planning a significant
change, the configured provider is consulted for:

- relevant architecture
- affected components
- system invariants
- contracts
- blast radius
- historical failures
- hidden dependencies
- uncertainty

The native provider is always available, so no external service is required
to plan. Tangerine may be configured as a richer
`SystemIntelligenceProvider`: when configured, it is selected by declared
capabilities and serves the same bounded query surface as every other
provider; when absent, intelligence degrades to the native baseline
honestly.

**No contract field, planner assumption or completion condition may require
Tangerine.** A missing external intelligence source can lower the declared
fidelity and tier; it must never block planning, alter a contract, or fail a
task.

Declared intelligence tiers are bounded `0..=5`:

```text
0  filesystem + text search (native baseline)
1  syntax parsing
2  LSP / compiler semantics
3  static dependency / call graph
4  runtime + historical evidence
5  enriched external model (for example a configured Tangerine)
```

Tier alone never creates confidence: a provider that does not declare a
fidelity is treated as unknown, and tier claims outside `0..=5` are refused.
Faktor must not maintain a separate competing system model; provider output
always enters as validated data through the neutral interface.

## Verification (neutral)

Faktor consumes verification through the generic `VerificationProvider`
interface. A proof obligation is a bounded claim that the verification
registry adjudicates; the serving provider returns evidence or a
counterexample. Proof obligations are verified by whichever neutral provider
the registry selects — Kiwi can implement that provider, but Kiwi is never
the definition of verification.

- Native Faktor verification is the baseline and is always available:
  criterion proofs, checks, execution and review evidence.
- Kiwi may be configured as an external `VerificationProvider` providing
  deeper independent and adversarial verification. It is selected when it is
  configured and capable; it is never required for ordinary work.
- A task or organization policy MAY explicitly require an external verifier
  for named claims (`require_provider` semantics). Faktor itself never
  requires Kiwi: with no external verifier configured, the native baseline
  adjudicates every obligation that policy has not explicitly routed to an
  external provider.
- A registry asked to adjudicate a claim bound to an external provider when
  none is configured refuses with a typed error; it never substitutes the
  native baseline silently and never fabricates evidence.
- Faktor must not tell a provider what verdict to return: a provider is told
  what must hold, never what results to expect beyond the required behavior.
- Provider verdicts are data: bounded, validated and identity-checked before
  any of them can influence a decision or a completion gate.

## Replanning loop

The central control loop is neutral:

```text
Faktor plans
↓
Faktor implements part of the plan
↓
The configured verification provider verifies or attacks the result
↓
The provider returns evidence or a counterexample
↓
Faktor updates its hypothesis
↓
Faktor replans
```

A failed proof obligation is not simply a failed build: it is new information
Faktor must reason about.

## Completion rules

Faktor must never declare completion merely because:

- code compiles
- existing tests pass
- the requested file changed
- an agent says the task is done

Completion requires:

- acceptance criteria satisfied
- required proof obligations adjudicated by the configured verification
  providers
- relevant invariants preserved
- unresolved risk explicitly disclosed

Completion never depends on a specific vendor. The native baseline is
sufficient unless a task or organization policy explicitly requires an
external verifier for named claims; no completion condition may
unconditionally require Tangerine or Kiwi.

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
- certify its own unverified claims
- own CI infrastructure
- hide uncertainty
- treat test execution as equivalent to verification
- duplicate an external verifier's implementation logic instead of consuming
  it through the neutral interface
- bind contracts, planner assumptions, registry selection or completion
  conditions to a vendor name

## Success criterion

Faktor succeeds when the user can provide an engineering objective and
receive:

**A review-ready, well-reasoned change whose important claims have been
independently adjudicated by the configured verification provider — the
native baseline by default, an external verifier where a task or
organization policy explicitly requires one.**

## Implementation status

This section is descriptive, not normative: it records how the current tree
implements the neutral surfaces above. The machine-readable truth for
per-capability state is the DERIVED capability axes
(`scripts/capabilities-manifest.mjs` → `target/certification/capabilities.json`);
the committed README block under-claims by design and the certified columns
stay `false`/`none` until the trusted workflow writes HEAD- and tree-bound
certificates at the exact commit.

- **System intelligence.** Shipped as the bounded provider abstraction in
  `crates/semantic`: the `SemanticProvider` contract and
  `SemanticProviderRegistry` select providers by declared capabilities only,
  and the native `GenericSemanticFallback` (Tier 0: filesystem + text
  search) is always available. External providers are strictly config-gated
  and execute through the daemon's own process supervisor / checked egress;
  absence degrades honestly, and a call that demands provider proof
  (`require_provider`) is refused typed rather than served by the fallback.
  Tangerine is the intended rich implementation of this surface; none of the
  registry, contract, planner or completion code paths names it.
- **Verification.** Shipped in `crates/verify` as the `VerificationProvider`
  contract plus `VerificationRegistry`: the native baseline is registered
  always and first, and a `require_provider` claim is served by an external
  provider or refused typed. The optional external adapter (`faktor-kiwi`)
  is constructed only when `[verification.kiwi]` is enabled and valid —
  disabled by default — so the shipped daemon matches the neutral rule that
  Kiwi is never required for ordinary work.
- **Honest coverage.** The capability axes currently certify the production
  wiring of the retrieval/provider ladder and the daemon-e2e paths with
  faked external services; adapter/live certification for external
  providers remains open and is reported as such by the axes rather than
  claimed here.
