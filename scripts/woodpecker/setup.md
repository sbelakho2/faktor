# Woodpecker CI setup

The CI for this repository is defined by the workflow files in
[`.woodpecker/`](../../.woodpecker) and runs on
[Woodpecker CI](https://woodpecker-ci.org). This directory holds a local
server/agent stack (`docker-compose.yml`), the activation script
(`activate.sh`), the boundary verifier (`verify-boundary.sh`) and the
operator notes below.

Targeted version: **Woodpecker 3.x** (verified against the 3.18 documentation
and the v3.18.0 API; the syntax also validates against the 2.8 subset it
uses: matrix, labels, steps, workflow-level `when`, `depends_on`, status
filters, cron filters).

## 1. Pipeline model: two projects, one repository

The repository is enabled **twice** on the same Woodpecker instance, as two
projects with different server-side trust and different pipeline paths:

| Project | Server-side settings | Pipeline path | Events | Workflows |
| --- | --- | --- | --- | --- |
| **untrusted** | `trusted.volumes=false`, `allow_pr=true` | `.woodpecker/untrusted/` | `pull_request` | `pr.yaml` (`pr`) |
| **trusted** | `trusted.volumes=true` (admin), `allow_pr=false` | `.woodpecker/trusted/` | `push`, `tag`, `cron` | `trusted.yaml` (`trusted`) + `nightly.yaml` (`nightly`) |

`pull_request` events only ever reach the untrusted project, and Woodpecker
refuses `volumes:` at policy level for a project that is not trusted. The
trusted project never loads the PR workflow because its *pipeline path* — a
project setting on the server, not a repository file — points at
`.woodpecker/trusted/` only.

There are **no workflow files at the `.woodpecker/` top level**, so the
default resolution (`.woodpecker/*.{yaml,yml}` -> `.woodpecker.yaml` ->
`.woodpecker.yml`, used when *Pipeline path* is empty) finds nothing: an
unset path fails closed instead of silently loading another project's
workflow. `.woodpecker/pr.yaml`, `trusted.yaml` and `nightly.yaml` are
in-repo symlinks to the real files, kept only so repository tooling that
references the historical paths (`scripts/capabilities-manifest.mjs`, the
evidence selftest) keeps working; they must never be selected as a project
pipeline path.

| File (workflow) | Project | Event filter | Storage | Contents |
| --- | --- | --- | --- | --- |
| `.woodpecker/untrusted/pr.yaml` (`pr`) | untrusted | `pull_request` | **no volumes at all** | reduced lane set: `storage-policy`, `linux`, `static`, `docs`, `vscode`, `jetbrains-build`, `jetbrains-smoke`, then the aggregate `certificate` |
| `.woodpecker/trusted/trusted.yaml` (`trusted`) | trusted | `push` (any branch) + `tag` (linux); `push` to `main` for darwin/windows | trusted named volumes `faktor-trusted-*` | full linux lane set incl. release `[perf]` + `certificate`; darwin/windows matrix combos + per-platform certificates |
| `.woodpecker/trusted/nightly.yaml` (`nightly`) | trusted | `cron` job `nightly` | own `faktor-nightly-*` volumes | `[fault]` at scale, longrun, efficiency, economy, coding-benchmark smoke, provider-key real-model run (recorded skip by default), supply-chain, then `certificate-nightly` |

`labels: platform: ${platform}` (trusted) or `labels: platform: linux/amd64`
(pr/cron) routes workflows to agents. The deprecated `runs_on` key is **not**
an agent selector in Woodpecker 2.x/3.x: it feeds the run-on-success/failure
status of dependent tasks, and `runs_on: [macos]` would make the workflow
match no status and never run. Agent selection is `labels`.

The darwin and windows combos are gated to `push` on `main` (change
`branch: main` in `.woodpecker/trusted/trusted.yaml` if your default branch
differs).

## 2. Start the server

```sh
cd scripts/woodpecker
export WOODPECKER_AGENT_SECRET="$(openssl rand -hex 32)"   # shared by server + agents
export WOODPECKER_HOST="http://localhost:8000"             # public URL of this server
export WOODPECKER_GITHUB_CLIENT="<oauth app client id>"
export WOODPECKER_GITHUB_SECRET="<oauth app client secret>"
docker compose up -d
```

The compose file starts `woodpecker-server` (port 8000 for UI/API, port 9000
for the agent gRPC channel, data in the `woodpecker-data` volume) and one
linux docker agent. It also raises `WOODPECKER_MAX_PIPELINE_TIMEOUT` (default
1560 minutes / 26h) so the nightly longrun campaign can be admitted; see §6.
Port 9000 must be reachable from the macOS/Windows agent hosts and should be
firewalled otherwise (only the shared agent secret crosses it). Server state
survives
`docker compose down`; use `down -v` only to reset it.

Create a GitHub OAuth application for the forge connection (Settings →
Developer settings → OAuth Apps):

- Homepage URL: `$WOODPECKER_HOST`
- Authorization callback URL: `$WOODPECKER_HOST/authorize`

## 3. Activate the repository (two projects)

**Forge webhook and commit-status behavior.** Activating a repository in
Woodpecker installs the forge webhook and starts processing its events:
`push`, `tag` and `pull_request` (the webhook for `pull_request` is only
honored while *Allow pull requests* is enabled, which the untrusted project
keeps enabled and the trusted project disables). Pipelines are triggered per
event and each **workflow file** posts its own commit status; with the default
server settings (`WOODPECKER_STATUS_CONTEXT` = `ci/woodpecker`,
`WOODPECKER_STATUS_CONTEXT_FORMAT` =
`{{ .context }}/{{ .event }}/{{ .workflow }}`) the contexts are:

| Event | Project | Context |
| --- | --- | --- |
| pull request | untrusted | `ci/woodpecker/pr/pr` |
| push (any branch) | trusted | `ci/woodpecker/push/trusted` |
| tag | trusted | `ci/woodpecker/tag/trusted` |
| cron | trusted | `ci/woodpecker/cron/nightly` |

A workflow excluded by its `when` filter posts nothing. Server overrides of
`WOODPECKER_STATUS_CONTEXT(_FORMAT)` rename every context; use the resulting
strings in branch protection.

### 3a. UI steps (no account scripts required)

1. Log into Woodpecker with an admin/owner account and enable the repository
   (*Repositories → Add*). This first project is the **untrusted** one.
2. Untrusted project settings: **Pipeline path** = `.woodpecker/untrusted/`,
   *Allow pull requests* = on, *Trusted* = off (leave the admin-only default
   off; never enable it here).
3. Create the **trusted** project over the same repository. `activate.sh`
   performs the project-scoped activation/lookup (`?project=trusted`); the UI
   equivalent depends on whether your instance supports several projects per
   repository. If it does not, stop — a single trusted project cannot run
   `pull_request` events without granting them volume access (§5).
4. Trusted project settings: **Pipeline path** = `.woodpecker/trusted/`,
   *Allow pull requests* = off, *Trusted* = on (server admin only). Register
   the `nightly` cron here (§6).
5. Confirm the webhook delivers push, tag and pull_request events, and keep
   *Require approval for forked repositories* enabled (Woodpecker default).

### 3b. Scripted activation (`activate.sh`)

```sh
export WOODPECKER_HOST="https://<your-instance>"     # no trailing slash
export WOODPECKER_TOKEN="<personal access token>"    # Woodpecker UI -> user settings
bash scripts/woodpecker/activate.sh <owner/repo> \
  --timeout-minutes 1560 \
  --trusted                 # instance admin only; grants trusted.volumes to the TRUSTED project
```

The script is idempotent and uses the Woodpecker 3.x API. It configures the
untrusted project **first** (`trusted.volumes=false`, `allow_pr=true`,
`config_file=.woodpecker/untrusted/`) and then the trusted project
(`config_file=.woodpecker/trusted/`, `allow_pr=false`,
`trusted.volumes=true` with `--trusted`), and it registers cron `nightly`
on the trusted project only:

- `GET /api/user` — token check;
- `GET /api/repos/lookup/<owner>/<repo>[?project=trusted]` — resolve each
  project when it already exists;
- `POST /api/repos?forge_remote_id=<id>[&project=trusted]` — activate the
  repository (untrusted project) and create the second, trusted project
  (409/refusal = create it in the UI and pass `--trusted-repo-id`);
  `<id>` comes from `FORGE_REMOTE_ID` or `gh api repos/<owner>/<repo> --jq .id`;
- `PATCH /api/repos/<untrusted_id>` `{"config_file":".woodpecker/untrusted/",
  "allow_pr":true,"trusted":{"volumes":false}}`;
- `PATCH /api/repos/<trusted_id>` `{"config_file":".woodpecker/trusted/",
  "allow_pr":false,"trusted":{"volumes":true}}` (the volumes grant is
  instance-admin only; without `--trusted` the script reports the stored
  value and what still needs granting);
- `PATCH /api/repos/<trusted_id> {"timeout":<minutes>}` — the pipeline timeout
  (needed by the nightly longrun campaign; see §6);
- `POST|PATCH /api/repos/<trusted_id>/secrets[/<name>]` — secrets (`--secret
  NAME=VALUE`, repeatable; **none are required by default**, and the untrusted
  project never gets secrets);
- `GET|POST|PATCH /api/repos/<trusted_id>/cron` — register/patch the
  `nightly` cron job (§6);
- `--run-now` triggers the job once after registration;
- the script prints exactly which server-side settings are required and the
  branch-protection setup for `ci/woodpecker/pr/pr` (§7). Use `--dry-run` to
  print every call without sending it, and `--untrusted-repo-id`/
  `--trusted-repo-id` (or `WOODPECKER_UNTRUSTED_REPO_ID`/
  `WOODPECKER_TRUSTED_REPO_ID`) to skip lookup/creation.

### 3c. Activation status in this checkout

**Activation could not be executed from this machine:** there is no
`WOODPECKER_HOST`/`WOODPECKER_TOKEN` in the environment, no Woodpecker
instance reachable on `localhost:8000`, and no server credentials of any kind
were available. Nothing in this repository has been enabled on an instance by
this change. The exact commands to run somewhere with credentials are:

```sh
export WOODPECKER_HOST="https://<your-instance>"
export WOODPECKER_TOKEN="<personal access token>"
bash scripts/woodpecker/activate.sh <owner/repo> --timeout-minutes 1560 --trusted --run-now
# then verify the boundary (layout guards + both server-side projects):
bash scripts/woodpecker/verify-boundary.sh <owner/repo>
```

`--dry-run` on either script prints every planned API call without sending
one. If the API path differs on a future Woodpecker minor, use the UI
equivalents (Project settings, Cron Jobs) — §3a, §4 and §6 give the exact
fields.

## 4. Secrets

**None are required for the default pipelines.** All gates are deterministic
and offline-capable apart from image/package downloads (Rust crates, npm
packages, Gradle distribution/toolchain downloads).
Provider-key (real-model) runs are deliberately not part of the PR/trusted
pipelines; the nightly `coding-benchmark-real-model` lane records an explicit
skip marker unless `FAKTOR_BENCH_PROVIDER`, `FAKTOR_BENCH_MODEL` and
`FAKTOR_BENCH_API_KEY` are present in the step environment. Note that a
`from_secret` reference to a secret that does not exist is a config compile
error in Woodpecker, so the workflow does not reference secrets blindly. To
enable real nightly runs, either provide those variables to the agent's step
environment (self-hosted) or register project secrets with
`activate.sh --secret NAME=VALUE` and add explicit
`environment: {FAKTOR_BENCH_API_KEY: {from_secret: ...}}` entries to the lane
in `.woodpecker/trusted/nightly.yaml`.

Secrets are registered on the **trusted project only** (`activate.sh` targets
it): a secret that exists for the untrusted PR project would be readable by
unreviewed PR code. The untrusted project needs no secrets.

## 5. The untrusted-PR / trusted-push trust boundary

Woodpecker only allows `volumes:` when a project is marked **Trusted
(volumes)** by a server admin (Project settings → Trusted; the *Trusted*
section is admin-only). Trust is per **project** and lives on the server, so
the CI is split into two projects over the one repository:

- the **untrusted project** (pipeline path `.woodpecker/untrusted/`,
  `trusted.volumes=false`, `allow_pr=true`) is the only project that handles
  `pull_request` events. Woodpecker refuses volume mounts for it at policy
  level, so a PR cannot obtain a volume even if it rewrites its own YAML.
- the **trusted project** (pipeline path `.woodpecker/trusted/`,
  `trusted.volumes=true`, `allow_pr=false`) runs `push`/`tag`
  (`trusted.yaml`) and the `nightly` cron (`nightly.yaml`). It never loads
  the PR workflow.

The invariant: **trust and pipeline paths are project settings on the server,
and a PR diff cannot change them.** A PR cannot mark a project trusted, cannot
move the trusted project onto the PR YAML, and cannot add a file that a
project loads outside its configured path. `.woodpecker/pr.yaml`,
`.woodpecker/trusted.yaml` and `.woodpecker/nightly.yaml` at the top level
are in-repo symlinks for tooling only; with both pipeline paths set
explicitly they are never selected, and the default resolution must stay
unused.

Defense-in-depth (not the boundary):

- `.woodpecker/untrusted/pr.yaml` declares zero volumes, and its
  `storage-policy` step plus the PR certificate fail if a `volumes:` key
  appears, if a `faktor-trusted-*`/`faktor-nightly-*` reference appears, or
  if a workflow file appears at the `.woodpecker/` top level. A PR can delete
  this guard; it cannot change the project settings.
- `.woodpecker/trusted/nightly.yaml` uses its own `faktor-nightly-*` volumes
  so a heavy campaign can never corrupt the caches a trusted build reuses.
- `scripts/woodpecker/verify-boundary.sh [owner/repo]` re-checks the layout
  offline and, with `WOODPECKER_HOST`/`WOODPECKER_TOKEN`, both projects'
  server-side settings.
- No CI step pipes a remote script into a shell: the JetBrains smoke
  bootstraps Rust from the pinned rustup-init binary after verifying its
  published SHA-256, and the toolchain comes from pinned image tags
  (`rust:1.98`, `node:24`, `ubuntu:24.04`). The remaining PR fetches are
  package-manager downloads (`npx @vscode/vsce`); they never pipe to a
  shell, and their output is gated by the VSIX panel-surface verifier and
  the packaged selftest.
  Audit with `rg -n '\|[[:space:]]*(bash|sh)([[:space:]]|$)' .woodpecker/`.

**Residual risks (must stay documented):**

- A collaborator `push` runs collaborator-authored code with volumes
  available by design; the isolation is PR-vs-push, not author-vs-author.
- An operator who leaves a project pipeline path empty would fall back to the
  default resolution; the top level deliberately holds no regular workflow
  files, so that fails closed, and `activate.sh`/`verify-boundary.sh` say so
  loudly.
- If the instance cannot grant trusted status or cannot host two projects,
  run everything without volumes: delete every `volumes:` block and
  `CARGO_TARGET_DIR` entry from `.woodpecker/trusted/` (everything still
  runs, just without warm caches) and keep the PR project untrusted.
  **Never enable trusted volumes on the PR project.**
- Keep *Require approval for forked repositories* enabled (Woodpecker
  default) and review `.woodpecker/` changes in PRs.

## 6. Cron job (`nightly`)

`.woodpecker/trusted/nightly.yaml` only runs for a **cron event whose job
name matches `when.cron`**. Register the job **on the trusted project only**
(the untrusted PR project must have no cron job — a cron event is
server-owned and would otherwise be one more way to reach caches). `activate.sh`
targets the trusted project:

- UI: trusted project → Project settings → **Cron Jobs** → *Add*, with
  `nightly`: schedule `0 3 * * *`, branch `main`, timezone `UTC`.
- API/script: `bash scripts/woodpecker/activate.sh <owner/repo> --run-now`
  (idempotent; it creates or patches the job and can trigger it once),
  equivalent to
  `POST /api/repos/{trusted_repo_id}/cron` with
  `{"name":"nightly","schedule":"0 3 * * *","branch":"main","timezone":"UTC","enabled":true}`.

Supported schedule syntax: standard 5-field cron plus `@daily`, `@weekly`,
`@every 5m`, ... (see the Woodpecker Cron doc).

**Timeout:** Woodpecker has no per-step timeout; pipelines are capped by the
project timeout (default 60 min; the settable maximum defaults to
`WOODPECKER_MAX_PIPELINE_TIMEOUT` = 120 min). The nightly longrun campaign
therefore needs:

```sh
# server/agent environment (docker-compose.yml sets this by default)
WOODPECKER_MAX_PIPELINE_TIMEOUT=1560
# trusted project setting
bash scripts/woodpecker/activate.sh <owner/repo> --timeout-minutes 1560
```

On hosted `woodpecker-ci.org` the server cap is fixed and cannot be raised by
a user; run the nightly cron on a self-hosted instance (or accept that long
campaign lanes are cut off at the cap) until that changes.

## 7. Branch protection / required status checks

Woodpecker reports one commit status per workflow, so the required check for
PRs is the `pr` workflow context:

- GitHub UI: Settings → Branches → Add branch protection rule for `main` →
  *Require status checks to pass* → search for and add `ci/woodpecker/pr/pr`.
- API:

  ```sh
  gh api --method PUT repos/<owner>/<repo>/branches/main/protection --input - <<'JSON'
  {
    "required_status_checks": {
      "strict": true,
      "contexts": ["ci/woodpecker/pr/pr"]
    },
    "enforce_admins": false,
    "required_pull_request_reviews": null,
    "restrictions": null
  }
  JSON
  ```

`activate.sh` prints the same instructions after activation. The push/tag
contexts (`ci/woodpecker/push/trusted`, `ci/woodpecker/tag/trusted`) can be
required too if you want the post-merge trusted evidence checked before other
work lands; the darwin/windows per-platform certificates run inside that same
`trusted` workflow, so their failure also fails `ci/woodpecker/push/trusted`.

## 8. Linux agent

The compose `woodpecker-agent` registers with
`WOODPECKER_AGENT_LABELS=platform=linux/amd64`. Its default labels are
`hostname`, `platform`, `backend`, `repo=*`; a custom `platform` overrides the
backend-derived one, which is exactly what the workflows need.

- Docker backend requirements: Docker Engine on the agent host, the workspace
  needs ~40 GB free disk for the parallel lanes.
- Recommended sizing: **≥ 8 vCPU, 16 GB RAM, 60 GB disk**. The mandatory
  lanes run in parallel inside one workflow; `perf` is serialized after the
  other Rust lanes so release budget assertions do not race a loaded agent.
  Cron lanes use separate target volumes so their parallel cargo processes
  do not contend.
- To run several pipelines at once raise `WOODPECKER_MAX_WORKFLOWS`; the lane
  set is resource-heavy, so keep it low on shared hosts.

## 9. macOS and Windows agents (self-hosted, `local` backend)

Hosted `woodpecker-ci.org` and most Docker-agent deployments are linux-only.
The `darwin/*` and `windows/*` combos need an agent on that OS:

1. Install the Woodpecker agent binary (GitHub releases, `woodpecker-agent`
   for the host platform) and the required toolchain:
   - macOS: Rust stable + Xcode command line tools (`cargo` on `PATH`).
   - Windows: Rust stable (MSVC toolchain + Visual Studio Build Tools) with
     `cargo` on `PATH`.
2. The `local` backend runs steps directly on the host and needs the clone
   plugin binary (`woodpeckerci/plugin-git`) on `PATH` so the default clone
   step works; install it from its release page and confirm `plugin-git
   --help` succeeds.
3. Start the agent with labels matching the matrix values:

   ```sh
   # macOS (Apple Silicon)
   WOODPECKER_SERVER=http://<server-host>:9000 \
   WOODPECKER_AGENT_SECRET=<same secret as the server> \
   WOODPECKER_BACKEND=local \
   WOODPECKER_MAX_WORKFLOWS=1 \
   WOODPECKER_AGENT_LABELS=platform=darwin/arm64,hostname=faktor-darwin-agent \
   woodpecker-agent
   ```

   ```powershell
   # Windows (PowerShell)
   $env:WOODPECKER_SERVER = "http://<server-host>:9000"
   $env:WOODPECKER_AGENT_SECRET = "<same secret as the server>"
   $env:WOODPECKER_BACKEND = "local"
   $env:WOODPECKER_MAX_WORKFLOWS = "1"
   $env:WOODPECKER_AGENT_LABELS = "platform=windows/amd64,hostname=faktor-windows-agent"
   woodpecker-agent.exe
   ```

4. Intel Macs use `darwin/amd64`; keep the value in sync with the
   `.woodpecker/trusted/trusted.yaml` matrix entry. Until an agent exists,
   the darwin/windows combos stay queued (only on `push` to `main`) and no
   linux job is affected; if you never plan to add one, remove those matrix
   entries and the matching jobs from the config.

## 10. Certificate and aggregation

Woodpecker workflows are filesystem-isolated, so cross-workflow file markers
are not possible; the aggregate gate therefore lives **inside** each workflow:

- every lane ends by writing
  `target/certification/lanes/<lane>.json`
  (`{"schema":"faktor-woodpecker-lane/v2","lane":...,"status":"passed","commit":...,"tree":...,"runner":{...},"commands_digest":...}`);
- each `certificate`/`certificate-nightly` step
  `depends_on` every lane of its workflow, runs with
  `when.status: [success, failure]` (a dependent is otherwise skipped when a
  dependency fails), and fails when any marker is missing, marked failed,
  written for a foreign commit, or unexpected; it also passes
  `--yaml-dir .woodpecker/untrusted` or `.woodpecker/trusted` so the
  command-set drift check runs against its own file set (not the symlinks);
- it additionally checks the runtime's own `CI_PIPELINE_STATUS`, which is
  `failure` when any earlier step failed, and writes
  `target/certification/ci-certification.json`
  (`faktor-ci-certification/v1`, with `workflow` recorded).
- The only accepted non-`passed` marker is `skipped` on
  `coding-benchmark-real-model` in the nightly certificate, and only with a
  non-empty `reason` — a silent skip is a failure.

The darwin and windows combos carry their own `certificate-darwin` /
`certificate-windows` steps with the same marker/status rule. Woodpecker has
no cross-matrix summary job (upstream issue #2886), so the workflow-level
contexts are what branch protection requires (§7).

## 11. Cloud tier

[woodpecker-ci.org](https://woodpecker-ci.org) offers a free cloud tier for
open-source repositories; it provides **linux runners only**, and its
`WOODPECKER_MAX_PIPELINE_TIMEOUT` is fixed. The `pr` workflow (including the
required render gate and the aggregate `certificate`) and the `trusted`
linux lanes run there without any agent setup; the darwin/windows combos need
your own agents as in §9 (or removal of those matrix entries), and the nightly
cron needs a self-hosted instance because of the timeout cap (§6).

## 12. Local/offline certificate

CI covers the platform lanes; `bash scripts/certify-local.sh fast`
(and `full`) remains the **local/offline certificate** for the host and emits
`target/certification/manifest.json`. Neither replaces the other: see
`docs/certification.md` for the levels and the release rule.

## 13. Linting the configs locally

```sh
woodpecker-cli lint .woodpecker/            # one pass over all workflow files
bash scripts/woodpecker/verify-boundary.sh  # layout guards (offline) + server settings (with credentials)
python3 - <<'PY'                            # dependency-light parse gate
import glob, yaml
for f in sorted(glob.glob(".woodpecker/**/*.yml", recursive=True) + glob.glob(".woodpecker/**/*.yaml", recursive=True)):
    yaml.safe_load(open(f))
    print("ok", f)
PY
```
