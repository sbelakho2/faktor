# CI enforcement runbook

Status of the release/toolchain enforcement surfaces for
`sbelakho2/faktor` (GitHub remote `origin`), as of 2026-09-23. "Applied"
means the action was executed from this checkout via `gh`; "Operator" means
it needs credentials or a server this checkout does not have.

| Surface | State | Where |
| --- | --- | --- |
| `rust-toolchain.toml` pinned channel 1.98.0 (+rustfmt/clippy, minimal) | Applied | `rust-toolchain.toml` |
| CI resolves the toolchain file (linux lane asserts it; JetBrains smoke derives `--default-toolchain` from it) | Applied | `.woodpecker/**` |
| Apache-2.0 `LICENSE` (canonical text pinned by sha256) + `NOTICE`; every workspace member inherits `license.workspace = true` | Applied | `LICENSE`, `NOTICE`, `tests/static-authority/src/lib.rs` (scan 13) |
| Dependency license/bans/sources policy (`deny.toml`) enforced in the trusted static lane via `scripts/check-licenses.sh` (locked `cargo metadata` + optional cargo-deny) | Applied | `deny.toml`, `scripts/check-licenses.sh`, `.woodpecker/trusted/trusted.yaml` |
| Committed-artifact hygiene scan (bytecode / build artifacts / opaque extensionless binaries ≥ 64 KiB) with justified load-bearing allowlist and planted-fixture proof | Applied | `tests/static-authority/src/lib.rs` (scan 13), `.gitignore`, `docs/repo-hygiene.md` |
| Container images pinned by multi-arch index digest (non-Rust) | Applied | `.woodpecker/trusted/trusted.yaml`, `.woodpecker/untrusted/pr.yaml`, `.woodpecker/trusted/nightly.yaml`, `scripts/woodpecker/docker-compose.yml` |
| VS Code E2E toolchain pinned by release version + commit + sha256 in the lane script (verified before extraction; `code --version` must print both) | Applied | `scripts/vscode-e2e.sh`, `.woodpecker/trusted/trusted.yaml` |
| Lane-bound marker auth: per-lane HMAC `auth` over the canonical unsigned marker, one shared generator, `unattested` (not silent pass) without a token | Applied | `scripts/certification/lane-marker.{sh,mjs,py}`, `scripts/certification/evidence.mjs`, `.woodpecker/trusted/trusted.yaml`, `.woodpecker/trusted/nightly.yaml` |
| Invariant criticality + planted-mutation proof: every entry is release-critical by default (its `authority` names the production gate) and must ship a planted `mutation_command`; non-empty `mutation_debt` fails both checker modes, downgrades require `criticality_reason` | Applied | `tests/invariants.toml`, `scripts/check-invariants.mjs`, `scripts/mutation-run.mjs`, `scripts/mutations/*.json`, `.woodpecker/trusted/trusted.yaml`, `.woodpecker/untrusted/pr.yaml` |
| `@vscode/vsce` pinned exactly in the VS Code lockfile and run as the locked binary | Applied | `apps/vscode/package.json`, `apps/vscode/package-lock.json`, `.woodpecker/**`, `scripts/package-artifacts.sh` |
| Branch protection on `main` requires `ci/woodpecker/pr/pr` (strict) and the PR path | Applied | GitHub API |
| Woodpecker webhook for this repo | Applied (local tunnel) | hook `685914438`, ping `200` |
| `ci/woodpecker/pr/pr` publication | Applied via single-project switch (upstream 3.18/3.19 has no two-projects-per-repo selector) | Woodpecker server |
| Commit signature shows `verified:true` on GitHub | Applied (repo-local account noreply identity) | GitHub account + SSH signing key |
| Release-signing role `faktor-release-signing` | Provisioned (secret matches allowlist) | Woodpecker secret + `scripts/certification/attestation-keys.json` |

## 1. Branch protection (applied)

`main` now has (verified with
`gh api repos/sbelakho2/faktor/branches/main`):

- required status checks: `["ci/woodpecker/pr/pr"]`, `strict: true`
  (branch must be up to date before merge);
