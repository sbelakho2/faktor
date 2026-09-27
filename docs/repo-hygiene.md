# Repository hygiene and license policy

Audit items 20-21. This document is the operator-facing record of the
license authority, the dependency policy gate, and the committed-artifact
hygiene scanner.

## 1. License authority

- `LICENSE` is the canonical, unmodified Apache License 2.0 text from
  `https://www.apache.org/licenses/LICENSE-2.0.txt`
  (sha256 `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30`,
  11358 bytes). The digest is pinned by
  `tests/static-authority/src/lib.rs::license_and_policy_authority_is_wired`;
  any edit to the text is a red test until the pin is deliberately updated.
- `NOTICE` names the product and points at `LICENSE`; historical
  third-party attribution retained for removed vendored UI material stays
  under `ui/LICENSES/NOTICE.md`.
- Every workspace member inherits the license
  (`license.workspace = true`, workspace value `Apache-2.0`); hard-coded
  `license = "..."` overrides and `license-file` entries are refused by
  the same scan. The 50+ manifests already complied, so no manifest needed
  a change.
- Publish metadata sanity: `cargo metadata --locked` is the authority; the
  policy script fails when any workspace member resolves to a license other
  than Apache-2.0, so a future crate cannot silently ship under a different
  license. This workspace is an application, not a published crate set:
  no member declares `description`/`repository`, so cargo's own publish
  path is fail-closed on required metadata and the only publishability
  question the gate needs to answer is the license axis.

## 2. Dependency license / bans / sources policy

`deny.toml` (cargo-deny v2 format) is the single source of truth:

- `[licenses] allow` is a permissive allow list (MIT, Apache-2.0,
  `Apache-2.0 WITH LLVM-exception`, ISC, BSD-1/2/3-Clause, 0BSD, BSL-1.0,
  CC0-1.0, MIT-0, Unicode-3.0, Unlicense, Zlib). OR expressions pass when
  one branch is allowed, AND expressions only when every branch is
  allowed. Copyleft-only requirements fail the gate and need a reviewed
  policy change, never a per-crate exception.
- `[sources]` allows the crates.io index only (`unknown-registry` and
  `unknown-git` are `deny`, `allow-git` is empty), so a dependency cannot
  appear from an unvetted registry or git remote.
- `[bans]` refuses wildcard requirements on external dependencies.
  Version-less workspace-internal `{ path = ... }` edges are deliberately
  exempted and documented in `deny.toml` (cargo-deny's wildcard check
  cannot distinguish them; the script excludes path edges).

Enforcement: `scripts/check-licenses.sh` evaluates exactly this policy
against `cargo metadata --format-version 1 --locked` with python3 (no tool
install required) and additionally runs
`cargo deny --offline check licenses bans sources` when a cargo-deny binary
is on `PATH`, so the two engines cannot drift. The trusted CI static lane
runs `bash scripts/check-licenses.sh` as a required step (also recorded in
the lane marker's command digest). Local run:

```sh
bash scripts/check-licenses.sh
cargo deny check licenses bans sources   # optional reference engine
```

## 3. Committed-artifact hygiene scanner (scan 13)

`tests/static-authority` scan 13 (`committed_tree_carries_no_bytecode_build_artifacts_or_opaque_binaries`)
judges the COMMITTED file set (`git ls-files --cached`, filtered to files
present on disk) and fails on:

1. any path carrying a `__pycache__/` segment or a `*.py[cod]` bytecode
   extension;
2. compiled build-artifact extensions: `pyc pyo pyd o obj a rlib rmeta so
   dylib dll exe class pdb jar vsix`;
3. opaque extensionless binaries at or above 64 KiB (no extension, not a
   dotfile, NUL byte in the first 8 KiB) — no provenance, no source story.

`.gitignore` blocks the same classes (`__pycache__/`, `*.py[cod]`,
`*.o`, `*.obj`, `*.rlib`, `*.rmeta`, `*.dylib`, `*.dll`, `*.exe`,
`/mu_test`) so local runs do not propose them for commit in the first
place.

Every exception is an exact `HYGIENE_ALLOWLIST` entry with a written
justification; the scan asserts each entry is non-stale (the committed file
exists) and load-bearing (the rules would flag it without the exemption).
Current entries: the pinned Gradle wrapper jar (sha256-verified by
`scripts/check-gradle-integrity.sh`) and two certification fixtures that
simulate packaged release artifacts for the `scripts/certification`
evidence tests.

The scanner is adversarially tested
(`hygiene_scanner_fires_on_planted_violations`): a synthetic tree plants a
`__pycache__` bytecode file, a compiled object, and a 70 KB opaque
extensionless binary — all three fire — while an extensionless source
script, a binary with a non-artifact extension, and an allowlisted fixture
pass. The local SHA-256 helper backing the license pin is checked against
the published `abc` test vector.

### Adding a deliberate binary fixture

Commit it under a named fixture directory (never the repository root),
document provenance, include its sha256, add a consuming test that reads
it, and add the exact path + justification to `HYGIENE_ALLOWLIST`. A file
that cannot meet that bar does not belong in the repository.

## 4. Historical violations and disposition

- `crates/cli/tests/fixtures/__pycache__/mcp_mock.cpython-314.pyc`
  (2791 B Python 3.14 bytecode for the committed
  `crates/cli/tests/fixtures/mcp_mock.py` mock server): deleted; the `.py`
  source is the fixture, `__pycache__/` is now ignored, and scan 13 keeps
  the class out.
- root `mu_test` (465576 B Mach-O 64-bit arm64 executable, sha256
  `27f1470a9de168d23c8a407dc3738b90d857c35082a52fe7b906dac00cb08e12`):
  added 2026-09-04 by commit 9d4824a ("Wave 4: CI overhaul, ...") with zero
  references in tracked sources, tests, scripts or docs (verified with
  `git log --follow` and a repository-wide reference search). No
  provenance, no documented digest, no consuming test — the only
  disposition that meets the fixture bar is deletion. Deleted; the class
  (opaque extensionless binary ≥ 64 KiB) is refused by rule 3, `/mu_test`
  is ignored, and `HYGIENE_RETIRED_PATHS` keeps either path from
  reappearing unnoticed.
