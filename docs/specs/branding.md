# Branding (normative)

Product name: Faktor. The native surface carries zero legacy-brand
tokens: crates faktor-*, env FAKTOR_*, headers x-faktor-*, data dir
.faktor, handshake "faktor server listening on http://127.0.0.1:<port>".

Exceptions: historical identifiers confined to the attribution/license
notices under `ui/LICENSES/` and to the scan tooling itself. No vendored
UI bundle, pinned corpus or `compat/`/`vendor/`/`third-party/` tree exists
in this repository; the static source scan
(`tests/static-authority`, `faktor-tests-static-authority`) forbids the
retired product name everywhere else, so a reappearing vendored tree or
compatibility artifact is a red build. Attribution references to retired
upstream code in prose are permitted; the product name is not.

CI enforces the wordmark allowlist (token list and path allowlist in
`scripts/branding-scan.sh`) in two modes:

- **Source scan** (pr-lane, once — never per package): every source
  root (crates, tests, apps, docs, scripts, `.github`, root manifests)
  is matched recursively for the forbidden token strings; only paths
  under the allowlist are exempt.
- **Artifact scan** (`scripts/branding-scan.sh --artifacts DIR`): the
  same tokens are matched byte-level (`grep -a`, so compressed or
  compiled payloads count) over every file under DIR. Release CI passes
  the packaged artifacts — the VS Code `.vsix`, the JetBrains plugin
  binary, and cargo release artifacts (binaries, tarballs, SBOM-adjacent
  metadata) — through this mode. **Requirement:** a packaged `.vsix` or
  JetBrains binary must contain no forbidden wordmark string outside the
  allowlisted fixture paths, i.e. no masking or rebranding remnants may
  survive inside the shipped bytes.

Visual branding assets are Faktor's own exact assets — no "+"-logo
masking remnants. A shipped binary whose strings still carry the legacy
wordmark fails the artifact scan and is not released.