- a pull request is required before merging (`required_approving_review_count: 0`);
- force pushes and branch deletion disabled;
- `enforce_admins: true` after the first exact-SHA `ci/woodpecker/pr/pr`
  green run. Before that run the bypass stays available (the required context
  had no publisher); it is closed immediately afterwards:

  ```sh
  gh api --method POST repos/sbelakho2/faktor/branches/main/protection/enforce_admins
  gh api repos/sbelakho2/faktor/branches/main/protection/enforce_admins --jq .enabled
  ```

To re-apply the exact configuration (idempotent PUT):

```sh
cat > /tmp/protection.json <<'JSON'
{
  "required_status_checks": { "strict": true, "contexts": ["ci/woodpecker/pr/pr"] },
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": false,
    "require_code_owner_reviews": false,
    "required_approving_review_count": 0
  },
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON
gh api --method PUT repos/sbelakho2/faktor/branches/main/protection --input /tmp/protection.json
```

To remove it entirely (only if the Woodpecker project is abandoned):
`gh api --method DELETE repos/sbelakho2/faktor/branches/main/protection`.

## 2. Woodpecker webhook and PR publication (local, applied)

The local operator instance is Woodpecker 3.18.1 on
`http://localhost:8010` (container `faktor-woodpecker-server`; SQLite in
`~/.woodpecker/data`, admin API token in `~/.woodpecker/api-token`, never
committed). Two agents are registered: `darwin/arm64` (local backend) and
`linux/amd64` (docker backend). The repository webhook exists and is active:

```sh
gh api repos/sbelakho2/faktor/hooks --jq '.[] | {id,active,events}'
gh api --method POST repos/sbelakho2/faktor/hooks/<id>/pings
gh api 'repos/sbelakho2/faktor/hooks/<id>/deliveries?per_page=1' --jq '.[0] | {event,status_code}'
```

The hook URL must point at the live tunnel origin and keep its
`?access_token=` query value. The quick tunnel hostname changes on restart, so
repair it fail-closed (extract the token from the existing hook, never print
it):

```sh
URL=$(grep -o 'https://[a-z-]*\.trycloudflare\.com' ~/.woodpecker/tunnel.log | tail -1)
# PATCH config.url to "$URL/api/hook?access_token=<existing token>"
```

**Upstream limitation (verified in 3.18.1 and 3.19.0 source):** the server
has no `?project=` selector and stores one project per forge repository.
`scripts/woodpecker/activate.sh`'s "two projects over one repository" model
requires a server build that implements that selector; this instance ignores
it and the lookup returns the same project id. Do not run the script's
trusted step against an instance that behaves this way — it would overwrite
the PR configuration.

The applied fail-closed sequence for one project (repo id 2) is:

1. Before pushing the candidate branch, PATCH the project to the untrusted
   shape: `config_file=.woodpecker/untrusted/`, `allow_pr=true`,
   `trusted.volumes=false`. This is the only shape that can run
   `pull_request` events; it can never mount the trusted volumes.
2. Push the branch and open the PR. Woodpecker publishes
   `ci/woodpecker/pr/pr` for the PR head SHA.
3. Only after the PR is green, PATCH the project back to the trusted shape:
   `config_file=.woodpecker/trusted/`, `allow_pr=false`,
   `trusted.volumes=true`.
4. Fast-forward the exact PR head onto `main`. The push event runs the full
   trusted matrix (linux + darwin + windows) at that same SHA; the aggregate
   `status-publish` runs only on `main`.
5. Re-assert branch protection and close the admin bypass.

Never leave `allow_pr=true` on a project that has `trusted.volumes=true`,
and never point the trusted project at `.woodpecker/untrusted/`.

If no webhook or App can be created with the available credentials, the
fail-closed stop condition is: do not weaken or remove the required context,
do not substitute a parent/merge SHA, and record the local trusted run by
triggering the exact commit on the local server via the API (`POST
/api/repos/<id>/pipelines` with a manual event is not a release lane; use the
forge webhook path or a `push`/`pull_request` payload). `certify.sh
--verify-ci-evidence` only accepts an observed pipeline for the exact SHA.

If the instance runs elsewhere (hosted CI), the required context string is
fixed by `WOODPECKER_STATUS_CONTEXT_FORMAT` and must stay
`ci/woodpecker/pr/pr`. Full walkthrough:
`scripts/woodpecker/setup.md` (§3 activation, §5 trust, §6 cron).

## 3. Commit signature (applied)

