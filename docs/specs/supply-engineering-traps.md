# Supply + Engineering trap benchmark (normative runner)

This document defines the executable companion to the "Benchmarks (vicious by
design)" section of `docs/specs/supply-engineering.md`. The suite lives in
`tests/supply-engineering-traps` and is deterministic, bounded, and offline.

## Run

```
cargo test -p faktor-supply-engineering-traps
cargo run -p faktor-supply-engineering-traps -- --json
cargo run -p faktor-supply-engineering-traps -- --text
cargo run -p faktor-supply-engineering-traps -- --case fea/mesh-convergence-pathology
cargo run -p faktor-supply-engineering-traps -- --list
```

Exit codes: `0` every case passes, `1` at least one case fails, `2` CLI or
fixture failure. `--json` (default) emits the machine-readable report;
`--compact` removes whitespace. No network, model, or external solver is
required.

## Cases

| id | Shallow claim | Deterministic behaviour | Real authority used |
| --- | --- | --- | --- |
| `analog/opamp-nominal-gain-stability` | gain 10 means stable | typed research-required (Validity): phase/gain margin and loop data absent | bundled knowledge registry scan; no loop-stability pack |
| `power/flyback-leakage-overvoltage` | reflected voltage is the whole Vds stress | typed research-required (Theory): leakage/avalanche excluded by the pack | `power-electronics` assumption `asm-conduction-only` |
| `pcb/alternate-thermal-pad` | same copper area is thermally equivalent | typed refusal `MissingInterfaceResistance`; zero interface is rejected | `power-electronics` `prop-contact-rth`, `rule-contact-resistance`; MOSFET thermal kernel |
| `materials/aluminium-temper-thickness` | any 6061 has the familiar strength | typed research-required (Authority): T6 thin-sheet applicability cannot cover T651 80 mm | `EngineeringValue`/`MaterialRef`/`ThicknessRange`; no material property pack |
| `machine/fastener-nut-factor-scatter` | K = 0.2 gives a known preload | valid control returns an interval; scatter outside the qualified K domain is refused `OutsideValidity` | `bolted-joint` `prop-nut-factor`, `val-k-domain`, `trap-unrealistic-k`; torque-preload kernel |
| `fatigue/sn-outside-regime` | endurance limit = 0.5 UTS | typed research-required (Validity): no S-N/run-out envelope for the target regime | bundled pack scan; no fatigue/S-N pack |
| `thermal/missing-contact-resistance` | Rth_jc + Rth_sa is enough | missing interface is refused `MissingInterfaceResistance`; zero Rth_cs is a typed invalid input; known Rth_cs solves with intervals | `power-electronics` contact-resistance rules; MOSFET thermal kernel |
| `manufacturing/molding-draft-wall` | CAD closes, so it can be molded | draft, wall and flow checks fail; 0.5 rad and 0.5 deg are distinguished; a valid control passes | `injection-molding` rules and `trap-draft-units`; moldability kernel |
| `fea/mesh-convergence-pathology` | solver exit 0 + many elements means converged | truncated/empty meshes are refused; a manufactured-solution study shows second-order convergence; non-monotone and stalled studies are refused; Gmsh is mesh-only | Gmsh MSH2 parser, `AnalysisRequest::Mesh3d` capability, Rust Poisson kernel |
| `supply/cheaper-supplier-tco` | $8.50 beats $10.00 | exact landed TCO: cheap total and per-unit are higher under MOQ, freight, yield/quality, cash cycle and carrying | `faktor-supply-cost` TCO, evidence-bound terms |
| `customs/hs-classification-ambiguity` | pick the lower-duty code | two viable candidates produce `Ambiguous` + `BindingRulingRequired`; duty computation refuses; missing facts produce `InsufficientFacts` | `faktor-supply-logistics` classification and duty kernels |
| `units/torque-lbf-ft` | 100 ft-lb = 100 N-m | 100 lbf*ft = 135.58179483314 N*m; round trip is exact; force/energy mixing and unknown symbols are refused | `faktor-engineering-units` registry |
| `supply/unknown-cost-not-zero` | unknown duty is probably zero | total and per-unit stay `Unavailable`; `Duty` is named; explicit not-applicable is a separate zero control | `faktor-supply-cost` TCO |

## Measured dimensions

Every case report carries these booleans, and the suite passes only when all
eight pass for every case:

* `correct_physics` — the observed behaviour matches the physically or
  legally correct result, including the correct typed refusal.
* `authority` — the result is bound to a real pack/solver/unit/evidence
  authority, or explicitly records that no authority covers the claim.
* `missing_info_detected` — unknown inputs are named and stay unknown.
* `boundary_conditions` — the governing validity/boundary condition is named
  and checked.
* `uncertainty` — an interval/distribution or the typed absence of a value is
  explicit; a refusal never becomes a point estimate.
* `trap_detected` — the shallow claim is contradicted by the executed case.
* `falsifiable_experiment` — the report contains a concrete claim, procedure,
  pass criterion, refutation, instrument and evidence.
* `refused_where_appropriate` — the case refuses, rejects, escalates or marks
  unavailable exactly where the correct result requires it.

The summary also reports outcome counts (`solved`, `rejected`, `refused`,
`research_required`, `unavailable`, `escalated`) and per-metric pass counts.

## No fabricated data

Fixtures are synthetic benchmark inputs and are never presented as datasheet,
supplier, material, tariff or test evidence. Where a case would need such
data, it returns a typed research-required/refusal/unavailable/escalation
instead of inventing a constant:

* aluminium: no yield/tensile value is supplied;
* fatigue: no S-N curve or endurance limit is supplied;
* op-amp: no GBW, pole/zero or phase-margin data is supplied;
* flyback: no leakage inductance, snubber or avalanche data is supplied.

The MOSFET, bolted-joint, moldability, TCO, classification and unit cases use
the real kernels and unit registry in this workspace. The FEA case uses the
real MSH2 parser and a manufactured-solution finite-difference check; the
mesh-only Gmsh adapter is not treated as a structural solver.

## Bounds and determinism

The suite is fixed at 13 cases (`MAX_CASES = 32`). Evidence, missing-info and
boundary lists are bounded at 64 entries; observations at 128. The Poisson
study has five fixed refinement levels up to 128 interior nodes and is linear
in the number of levels; the pathological controls have at most eight levels.
There are no nested case-by-case scans and no solver process is spawned.
Reports are byte-stable across runs, and the tests run the suite twice and
compare the serialized bytes.

## Verification

```
cargo fmt --check
cargo test -p faktor-supply-engineering-traps
cargo test -p faktor-supply-engineering-traps
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p faktor-cli -- doctor --data-dir /tmp/faktor-ci
```

No certification or soak lane is part of this suite.
