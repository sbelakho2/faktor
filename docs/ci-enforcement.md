# CI enforcement runbook

Status of the release/toolchain enforcement surfaces for
`sbelakho2/faktor` (GitHub remote `origin`), as of 2026-09-23. "Applied"
means the action was executed from this checkout via `gh`; "Operator" means
it needs credentials or a server this checkout does not have.

| Surface | State | Where |
| --- | --- | --- |
| `rust-toolchain.toml` pinned channel 1.98.0 (+rustfmt/clippy, minimal) | Applied | `rust-toolchain.toml` |
| CI resolves the toolchain file (linux lane asserts it; JetBrains smoke derives `--default-toolchain` from it) | Applied | `.woodpecker/**` |
| Container images pinned by multi-arch index digest (non-Rust) | Applied | `.woodpecker/trusted/trusted.yaml`, `.woodpecker/untrusted/pr.yaml`, `.woodpecker/trusted/nightly.yaml`, `scripts/woodpecker/docker-compose.yml` |
| `@vscode/vsce` pinned exactly in the VS Code lockfile and run as the locked binary | Applied | `apps/vscode/package.json`, `apps/vscode/package-lock.json`, `.woodpecker/**`, `scripts/package-artifacts.sh` |
| Branch protection on `main` requires `ci/woodpecker/pr/pr` (strict) and the PR path | Applied | GitHub API |
| Woodpecker publishes `ci/woodpecker/pr/pr` for this repo | **Operator** | Woodpecker server |
| Commit signature shows `verified:true` on GitHub | **Operator** | GitHub account email |

## 1. Branch protection (applied)

`main` now has (verified with
`gh api repos/sbelakho2/faktor/branches/main`):

- required status checks: `["ci/woodpecker/pr/pr"]`, `strict: true`
  (branch must be up to date before merge);
- a pull request is required before merging (`required_approving_review_count: 0`);
- force pushes and branch deletion disabled;
- `enforce_admins: false`: the repository admin retains a bypass. This is
  deliberate until the Woodpecker PR context actually publishes on this repo
  (below); with a required context that no one publishes, the bypass is the
  only escape hatch. Once the first green `ci/woodpecker/pr/pr` run is
  observed, tighten it:

  ```sh
  gh api --method POST repos/sbelakho2/faktor/branches/main/protection/enforce_admins
  ```

To re-apply the exact configuration (idempotent PUT):

```sh
cat > /tmp/protection.json <<'JSON'
{
  "required_status_checks": { "strict": true, "contexts": ["ci/woodpecker/pr/pr"] },
  "enforce_admins": false,
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

## 2. Woodpecker PR publication (operator action; residual)

Right now the repository has **no Woodpecker webhook and no GitHub App
installation** (`gh api repos/sbelakho2/faktor/hooks` is `[]`), so no run
ever publishes `ci/woodpecker/pr/pr`. The in-tree certificate therefore
still cannot turn green on GitHub: the required check stays "Expected —
waiting" until a Woodpecker instance is connected to the repo.

Connect it with the in-tree activation script (Woodpecker 3.x server + an
instance admin token; the `--trusted` flag requests `trusted.volumes` for the
caches and needs the admin token):

```sh
WOODPECKER_HOST=https://<your-woodpecker-host> WOODPECKER_TOKEN=<admin-pat> \
  bash scripts/woodpecker/activate.sh sbelakho2/faktor --trusted --timeout-minutes 1560
