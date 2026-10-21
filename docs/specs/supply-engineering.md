# Faktor Supply + Faktor Engineering (normative)

Faktor acquires commercial truth (`faktor-acquire`, `faktor-commerce`,
connectors, browser). On top of that evidence plane, build two reasoning
systems that share one evidence/contract authority:

```
Faktor
 ├── Faktor Engineering   (requirements → design → analysis → verification)
 └── Faktor Supply        (sourcing → qualification → TCO → logistics → planning)
            ▲
      Acquire + Commerce  (deterministic commercial observation; no hidden LLM)
```

Hard rules (apply everywhere):

* No consequential assertion without: why it is true, under which conditions,
  its uncertainty, its provenance, and how it could be disproved.
* Never fabricate data or turn unknown into zero. Unknown is `unavailable`
  with a reason; assumptions are explicit typed assumptions with provenance.
* LLMs choose models/formulate work; consequential arithmetic is executed by
  deterministic Rust kernels or real solvers, with dimensional checking.
* Exact money and quantities (integer micros/units; no f64 in money paths).
* Every finding/decision is revision-bound, evidence-bound and typed;
  conflicts are surfaced, never averaged.
* All external access goes through the checked egress/browser authorities;
  secrets never enter model context, artifacts, logs or exports.
* No Python in runtime or tooling. Optional Tangerine/Kiwi remain optional;
  neither is required for Supply/Engineering to function.
* Bounded everything (inputs, outputs, graphs, solver runs, queues, memory).

## Faktor Supply

The central object is a `SupplyDecision`:
requirement, candidates, technical_fit, supplier_capability,
commercial_terms, landed_cost, quality_risk, continuity_risk,
logistics_plan, inventory_effect, lifecycle_risk, assumptions,
uncertainty, evidence, counterfactuals, recommended_action.

**Supply graph** (typed, revisioned): Part, Manufacturer, Supplier, Site,
Shipment, Process, Qualification, PCN/EOL, ContractTerms, Risk. Distinguish
manufacturer vs authorized distributor vs independent distributor vs broker
vs marketplace seller. Never merge them.

**Supplier capability** is engineering-grade, not name+country: manufacturing
processes, machinery/line capability (SMT package range, placement accuracy,
feeder capacity, soldering, AOI/SPI/X-ray, ICT/FCT, coating, traceability,
MSL handling; machining axes/travel/spindle/workholding/CMM/surface finish),
capacity and available capacity in the required period, lot sizes, tooling,
test/metrology/calibration, process capability (Cpk/Ppk), qualifications,
traceability, NCR/OTD/PPM/lead-time distributions, responsiveness, ownership,
site relationships, authorization status, continuity/disaster exposure.

**Should-cost** (manufactured item), every term provenance-or-assumption:
raw material × yield; process route; machine rate × cycle; setup and tooling
amortization; direct labor; inspection/test; scrap/rework; overhead;
packaging; margin → ex-works. Then risk-adjusted TCO: origin inland freight,
export handling, international freight, insurance, import handling, duty,
tax, brokerage, final mile, working-capital cost, expected quality cost,
expected expedite, excess-MOQ carrying, obsolescence exposure. Unknown stays
unknown.

**RFQ/negotiation**: generate RFQ packages, normalize bids to comparable
terms, amortize tooling/NRE, compare volume breaks, detect quote
inconsistencies, should-cost targets, BATNA, payment-term value, Incoterms,
warranty/liability, NCNR, lead-time distributions. Orders/messages are
explicitly authorization-gated. RFQ-only pricing is never fabricated.

**Logistics/customs**: Incoterms operationally, packaging/chargeable weight,
consolidation, mode selection, handling, port dwell, clearance, dangerous
goods, insurance, demurrage/storage, multimodal schedules. Customs:
identity → composition/function → candidate HS classifications →
interpretive-rule reasoning → country tariff code → origin → preference/FTA
→ documents → duty/tax. Ambiguous classifications stay ambiguous and are
escalated; tariff systems are authoritative inputs.

**Materials planning**: EBOM/MBOM × demand × scrap → gross; − on-hand −
confirmed supply → net; time-phased purchasing. Lead-time distributions,
service levels, safety stock, reorder points, ABC/XYZ, allocation, MOQ/
multiples, shelf life, dual sourcing, last-time-buy.

