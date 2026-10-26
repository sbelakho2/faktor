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

- **Native Intelligence** is always available and is **not limited to
  Tier 0**: Faktor's own filesystem/lexical search, tree-sitter syntax,
  LSP/compiler, revision-bound local graph and runtime/history evidence each
  contribute at the tier they can honestly declare (0–4). No native source
  may claim a tier its machinery cannot deliver.
- **Native Verification** is always available: Faktor's own criterion
  proofs, checks, execution evidence and review.
- **Tangerine** is an optional richer implementation of
  `SystemIntelligenceProvider`; **Kiwi** is an optional deeper implementation
  of `VerificationProvider`. Configuring neither leaves a complete, honest
  product; configuring either enriches the same neutral surfaces. Neither is
  ever required.

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

## Fresh-context design readiness (zero-call Elephant/Goldfish)

Faktor adopts the useful independence property of the Elephant/Goldfish
pattern **without adding another model invocation to the normal path**. After
native contract compilation and the existing optional
`SystemIntelligenceProvider` enrichment, a deterministic fresh-context pass
reviews only the compiled `EngineeringContract` and its bounded compiler
facts. It never sees the implementation conversation or hidden architect
reasoning.

The pass may only report bounded **risk hypotheses** for structural gaps such
as high/unknown risk without claim-bound proof, unbound required proof, or a
concurrency/migration/compatibility concern without corresponding proof
coverage. The findings are **advisory and ephemeral**: they are never applied
to the durable contract, so they create no risk row, artifact, evidence or
second source of truth. The pass **must not** create or discharge proof
obligations, certify completion, call tools, call a model, call Tangerine
again, or write a design capsule/reviewer transcript/artifact. Because the
findings are not persisted, they cannot newly promote a calm contract to the
High/Unknown risk tier or raise Faktor's routing quality floor.

The durable `EngineeringContract` remains the single design authority. A tiny
in-memory focus projection may be derived from its proof bindings, risks and
invariants. A lazy gate skips all projection work unless the active work item
duplicates more than 2 KiB of path material; the work item's unique title
(which may carry explicit evidence references) is preserved, and the task's
owned/recent path needs still carry every precise path, so the replacement
can never make a required path unreachable. The projection is never appended
to the baseline search material, is not rendered as a new prompt section and
is not persisted. Both final wire plans are rendered with the real tokenizer;
the focused plan is adopted only when it saves at least 512 tokens AND at
least 12.5% of the baseline. Otherwise the baseline plan and evidence stay
byte-identical. Small/local tasks and tasks that fail the gate keep the
baseline path byte-for-byte, while long tasks can sharpen evidence selection
without another context artifact.

Role separation is strict:

```text
Native intelligence + optional Tangerine enrichment
                         ↓
             EngineeringContract
                         ↓
        native fresh-context readiness audit
             (risk only; zero calls)
                         ↓
                  implementation
                         ↓
       native verification + optional Kiwi
                         ↓
               completion proof/gate
```

Tangerine therefore gives the contract richer eyes through the existing
neutral intelligence seam; the readiness audit challenges that contract
without sharing implementation context or persisting its hypotheses; Kiwi
independently adjudicates the neutral claim batch after implementation. Kiwi
submissions carry no contract constraints/invariants/risks and no readiness
text, so local advisory state can never become an independent-verification
premise. No stage creates a competing system model or a second source of
truth.

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
0  filesystem + lexical search (native baseline)
1  syntax parsing (tree-sitter)
2  LSP / compiler semantics
3  dependency / call graph (revision-bound native impact graph)
4  runtime + historical evidence (execution, coverage, past checks)
5  enriched external model (for example a configured Tangerine; optional)
```

Tier alone never creates confidence: a provider that does not declare a
fidelity is treated as unknown, and tier claims outside `0..=5` are refused.
A declared tier is additionally bounded by the evidence source that supports
it: a filesystem/lexical source cannot claim above Tier 0, tree-sitter above
Tier 1, LSP above Tier 2, the native graph above Tier 3, runtime/history
above Tier 4, or an external provider above Tier 5.

### Per-operation quality

Quality is declared **per operation** (`snapshot`, `context`, `delta`,
`affected`, `verify`, `explain`), never as one provider-wide scalar. For
each operation a provider descriptor declares:

- fidelity (how close the evidence is to compiler semantics);
- completeness (how much of the truth the answer covers);
- the orthogonal evidence dimensions (freshness, determinism, revision
  binding, provenance scope/uncertainty, measured corroboration);
- the intelligence tier; and
- provenance (the evidence-source class: filesystem/lexical, tree-sitter,
  LSP, graph, runtime/history, or external).

An absent declaration is untrusted (`none`/`unknown`), never inherited from
silence. The native fallback derives its per-operation quality from the
sources actually wired: LSP-backed operations report higher fidelity/tier
than lexical-only ones, the revision-bound impact graph reports Tier 3, and
runtime/history evidence reports Tier 4.

Authoritative consumers (merge/review preflight, planning context,
affected-set risk, verification metadata) declare the minimum quality they
require and pass it to **one selection gate**. The gate accepts or refuses
with a typed result; a low-fidelity answer can never satisfy a high-fidelity
requirement, and no consumer may hand-roll a separate fidelity check.
Non-authoritative consults may use the same gate with a declared-trust or
advisory floor.

The revision-bound local graph is a legitimate native evidence source under
this model. It is **not a competing authoritative system model**: it is one
revision-bound source of semantic evidence consumed through the same neutral
`SystemIntelligenceProvider` interface, its findings enter as validated
DATA with provenance, and external providers may enrich or supersede it by
declaring higher per-operation quality. Faktor does not maintain a second
source of truth beside the program's own compiler/runtime evidence.

Provider output always enters as validated data through the neutral
interface.

### Bounded findings protocol

Before pure contract compilation the runtime ASKS the configured provider
the fixed, bounded question set derived from the goal — affected
components/architecture, invariants that must survive, hidden dependencies,
blast radius, historical failures/migrations. The ask is an explicit
asynchronous operation (`operation_id`, `session_id`, workspace, start time,
deadline, cancellation) and is wall-bounded by the runtime; a parked or
unresponsive provider is dropped, never awaited forever.

Every answer is a typed finding (`component | invariant | dependency | risk |
historical_failure`) bound to the repository snapshot digest and the serving
provider identity. Findings are validated as DATA: a snapshot/revision or
provider mismatch, invalid provenance, an oversized text/basis, an
out-of-range confidence or an unknown kind is refused with a typed
diagnostic and dropped. Findings never fail a turn. Accepted findings become
immutable bounded enrichment statements carrying
`StatementProvenance { origin: external:<provider> }` and feed the pure
compiler; the native contract they enrich stays byte-identical when no
findings arrive. Confidence is clamped by the provider's declared evidence
dimensions — a tier claim never substitutes for an evidence claim.

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

- own a competing authoritative system model; the native graph is a
  revision-bound evidence source under the neutral interface, not a second
  source of truth
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
  `SemanticProviderRegistry` select providers by validated capabilities and
  per-operation declared quality, and the native `GenericSemanticFallback`
  is always available. Its per-operation quality is derived from the wired
  native sources (filesystem/lexical 0, tree-sitter 1, LSP 2, revision-bound
  impact graph 3, runtime/verification 4); it is not a Tier-0-only claim.
  External providers are strictly config-gated and execute through the
  daemon's own process supervisor / checked egress; a legacy wire descriptor
  is normalized to per-operation quality with `external` provenance.
  Absence degrades honestly, and a call that demands provider proof
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
