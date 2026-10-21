# Supply + Engineering trap benchmark (independent-oracle runner)

This document defines the executable companion to the "Benchmarks (vicious by
design)" section of `docs/specs/supply-engineering.md`. The suite lives in
`tests/supply-engineering-traps`. It is deterministic, bounded and offline.

## Why this is not self-scoring

The previous runner let each case hand-construct its `CaseOutcome` and set its
own `Metrics` booleans, so "13/13" only proved that fixtures matched
hand-authored answers. The v2 runner separates the two roles:

* **SUT** (`src/sut`): submits a fixture request to the real pipeline. For
  engineering cases this is
  `engineering_core::run_engineering_pipeline`: requirement validation ->
  authoritative `ModelSelector` decision over the bundled knowledge catalog ->
  unit/dimension conversion -> real `engineering-models` kernels ->
  machine-checked dimensional requirement evaluation -> an assessed 11-gate
  `EngineeringResponse`. Supply cases execute the real
  `faktor-supply-cost`, `faktor-supply-logistics` and `engineering-units`
  kernels. The SUT returns an opaque `SutAnswer`.
* **Oracle** (`src/oracle`): owns the expected outcome class, numerical
  windows, refusal codes, gate states, missing-information and boundary
  tokens, uncertainty shape and assumption coverage. It re-reads
  `engineering-knowledge` to verify pack coverage independently and computes
  the eight review dimensions. It does not import `src/sut`, the engineering
  model/solver crates or the supply cost/logistics kernels.

`SutAnswer` has no metric boolean, acceptance flag or metric setter. The public
contract has no `Metrics` type and no `with_physics`/`with_authority`/... 
builders. `CaseReport::passed()` is exactly `oracle.all()`.

## Run

```
cargo test -p faktor-supply-engineering-traps
cargo run -p faktor-supply-engineering-traps -- --json
cargo run -p faktor-supply-engineering-traps -- --text
cargo run -p faktor-supply-engineering-traps -- --case thermal/missing-contact-resistance
cargo run -p faktor-supply-engineering-traps -- --list
```

Exit codes: `0` every oracle verdict passes, `1` at least one fails, `2` CLI or
fixture failure. `--json` (default) emits the machine-readable report;
`--compact` removes whitespace. Every case reports the SUT outcome and the
oracle verdict side by side. No network, model or external solver is required.

## Cases

| id | expected SUT outcome | real execution | independent oracle check |
| --- | --- | --- | --- |
| `analog/opamp-nominal-gain-stability` | rejected `PhaseMarginViolation` | real op-amp loop-stability kernel with a 1 uF load | phase margin -25.75 deg and gain margin -14.4 dB against 45 deg/10 dB; compensated control solves; unordered poles refuse |
| `analog/opamp-large-signal-slew` | research-required (validity) | selector records the small-signal model, whose validity domain excludes large-signal slew limiting | `val-linear-small-signal` text must state the large-signal/slew exclusion; no large-signal model may be registered |
| `power/flyback-leakage-overvoltage` | research-required (validity) | the flyback authority records the model but excludes avalanche operation | `val-clamp-above-reflected` text must state the avalanche exclusion; no avalanche-energy model may exist |
| `pcb/alternate-thermal-pad` | refused `MissingInterfaceResistance` | MOSFET thermal kernel refuses the alternate without `Rth_cs`; the with-contact control solves | contact-resistance property/rule exists; original control carries an interval; zero interface is refused |
| `materials/aluminium-temper-thickness` | research-required (authority) | the materials pack supplies a conditional lookup rule but no T651 plate record | no pack contains a `plate-t651`/`yield_strength`/`250 MPa` record; a lookup rule without data cannot establish a value |
| `machine/fastener-nut-factor-scatter` | refused `OutsideValidity` | torque/preload kernel refuses K scatter outside the qualified domain | pack property/domain/trap exist; valid control returns an interval; invalid control refuses |
| `fatigue/sn-outside-regime` | research-required (validity) | the fatigue pack supplies the model but no S-N curve data | no pack contains `stress_coefficient`/`endurance_limit`/`sn_curve` data |
| `thermal/missing-contact-resistance` | refused `MissingInterfaceResistance` | thermal kernel refuses a missing interface and rejects zero | contact rule exists; with-contact control returns an interval; zero control refuses `NonPositiveGeometry` |
| `manufacturing/molding-draft-wall` | rejected `MoldabilityViolation` | moldability kernel fails draft/wall/flow; 0.5 deg and 0.5 rad controls differ | four pack rules exist; radian and good controls solve; cross-check gate is established |
| `fea/mesh-convergence-pathology` | refused `MeshConvergenceNotEstablished` | real Gmsh capability probe is mesh-only; truncated/empty meshes and pathological studies are refused | no pack contains a structural solver/convergence residual; SUT coverage must state mesh-only/Gmsh |
| `emc/return-path-loop-inductance` | rejected `ReturnPathViolation` | real EMC PCB kernel on a 10 mm decoupling distance | 18.4 nH against a 10 nH target, 11x placement factor, -0.1 mm clearance/creepage margins; adjacent control solves |
| `supply/cheaper-supplier-tco` | solved | exact `faktor-supply-cost` TCO over 15 evidence/assumption-bound terms | six exact integer-money windows, including cheap per-unit > incumbent |
| `customs/hs-classification-ambiguity` | ambiguous (`BindingRulingRequired`) | real classifier returns two viable codes and duty computation refuses | both candidate codes and a binding-ruling authority are recorded; missing facts produce a typed control |
| `units/torque-lbf-ft` | solved | real unit registry converts 100 lbf*ft | `135.58179483314 N*m`, exact round trip, 26.2% shallow shortfall, dimension/unknown-unit controls refuse |
| `supply/unknown-cost-not-zero` | unavailable (`Duty`) | exact TCO keeps total and per-unit unavailable | no fabricated total; not-applicable control solves to zero and is separately flagged |

