# Engineering receipt attestation and model applicability

This document is normative for the two typed residuals closed in the
Engineering layers: signed analysis receipts and authoritative model
selection.

## Receipt attestation

Every executed analysis receipt is signed with Ed25519 over one canonical
digest. The digest covers the fields that make the claim:

- solver/adapter id and version (an external tool appends `+tool/<version>`);
- input, model/mesh and settings digests;
- property revisions and boundary conditions;
- convergence, warnings, results, uncertainty and artifacts;
- repeatability and the execution evidence (including the captured process
  output digest for external adapters);
- the applicability-selection digest, when a selector decision governed the
  analysis.

The receipt carries only the public key, its BLAKE3 fingerprint (`key_id`),
the signature and the canonical digest. It never carries the signing seed.

### Key location and permissions

The daemon-local engineering signing key is:

```
<data-dir>/engineering-signing.key
```

It is 32 random bytes, created on first use, and must be owner-only: unix
mode `0600` or stricter. The loader validates the opened handle (regular
file, exact length, no group/other bits) and refuses a key other identities
can read or write. Non-unix hosts have no POSIX mode bits; that is the honest
platform limitation. The key is never logged, exported, embedded in a
receipt, or written into a durable transcript.

`faktor engineering analyse` creates the key when absent. Verification
(`faktor engineering verify`, the daemon verify tool) loads it and never
mints a replacement: a missing key is a typed refusal, not a new identity.

### Verification path

```sh
faktor engineering verify --data-dir ~/.faktor --input stored-outcome.json
```

The CLI and the daemon `engineering_verify` tool verify:

1. the canonical digest recomputes from the receipt;
2. the Ed25519 signature verifies under the enclosed public key;
3. the enclosed key fingerprint is in the trusted set — exactly the
   daemon-local key fingerprint for this daemon.

The 11-gate contract requires gate 6 (executed analysis) and gate 7
(independent cross-check) receipts to pass this verification. A missing,
tampered, malformed or wrong-key attestation is a typed gate violation.
`EngineeringResponse::assess_with_verifier` performs the trusted check;
`LoopOutcome::verify_with_verifier` uses it for every receipt and the
response gates.

### External solvers

The signing key belongs to the daemon, not the third-party tool. For a
supervised external solver the canonical digest binds the adapter identity,
the tool version suffix and the exact captured output digest (and the
`process-output` artifact). A post-capture rewrite of the output digest,
artifact or adapter id invalidates the signature, so the receipt attests
"this adapter executed and observed these bytes", not "the vendor signed
this".

### Rotation and compromise

1. Stop the daemon or drain engineering work.
2. Move `engineering-signing.key` aside (for example to an offline archive
   with the same `0600` permissions). Do not copy it into a receipt or log.
3. Start the daemon or run `faktor engineering analyse` once; a fresh key is
   created at the same path.
4. New receipts verify under the new fingerprint. Receipts signed with the
   retired key are intentionally refused by the daemon's trusted verifier.
   To audit an old receipt, verify it offline with the archived public
   fingerprint:

   ```rust
   let verifier = engineering_core::ReceiptVerifier::trusting_key_id(old_key_id)?;
   verifier.verify(&old_receipt)?;
   ```

5. If the old key was compromised, treat every receipt it signed as
   untrusted. Re-execute the analysis to produce new receipts; do not
   copy an old signature onto a new digest.

There is no online revocation or PKI in this layer: trust is the local
daemon key set. A future operator trust store must add fingerprints, never
weaken the signature/digest checks.

## Model applicability selection

`engineering_core::ModelSelector` is the only authority for choosing
governing models. The catalog is built from the registered knowledge packs:
each model records its discipline/property targets, equations, explicit
validity domains (temperature, conditions, exclusions, material) and
provenance. The selector is deterministic and returns one of:

- `Selected`: exactly one model, with domain ids, rationale and provenance;
- `ResearchRequired`: no registered model covers the regime, or every
  candidate is outside its domain;
- `Ambiguous`: more than one registered model satisfies the same domains.
  The selector refuses to choose by preference.

Supply builds one request per requirement that binds a model, using the
candidate's established conditions, material and temperature. For a model
bound to a registered pack, the pack's validity domains are authoritative;
a requirement binding's temperature/conditions/exclusions cannot widen them.
Models with no registered pack remain explicit revision-bound bindings and
are recorded as such.

Gates 3 and 4 consume the selector result:

- gate 3 recomputes the selection from the recorded catalog and requests,
  requires every outcome to be `Selected`, and matches the theory model
  ids/versions/equations exactly;
- gate 4 derives the validity envelopes from the selected registered
  domains and requires the recorded envelopes to equal them exactly.

A caller that substitutes a model, widens a domain or edits a selected
record is refused. Research and ambiguity are surfaced as typed gate
research requests. The response records the full selection; analysis
receipts bind its digest (`selection_digest`) inside the signed canonical
digest, so a stored decision cannot be re-pointed at a different model
selection after execution.
