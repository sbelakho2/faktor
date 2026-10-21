# Trap fixture policy

Every file in this directory is a **synthetic benchmark input**, not
datasheet, supplier, material, tariff, or test evidence. Fixture ids and
digests are prefixed `fixture-` so they cannot be mistaken for captured
authority.

The strength of the suite is what it refuses to claim:

* The aluminium case supplies no strength value. It supplies a wrong
  temper/thickness applicability and returns research-required.
* The fatigue case supplies no S-N curve. It returns a typed validity
  research request rather than extrapolating from UTS.
* The op-amp and flyback cases supply no GBW, phase-margin, leakage or
  avalanche data. They return research-required and name the missing inputs.
* The mesh fixtures are count-level MSH2 files. The manufactured-solution
  Poisson study is computed by the Rust kernel; the pathological sequences
  are adversarial controls that must be refused.
* The HS fixture is a hypothetical ambiguity used to exercise the escalation
  path. It is not a claim about any real tariff line.

Any case that needs real property data must obtain it through the normal
revision-bound knowledge/evidence authorities; a fixture is never promoted to
authority.