## Faktor Engineering

One common **Engineering Design Graph**: Requirement, Constraint, Function,
Interface, Architecture, Component, Material, Geometry, Net, Signal,
PowerRail, LoadCase, Environment, Tolerance, Datum, ManufacturingProcess,
FailureMode, Hazard, Inspection, Test, Equation, PhysicalModel,
SimulationCase, SimulationResult, Assumption, Uncertainty, Evidence,
Decision.

Properties are conditional facts, not constants:

```rust
EngineeringValue {
    quantity, material, condition, temperature, orientation,
    thickness_range, tolerance_or_distribution, source, revision,
    applicability, uncertainty,
}
```

**Knowledge packs** (versioned): terminology, governing models, equations,
assumptions, validity domains, property sources, design/manufacturing rules,
failure modes, diagnostic signatures, test methods, standards mappings,
worked cases, counterexamples, known traps, provenance. Standards accessed
through licensed stores/connectors; never reproduced blindly.

**Solvers**: neutral `EngineeringSolver` (capabilities, applicability, solve
on a frozen case) with receipts binding input/model digest, solver identity
and version, property revisions, boundary conditions, mesh/settings,
convergence, warnings, results, uncertainty, artifacts, repeatability and an
Ed25519 attestation over the canonical digest. Model choice is owned by the
central applicability selector, and gates 3/4 recompute its result; see
`docs/engineering-receipt-attestation.md` for key location, rotation and
verification instructions.
Rust numerical kernels for units/dimensional analysis and deterministic
math; real solvers through supervised adapters (KiCad CLI, ngspice, Gmsh,
OpenFOAM, FMI; commercial via the same contract). No Python.

**11-gate response contract**: (1) problem formulation; (2) knowledge
authority; (3) theory selection; (4) validity envelope; (5) dimensional
consistency (machine-checked); (6) executed analysis; (7) independent
cross-check; (8) sensitivity/uncertainty; (9) counterexample/failure search;
(10) realization consequences (manufacturing/reliability/sourcing/cost);
(11) falsifiable verification. If 3–4 fail, research instead of improvise.

**Domains** (domain packs over one substrate): systems, analog, power
electronics, digital, PCB, RF, EMC/EMI, thermal, solid mechanics, dynamics,
fatigue/fracture, fluids, materials, machine design, GD&T/metrology,
manufacturing, reliability, test engineering, cost engineering. Cross-
disciplinary coupling is mandatory (e.g. MOSFET loss → Tj → heatsink →
airflow → PCB copper → sourcing variation).

## Closed loop

Engineering constraints flow to Supply (parts/materials/processes/cost/
availability/risk); Supply candidates flow back to Engineering (technical
fit, thermal/tolerance/EMC/reliability impact, qualification tests); only
then promote an alternate, negotiate, plan logistics, set inventory policy.

## Crate layout

```
crates/supply-core, supply-supplier, supply-cost, supply-quality,
       supply-logistics, supply-planning, supply-execution
crates/engineering-core, engineering-knowledge, engineering-units,
       engineering-models, engineering-electrical, engineering-mechanical,
       engineering-thermal, engineering-manufacturing,
       engineering-reliability, engineering-research, engineering-solvers,
       engineering-eda, engineering-cad
```

Reuse the existing evidence/contract/journal/proof authorities; do not create
a second notion of truth.

## Benchmarks (vicious by design)

Include traps where shallow AI looks right and is wrong: unstable op-amp
despite nominal gain; flyback leakage-induced MOSFET stress; PCB alternate
with different thermal pad; aluminium strength for wrong temper/thickness;
fastener torque with unrealistic K; fatigue outside the S-N regime; thermal
model ignoring contact resistance; injection-molded geometry that cannot be
manufactured; FEA mesh/convergence pathology; cheaper supplier whose saving
vanishes under MOQ/logistics/yield/cash cycle. Measure correct physics,
correct authority, missing-info detection, boundary conditions, uncertainty,
trap detection, valid experiment construction, and willingness to refuse.
The executable runner and its evidence/refusal policy are specified in
`docs/specs/supply-engineering-traps.md`.