Resolution: keep signing with the registered SSH key but commit with the
account's associated noreply address in the repository-local config only
(no global change):

```sh
git config user.email "240468762+sbelakho2@users.noreply.github.com"
git config user.name "sabelakhoua"
```

The signing key is already registered on the account
(`gh api user/ssh_signing_keys`; ED25519
`SHA256:aE3x/e/26DPnlX19RR6P3foOCCPaNaL9iNEhH5Ar3s8`, title
`kiwicaptcha-commit-signing`). The previous `verified:false, reason:no_user`
came from `belakhoua.s@northeastern.edu` not being an associated account
email; that address remains unverified because the current `gh` token lacks
the `user` scope. If the northeastern address must verify instead, the
operator step is interactive and cannot be automated here:

```sh
gh auth refresh -h github.com -s user     # interactive browser flow
# Settings -> Emails -> add + verify belakhoua.s@northeastern.edu
```

Re-check the exact commit after pushing:

```sh
gh api repos/sbelakho2/faktor/commits/$(git rev-parse HEAD) --jq '.commit.verification'
```

The expected result is `{"verified":true,"reason":"valid"}`. Do not rewrite
or re-sign a commit after certification: the trusted-build attestation binds
the exact source SHA and tree.

## 4. Toolchain, images, vsce (applied; how to verify)

- Rust: `rust-toolchain.toml` is authoritative (`channel = "1.98.0"`,
  components `rustfmt`+`clippy`, `profile = "minimal"`), matching
  `Cargo.toml` `rust-version = "1.98.0"`. CI runs inside `rust:1.98` only
  as a rustup baseline; the linux lane asserts the resolution with
  `rustup show active-toolchain`, and the JetBrains smoke lane reads
  `channel` from the file for `rustup-init --default-toolchain`.
  Verify locally from the repo root: `rustup show active-toolchain`.
- Container images: EVERY image (including the Rust baseline) is pinned as
  `image:tag@sha256:<multi-arch index digest>` with the tag and recording
  date in a trailing comment. Digests recorded 2026-09-23/24 (cross-checked
  with `docker buildx imagetools inspect`):

  | Image | Digest |
  | --- | --- |
  | `rust:1.98.0` | `sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959` |
  | `node:24` | `sha256:64af3819f9275802414d7cdc38c27e9d82bd564dec4d4da87d008255d36c63b4` |
  | `faktor-ci` (local build, linux/amd64) | `sha256:bf7739456df1fc9aad818f4dc08990092f89d4c36785abedc14751441b143166` (`docker/faktor-ci/image-digest.txt`; rebuilt 2026-10-16: standalone kotlinc 2.4.20 re-published (the previously pinned digest had no registry copy and was lost with the operator VM reset)) |
  | `ubuntu:24.04` | `sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3` |
  | `alpine:3.20` | `sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc` |
  | `mcr.microsoft.com/playwright:v1.55.0-noble` (VS Code Extension Host E2E; Chromium runtime libs, no apt step) | `sha256:b27e719ecbfef153e13fd24e8341736733bf2658b229677eb21ff57ff5d7fb29` |
  | `bash:latest` | `sha256:61962062d969cb46dfc2bad061d36342406fa485f64f246aa7e95693ca07df1f` |
  | `woodpeckerci/woodpecker-server:v3` | `sha256:58dafbe56bb3529d78b48ee8d56a1f4b0886748763fd04ac23c01cc06c3dd24e` |
  | `woodpeckerci/woodpecker-agent:v3` | `sha256:73ee7cc63161b40bfefa4a26eae45c518a09124ab34f7e3c5e781df85e1ba4ac` |

  `rust:1.98.0` is a rustup baseline only — `rust-toolchain.toml` remains the
  Rust channel authority. `sh scripts/check-ci-image-pins.sh` enforces the
  pin in CI (the `image-pins` step in the PR and trusted workflows); the
  self-hosted shell pseudo-images (`powershell` on Windows, `bash` on the
  local-backend darwin agent) are the only documented `digest-exempt:`
  exceptions (no container image is pulled for either). Bump a digest
  deliberately with
  `docker buildx imagetools inspect <image:tag>`.