Thirteen cases are typed non-answers: four `refused`, three `rejected`, four
`research_required`, one `unavailable` and one `ambiguous`.

## Measured dimensions

The oracle, not the SUT, computes these eight booleans. A case passes only when
all eight pass:

* `correct_physics` — outcome class, refusal code and every required numeric
  window match the oracle's independent expectation.
* `authority` — required pack items/text exist (or, for research-required, the
  absence of pack coverage is verified), and the SUT records the authority it
  used.
* `missing_info_detected` — every expected missing-information token group is
  named.
* `boundary_conditions` — every expected validity/boundary token group is
  named.
* `uncertainty` — exact, explicit interval, or typed `NotEstimated` exactly as
  the correct result requires; a refusal is never an exact point estimate.
* `trap_detected` — the shallow claim is contradicted by the executed result
  (non-solved outcome or an observation outside the shallow window).
* `falsifiable_experiment` — a complete experiment exists and covers the
  expected claim/procedure/instrument tokens.
* `refused_where_appropriate` — the SUT refuses, rejects, escalates or marks
  unavailable exactly where expected, including control probes.

## Negative controls

`negative_control(case)` replaces the SUT answer with a shallow solved answer
(empty evidence/coverage, wrong or zero observations). Every one of the 15
cases must fail the oracle. Targeted controls additionally pin the classics:
the 100 N*m unit answer, a fabricated 100 degC junction temperature and a cheap
supplier per-unit price below the incumbent. These controls exist so the
evaluator cannot be made to pass by a SUT that merely returns "solved".

## No fabricated data

Fixtures are synthetic benchmark inputs and are never presented as datasheet,
supplier, material, tariff or test evidence. Where a case would need such data,
the SUT returns a typed research/refusal/unavailable/ambiguity instead of
inventing a constant. The oracle independently verifies that no bundled pack
covers the missing regime, so research-required is a checked conclusion rather
than a SUT preference.

## Bounds and determinism

The suite is fixed at 15 cases (`MAX_CASES = 32`). Evidence, missing-info and
boundary lists are bounded at 64 entries; observations at 128. Receipts are
signed with a deterministic key derived from the fixture id; no clock, random
or process state enters the report. The Poisson control has five fixed levels
up to 128 interior nodes; pathological controls have at most eight levels.
Reports are byte-stable across runs; the tests run the suite twice and compare
serialized bytes. No nested case-by-case scans and no solver process is
spawned.

## Verification

```
cargo fmt --check
cargo test -p faktor-supply-engineering-traps
cargo test -p faktor-supply-engineering-traps
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p faktor-cli -- doctor --data-dir /tmp/faktor-ci
```

No certification or soak lane is part of this suite.
