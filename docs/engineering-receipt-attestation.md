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
the signature, the canonical digest and the signed instant (`signed_at_ms`).
The signed instant is part of the signature domain, so it cannot be moved to
evade a key's retirement window. The receipt never carries the signing seed.

### Key location and permissions

The daemon-local engineering signing material is:

```
<data-dir>/engineering-signing.key
<data-dir>/engineering-signing-keys.json
```

The seed file is 32 random bytes, created on first use, and must be
owner-only: unix mode exactly `0600`. The loader opens the data directory
once (anchored) and opens the key relative to that handle with `O_NOFOLLOW`,
so a symlink or reparse point at the key path is refused typed instead of
followed. It then validates the opened handle, never a re-resolved pathname:

- the key is a regular file of exactly 32 bytes;
- its owner UID equals the daemon's effective UID;
- its mode is exactly `0600`;
- the parent data directory is owned by the daemon user and is not group or
  world writable.

A violation is a typed `KeyStoreRefused`/`KeyStore` error naming the path and
reason; the key is never logged, exported, embedded in a receipt, or written
into a durable transcript. Windows has no POSIX owner/mode bits; the loader
returns the typed `KeyStorePlatformUnsupported` refusal instead of pretending
to enforce a check it cannot make. That is the honest platform boundary.

`engineering-signing-keys.json` is the public key history: every key's
fingerprint, raw public key, start instant and optional retirement instant.
It is `0600`, opened no-follow, bounded to 64 KiB and validated against the
enclosed public keys. Retired keys stay in the history and are verify-only.

`faktor engineering analyse` and `faktor supply decision` create the key when
absent. Verification (`faktor engineering verify`, the daemon verify tool)
loads the key history and never mints a replacement: a missing key is a typed
refusal, not a new identity.

### Verification path

```sh
faktor engineering verify --data-dir ~/.faktor --input stored-outcome.json
```

The CLI and the daemon `engineering_verify` tool verify:

1. the canonical digest recomputes from the receipt;
2. the Ed25519 signature verifies under the enclosed public key;
3. the enclosed key fingerprint is in the trusted set — every key in the
   daemon's key history, with its recorded validity window. A retired key is
   accepted only when `signed_at_ms` is before its retirement instant; it is
   refused for receipts presented as signed at or after retirement. The
   current data-dir key is used as the trust set only when no history exists
   yet (first-run migration).

The 11-gate contract requires gate 6 (executed analysis) and gate 7
(independent cross-check) receipts to pass this verification. A missing,
tampered, malformed or wrong-key attestation is a typed gate violation.
`EngineeringResponse::assess_with_verifier` performs the trusted check;
`LoopOutcome::verify_with_verifier` uses it for every receipt and the
response gates.

API naming is part of the authority boundary:

- `EngineeringResponse::assess_integrity_only` and `is_self_consistent`
  (and `LoopOutcome::verify_integrity_only` / `is_self_verified`) check only
  that a receipt's own enclosed key signs its own digest. A self-signed or
  attacker-signed document can satisfy them; they never authorize a result.
- `EngineeringResponse::is_verified` / `require_ready` and
  `LoopOutcome::is_verified` require an explicit `ReceiptVerifier` and are
  the only names reserved for authority decisions. There is deliberately no
  verifier-less `is_verified` or `require_ready` overload.
- Internal authority paths (`build_decision`, `LoopOutcome` verification)
  construct the verifier from the daemon signer's fingerprint and pass it
  through, so no gate decision falls back to self-attestation.

### External solvers

The signing key belongs to the daemon, not the third-party tool. For a
supervised external solver the canonical digest binds the adapter identity,
the tool version suffix and the exact captured output digest (and the
`process-output` artifact). A post-capture rewrite of the output digest,
artifact or adapter id invalidates the signature, so the receipt attests
"this adapter executed and observed these bytes", not "the vendor signed
this".

### Rotation and compromise

`engineering_core::rotate_signing_key(data_dir, retired_at_ms)` retires the
current key at the explicit instant, appends that fact to the key history,
and creates a fresh active key. The append-only history preserves the old
public key, so receipts signed before retirement remain auditable after a
restart:

```rust
let old = engineering_core::load_or_create_signing_key(data_dir)?;
let old_receipt = /* signed before retirement */;
let new = engineering_core::rotate_signing_key(data_dir, retired_at_ms)?;
let verifier = engineering_core::load_trusted_verifier(data_dir)?;
verifier.verify(&old_receipt)?; // retired key, old signed instant
let late = /* signed at/after retired_at_ms */;
assert!(verifier.verify(&late).is_err());
```

A signer loaded with a retired window refuses to mint new receipts
(`RetiredSigningKey`); a signature the retired key produced elsewhere cannot
pass the verifier after the retirement instant. If the old key was
compromised, treat every receipt it signed as untrusted and re-execute the
analysis; do not copy an old signature onto a new digest.

There is no online revocation or PKI in this layer: trust is the local
daemon key history. A future operator trust store must add fingerprints,
never weaken the signature/digest/validity checks.

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