bash scripts/woodpecker/verify-boundary.sh sbelakho2/faktor   # offline layout + server settings
```

This creates the two projects (untrusted `.woodpecker/untrusted/` with
`allow_pr: true`; trusted `.woodpecker/trusted/` with `trusted.volumes: true`)
and registers the `nightly` cron; the server then installs the GitHub webhook
and publishes commit statuses with the default context format, including
`ci/woodpecker/pr/pr` on pull requests. Full walkthrough:
`scripts/woodpecker/setup.md` (§3 activation, §5 trust, §6 cron).

If the instance runs elsewhere (hosted CI), only the webhook/publication part
matters: the required context string is fixed by
`WOODPECKER_STATUS_CONTEXT_FORMAT` and must stay
`ci/woodpecker/pr/pr`.

## 3. Commit signature (operator action; residual)

Diagnosis of HEAD (`3c67f794`, same for the pushed `main`):

- `git log -1 --show-signature` -> `Good "git" signature ... ED25519 key
  SHA256:aE3x/e/26DPnlX19RR6P3foOCCPaNaL9iNEhH5Ar3s8`;
  `git config` signs with `/Users/sabelakhoua/.ssh/kiwicaptcha_signing`
  (`gpg.format ssh`, `commit.gpgsign true`) and uses
  `user.email belakhoua.s@northeastern.edu`.
- `gh api user/ssh_signing_keys` contains exactly that fingerprint (title
  `kiwicaptcha-commit-signing`, id 1115429), so the signing key **is**
  registered on the GitHub account.
- GitHub still reports `verified:false, reason:"no_user"` and the commit's
  `author`/`committer` fields are `null` — the commit email
  `belakhoua.s@northeastern.edu` is not an associated/verified email on
  account `sbelakho2`. For SSH signatures GitHub requires both the key and
  the committer email to belong to the account.

Operator options (no git config was changed by this hardening pass):

1. Add and verify `belakhoua.s@northeastern.edu` in GitHub
   Settings -> Emails, then re-run any signed commit; verification turns
   green. The current `gh` token lacks the `user` scope, so this cannot be
   done from here (`gh auth refresh -h github.com -s user` is interactive).
2. Or keep signing with the same key but use a verified address, e.g. the
   account noreply address:
   `git config --global user.email "240468762+sbelakho2@users.noreply.github.com"`.
   Decide this deliberately: it is a global git-config change, not applied
   here.

Re-check with:

```sh
gh api repos/sbelakho2/faktor/commits/$(git rev-parse HEAD) --jq '.commit.verification'
```

## 4. Toolchain, images, vsce (applied; how to verify)

- Rust: `rust-toolchain.toml` is authoritative (`channel = "1.98.0"`,
  components `rustfmt`+`clippy`, `profile = "minimal"`), matching
  `Cargo.toml` `rust-version = "1.98.0"`. CI runs inside `rust:1.98` only
  as a rustup baseline; the linux lane asserts the resolution with
  `rustup show active-toolchain`, and the JetBrains smoke lane reads
  `channel` from the file for `rustup-init --default-toolchain`.
  Verify locally from the repo root: `rustup show active-toolchain`.
- Container images: non-Rust images are pinned as
  `image:tag@sha256:<multi-arch index digest>` with the tag and recording
  date in a trailing comment. Digests recorded 2026-09-23 (cross-checked
  with `docker buildx imagetools inspect`):

  | Image | Digest |
  | --- | --- |
  | `node:24` | `sha256:64af3819f9275802414d7cdc38c27e9d82bd564dec4d4da87d008255d36c63b4` |
  | `eclipse-temurin:17-jdk` | `sha256:bc033b57e11b773c3043babfd664e7a5ef110805548b921cbfc3e8c67a0725d6` |
  | `ubuntu:24.04` | `sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3` |
  | `alpine:3.20` | `sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc` |
  | `bash:latest` | `sha256:61962062d969cb46dfc2bad061d36342406fa485f64f246aa7e95693ca07df1f` |
  | `woodpeckerci/woodpecker-server:v3` | `sha256:58dafbe56bb3529d78b48ee8d56a1f4b0886748763fd04ac23c01cc06c3dd24e` |
  | `woodpeckerci/woodpecker-agent:v3` | `sha256:73ee7cc63161b40bfefa4a26eae45c518a09124ab34f7e3c5e781df85e1ba4ac` |

  `rust:1.98` intentionally stays tag-based (the toolchain file is the Rust
  version authority); `powershell` stays tag-based because no `docker.io`
  library image exists to record a digest from (self-hosted Windows agents
  provide it). Bump a digest deliberately with
  `docker buildx imagetools inspect <image:tag>`.
- vsce: `@vscode/vsce` is an exact devDependency (`4.0.0`) in
  `apps/vscode/package.json`, resolved through `apps/vscode/package-lock.json`;
  CI and `scripts/package-artifacts.sh` run `npx --no-install vsce package`
  (never `npx --yes @vscode/vsce`, which could fetch a mutable latest).
  Verify locally: `cd apps/vscode && npm ci && npx --no-install vsce --version`.

Workflow YAML is validated with `woodpecker-cli lint .woodpecker/` (or the
container fallback in `scripts/woodpecker/setup.md` §13) plus the
dependency-light PyYAML parse gate in the same section; both run before any
CI change is pushed.