- Gradle: `apps/jetbrains/gradle/wrapper/gradle-wrapper.properties` pins
  `distributionSha256Sum` (published
  `gradle-9.7.1-bin.zip.sha256`), the committed wrapper JAR is validated in
  CI against the published `gradle-9.7.1-wrapper.jar.sha256`, and
  dependency verification metadata + per-subproject `gradle.lockfile`
  dependency locks are committed (`apps/jetbrains/gradle/wrapper/wrapper-checksums.txt`
  is the file of record). Verify with
  `bash scripts/check-gradle-integrity.sh`.
- vsce: `@vscode/vsce` is an exact devDependency (`4.0.0`) in
  `apps/vscode/package.json`, resolved through `apps/vscode/package-lock.json`;
  CI and `scripts/package-artifacts.sh` run `npx --no-install vsce package`
  (never `npx --yes @vscode/vsce`, which could fetch a mutable latest).
  Verify locally: `cd apps/vscode && npm ci && npx --no-install vsce --version`.
- VS Code E2E toolchain: `scripts/vscode-e2e.sh` records the release
  version (`1.140.0`), commit (`07f806f9…`), build id and SHA-256
  (`d32031e9…`) of the linux-x64 tarball; the SHA is verified before
  extraction and `code --version` must print the pinned version and commit,
  so a retagged/mutated download cannot run. The cached tarball lives on the
  `faktor-trusted-vscode-e2e-cache` volume. Re-pin deliberately with
  `curl https://update.code.visualstudio.com/api/update/linux-x64/stable/<version>`
  and update the constants (see §2.15 of `docs/certification.md`).
- Invariant criticality and planted-mutation proof: every `[[invariant]]` in
  `tests/invariants.toml` is release-critical by default because its
  `authority` names the production gate, and it must ship an executable
  `mutation_command` through `scripts/mutation-run.mjs` (the runner first
  requires the gate to pass on the pristine source, then requires the planted
  violation to make it fail, optionally matching a declared `expect`
  signature). A non-empty `mutation_debt` on a release-critical entry fails
  both `node scripts/check-invariants.mjs` (trusted/PR certificate steps) and
  `node scripts/check-invariants.mjs --mutations` (trusted `jetbrains-smoke`
  lane); only an explicit `release_critical = false` with a non-empty
  `criticality_reason` may keep debt, and it is reported as a warning. This
  closes the audit gap where a release-critical invariant could stay green
  while its production authority was no longer exercised.
- Lane-bound marker auth: the trusted linux and nightly campaign lanes emit
  their `faktor-woodpecker-lane/v2` markers through ONE helper
  (`scripts/certification/lane-marker.sh write --lane ...`, backed by the
  built Rust `faktor-ci-tools lane-marker` and the byte-identical Node
  reference `lane-marker.mjs`), which adds `auth = hmac-sha256:<hex>` over the
  canonical unsigned marker JSON keyed by the lane's own
  `faktor_lane_token_<lane>` CI secret (`-` -> `_`; uppercase accepted). The
  `certificate`/`certificate-nightly` step maps every lane token in its
  `environment:` and `evidence.mjs verify-markers` recomputes each MAC:
  wrong/missing auth for a configured token fails (`auth-mismatch` /
  `auth-missing`), malformed auth always fails, and a lane whose token is
  not configured is recorded `unattested` (visible, not silently passed)
  with no hard failure for PR/untrusted. A lane step only holds its own
  token, so one lane cannot mint another lane's marker; the selftest proves
  the forgery case. Register the tokens and uncomment the exact step
  `environment:` blocks per `scripts/woodpecker/setup.md` §4. Since a
  `from_secret` to a missing secret is a Woodpecker config error, the
  mappings ship commented: binding is opt-in per configured secret, and
  until enabled the certificate output shows which lanes are `unattested`.

Workflow YAML is validated with `woodpecker-cli lint .woodpecker/` (or the
container fallback in `scripts/woodpecker/setup.md` §13) plus the
dependency-light PyYAML parse gate in the same section; both run before any
CI change is pushed.

## 5. Trusted-build attestation and apt reproducibility

- **Attestation (P1-J).** The trusted workflow's `attestation` step (after
  `certificate`, `linux/amd64` only, `when.status: [success]`) writes a
  `faktor-build-attestation/v1` object and prints its base64 block into the
  step log: source/tree SHA, workflow, event, pipeline id/number, the
  attestation step's CI image digest (`build_environment_digest`), the
  `rust-toolchain.toml` channel, and `artifacts{name: sha256}` collected
  from the lane markers (VSIX, JetBrains zip) with every digest re-hashed at
  attestation time. `scripts/certify.sh` fetches that block for the EXACT
  trusted pipeline and refuses release certification on any mismatch.
  The step resolves the OBSERVED pipeline id from the Woodpecker API (repo
  lookup then `/pipelines/<CI_PIPELINE_NUMBER>`, take `id`) with the
  `WOODPECKER_HOST`/`WOODPECKER_TOKEN` available to trusted lanes and passes
  it as `--pipeline-id`; that lookup fails closed with the typed
  `observed-pipeline-id-lookup-failed` error instead of copying the pipeline
  number into the id field. Signing is **fail-closed** under the split
  allowlist (`faktor-signing-allowlist/v2`, `scripts/certification/
  attestation-keys.json`): the `attestation` step signs with the build role
  `faktor-ci-build` / `build-attestation` from the trusted project's
  `faktor_attest_signing_key` secret mapped to
  `FAKTOR_ATTEST_SIGN_KEY_PEM`, while the separate `release-signature` step
  signs `faktor-release-signature/v1` with the release role
  `faktor-release-signing` / `release-signing` from
  `faktor_release_signing_key` / `FAKTOR_RELEASE_SIGN_KEY_PEM` (a
  `from_secret` to a missing secret is a Woodpecker config error, so those
  mappings are documented, not hardcoded). Provision the release role on the
  operator machine with `node scripts/certification/attestation.mjs keygen
  --role release-signing --out-key release.pem --out-keys release-keys.json`
  and paste ONLY the public key into the allowlist; until then every
  release-class verification fails closed with the typed
  `signature-role-key-missing`, and a verifier asked for one role refuses a
  signature bound to the other with the typed `signature-role-mismatch`
  (`scripts/woodpecker/setup.md` §4.2). Operators receive the allowlist as
  `FAKTOR_ATTEST_KEYS`. WITHOUT the secret the step exits 3 with the typed
  `signing-key-missing` error and writes NO artifact — there is no unsigned
  code path. The documented alternative is Sigstore/keyless
  signing of the same payload against the pipeline OIDC identity
  (`docs/certification.md` §2.12). Preferred release model: distribute the
  CI-built artifacts the attestation covers, not local rebuilds.
- **apt reproducibility (P2-D, closed).** Every apt install in
  `.woodpecker/**` runs against a fixed Debian/Ubuntu snapshot timestamp
  (`20260923T000000Z`) with exact `pkg=version` pins; nothing reaches live
  distribution repositories. `scripts/check-ci-image-pins.sh` audits this:
  each apt-bearing step needs `# apt-snapshot: <timestamp> <reason>` (with
  the matching `snapshot.debian.org`/`snapshot.ubuntu.com` URL in the same
  step and exact-version installs) or `# apt-pinned: <image@sha256:...>`
  when the step runs on the dedicated Faktor CI image. The legacy
  `apt-residual` annotation is rejected outright. `docker/faktor-ci/` +
  `scripts/build-ci-image.sh` build the dedicated image (pinned base digest
  + snapshot + exact versions, including `git` and `openjdk-17-jdk-headless`
  for the JetBrains lanes); the PR and trusted JetBrains build/smoke lanes
  run on the recorded `image@sha256:<digest>`. See
  `docs/certification.md` §2.14.

## 6. Context registry for release certification

`scripts/certify.sh` accepts only these contexts (immutable registry;
`--context` selects, never invents):

| Context | Event | Workflow | Class |
| --- | --- | --- | --- |
| `ci/woodpecker/pr/pr` | `pull_request` | `pr` | untrusted |
| `ci/woodpecker/push/trusted` | `push` | `trusted` | trusted |
| `ci/woodpecker/tag/trusted` | `tag` | `trusted` | trusted |

The verifier observes repository/config-file/trusted-class, exact SHA,
actual event, actual workflow state, pipeline status and pipeline
id/number from the Woodpecker API; the certificate records those observed
values. `--verify-ci-evidence` (renamed from `--ci-only`, which is now
rejected) always terminates with `CI EVIDENCE: PASS — NOT A RELEASE
CERTIFICATE`; a release certificate additionally requires the full local
  gates plus a trusted context or a verified signed attestation.
