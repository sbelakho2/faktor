# Faktor

**Same Kilo Code UX. A substantially better native engine.**

Faktor replaces the Kilo Code engine (TypeScript/Bun) with a native Rust
runtime. The v7.5.6 VS Code webview sources are vendored under `ui/` as a
frontend source dependency (pinned at commit `fa02955` with a SHA-256
manifest) and served through `apps/vscode`; the pinned closure plus the
Faktor companion overlay are staged into the extension's `media/` and
ship inside the VSIX. Wire/behavior parity with Kilo is not a release
objective: the daemon speaks its own native protocol. The JetBrains 7.1.2
sources are vendored at
`compat/jetbrains-712/` (tag `jetbrains/v7.1.2`, commit
`436ff09e649bd0866c84bd9f98933a74cad2d25c`, also SHA-256-pinned in
`ui/upstream.json`); `apps/jetbrains` carries the real Faktor-owned
frontend/backend panels (task tree, blockers, tournament, board, evidence,
attachments) that talk to the native daemon.

```
same UI
   ↓
Faktor native protocol bridge
   ↓
native Rust engineering runtime
   ↓
LLM used only where reasoning is actually needed
```

## Architecture at a glance

- **Durable state machine** — every session is an explicit state machine fed by
  an append-only event journal. No `await Promise` implicitly defines
  application state. On daemon restart, unfinished operations are reconstructed
  from durable state.
- **Bounded context** — five separate memory classes (immutable instructions,
  durable task state, repository knowledge, recent conversation, historical
  artifacts). Compaction cannot enter a death spiral: a successful compaction
  must achieve a configured minimum reduction or it is rejected.
- **Native checkpoints** — content-addressed (BLAKE3 + Zstd) snapshot store
  instead of Git repositories pretending to be undo history. Git stays for
  branches/commits/worktrees/diffs only.
- **Transactional editing** — every agent edit is optimistic and versioned
  against `expected_hash`; parse-before-accept; atomic writes; no old patch
  applied to unexpected contents.
- **Hybrid retrieval** — exact + lexical + symbol + optional semantic search
  fused by rank; automatic retrieval is a TARGET (the machinery exists; the
  production daemon wiring is in progress).
- **Explicit concurrency** — resource-class budgets, dependency DAG scheduling,
  state-aware retries with jitter, circuit breakers.
- **Process supervision** — no orphans. Process groups on Unix; Windows
  creates a kill-on-close Job Object per supervised child
  (`CreateJobObjectW` + `AssignProcessToJobObject` +
  `SetInformationJobObject` in `crates/winjob`, wired through
  `faktor-terminal`) with `taskkill /T` as the escalation path. The session
  PTY authority (`crates/pty`, ACP-negotiated `faktor.terminal`) spawns
  ConPTY children `CREATE_SUSPENDED`, assigns them to a
  `KILL_ON_JOB_CLOSE` job, then resumes: no child runs outside the job.
- **Provider normalization** — ~10 transport families + dynamic model registry.
  OpenAI Chat Completions and the native **Responses** API are both
  first-class (`api=chat|responses`, Responses by default for the official
  endpoint; strict parsing, streaming/deadline/retry/terminal tests). No
  `if provider == "deepseek"` anywhere in the agent.

## Layout

```
apps/        (real Faktor IDE panels: the VS Code extension host/chat/cockpit and the JetBrains split-mode frontend/backend — task tree, blockers, tournament, board, evidence; no upstream JetBrains sources)
crates/      (the Rust engine workspace, incl. winjob/pty/agent/verify/sandbox/index/cas/snapshot)
compat/      (pinned JetBrains 7.1.2 upstream corpus: jetbrains-712/)
fixtures/    (protocol, providers, screenshots, repositories)
tests/       (integration, soak, fault, visual, performance — adversarial only)
ui/          (vendored frozen upstream UI: kilo-v756-webview/ + kilo-ui/, pinned manifest)
```

## Frozen baselines

- **VS Code:** Kilo Code v7.5.6 UI (webview, CSS, images, message layout) —
  byte-for-byte fixture; the upstream trees are vendored under `ui/` at
  commit `fa02955` with a SHA-256 manifest (`ui/upstream.json`,
  `scripts/verify-upstream.mjs`), and `apps/vscode/src/kilo-bridge.ts`
  translates native state onto the frozen message ABI. `npm run
  prepackage:vsix` stages the verified pinned closure plus the additive
  Faktor companion overlay at `media/kilo-v756-webview/`, so the packaged
  VSIX ships the vendored UI self-contained (hash-asserted at package
  time); without a staged bundle the built-in Faktor chat panel is the
  fallback. Later releases are never merged wholesale.
- **JetBrains:** pinned upstream JetBrains 7.1.2 sources
  (`compat/jetbrains-712/`, SHA-256 in `ui/upstream.json`) with the
  Faktor-owned Swing frontend; the process manager launches the Faktor
  binary.
- **Protocol:** the daemon speaks the Faktor Native Protocol v1
  (`docs/native-protocol.md`) — its own durable, cursor-paged HTTP surface.
  The v7.5.6 wire-compatibility surface was retired by owner decision; no
  Kilo wire/behavior parity is claimed.

## Building

```bash
cargo build --workspace
cargo test --workspace
```

## Running

```bash
cargo run -p faktor-cli -- serve --port 0
cargo run -p faktor-cli -- run --data-dir /tmp/kp-demo "explain this repo"
cargo run -p faktor-cli -- doctor
```

## CI

CI runs on [Woodpecker](https://woodpecker-ci.org) from three event-scoped
workflows in `.woodpecker/` (targeted at Woodpecker 3.x; the folder takes
precedence over a root `.woodpecker.yml`, and this repository has none):

- **`pr.yaml`** (`pull_request`, reduced lane set, **no named volumes at
  all**) — `linux` (fmt, clippy `-D warnings`, `check`/`test --workspace
  --all-features`, doctor), `static`, `docs`, `vscode` (npm build + offline
  selftest + self-contained VSIX verify), `vscode-visual` (required chromium
  render gate) with its diagnostics twin, `jetbrains-build` and
  `jetbrains-smoke`, then the aggregate `certificate`. A `storage-policy`
  step fails closed if the PR workflow ever declares volumes.
- **`trusted.yaml`** (`push`/`tag`, darwin/windows on `push` to `main`) — the
  full lane set including the release `[perf]` lane and the darwin/windows
  matrix combos, using the trusted named-volume caches (`faktor-trusted-*`).
- **`nightly.yaml`** (`cron` job `nightly`) — `[fault]` campaigns at scale,
  longrun, efficiency, economy, coding-benchmark smoke (provider-key runs are
  recorded as explicit skips, never silent) and supply-chain evidence.
  Registration is in `scripts/woodpecker/setup.md`;
  `scripts/woodpecker/activate.sh` does it via the Woodpecker API.

The 12–24h real-time soak is out of scope by owner decision: it is not a
release criterion, no CI workflow or release gate consumes it, and the
`[soak]`-ignored longrun suites remain runnable manually.

The `certificate` job is the aggregate gate in every workflow: every lane
emits a `faktor-woodpecker-lane/v2` marker with the exact commit and tree,
runner identity, command-set digest, timestamps and artifact hashes into
`target/certification/lanes/<lane>.json`, and the certificate (depending on
all lanes, running on `success` or `failure`) runs
`node scripts/certification/evidence.mjs verify-markers` — rejecting missing,
unexpected, duplicate, unreadable, other-commit, tree-mismatch, stale-run,
failed-lane, skipped-required, silent-skip, command-digest/drift,
artifact-mismatch and non-success workflow status — then writes
`ci-certification.json`. Passing campaigns also write
`target/certification/evidence/<kind>.json` `faktor-cert-evidence/v1`
objects (`real_provider`, ...); release gates require those
objects to be signed by an allowlisted ed25519 identity. darwin/windows
carry per-platform certificates inside `trusted.yaml`. Woodpecker reports
one commit status per workflow, so branch protection requires
`ci/woodpecker/pr/pr`. No secrets are required; the named cache volumes need
the repository to be marked trusted by a server admin (see
`scripts/woodpecker/setup.md` for the PR-vs-trusted storage policy and its
residual risks).

The offline per-host certificate is unchanged: `bash scripts/certify-local.sh
fast` (or `full`) emits `target/certification/manifest.json` and consumes
signed evidence objects for release gates (`docs/certification.md` §6); the
`CERTIFY_*` booleans are inert self-test inputs and setting one without a
verifying signed evidence file fails the run.

## Branding

All user-visible metadata in this repository uses Faktor branding; legacy
wordmark tokens survive only inside the vendored/pinned upstream trees and
attribution prose (enforced by `scripts/branding-scan.sh`). The external
GitHub repository name and description are not tracked in-tree; they were set
with `gh repo rename` (name `faktor`) and
`gh repo edit --description "Faktor — native Rust coding-agent runtime with
Kilo-compatible IDE UX"`. In-tree package/manifest metadata remains the
authoritative surface and is scan-enforced.

## Status notes

- **Completion contract (gate + step execution IMPLEMENTED).** The reviewed
  PR/CI-fix item that lets a native task declare `completion_contract`
  (`include_commit`, `include_push`, `include_pr`) is implemented end to end:
  the DTO parses strictly, the accepted contract and every per-step outcome
  are durable ledger rows (immutable per task revision, pinned across
  compaction), `VerifiedComplete` is refused with a typed error while any
  requested step lacks a succeeding status row, and
  `crates/orchestrator/src/completion_steps.rs` executes the requested steps
  in gate order (idempotent, policy-checked push, supervisor-issued PR
  command). Both IDEs expose the Task-mode checkboxes and report the durable
  step provenance; a serving daemon without the additive completion read
  shows `unavailable`, never a fabricated success. Normative semantics and
  the exact seams are in `docs/certification.md` §3.3.

| Surface | Status | Evidence |
| --- | --- | --- |
| PR/CI-fix completion contract | IMPLEMENTED (gate + ordered step execution) | `crates/session/src/task.rs`, `crates/session/src/ledger.rs`, `crates/orchestrator/src/task_executor.rs`, `crates/orchestrator/src/completion_steps.rs`, §3.3 |
| Coordination board | IMPLEMENTED (durable ledger rows + native `GET/POST /native/session/{id}/board` + both IDE board panels with truthful unavailable state) | `crates/session/src/board.rs`, `crates/server/src/native/board.rs`, `apps/vscode/src/nativeClient.ts`, `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/BoardPanel.kt` |
| Multi-candidate tournament | IMPLEMENTED | `crates/orchestrator/src/tournament.rs` + native start/state/list endpoints; integration stays an explicit approved merge |
| Pixel agents | IMPLEMENTED | `apps/vscode/src/pixelAgents.ts` + JetBrains `PixelAgents.kt` (identical FNV-1a hashes) |
| Canonical child blockers | IMPLEMENTED | `crates/session/src/child.rs` (`child_runtime` v23 row) + native agent projection |
| Presentation continuity | IMPLEMENTED | durable `child_presentation_changed` fold + `POST /native/session/{id}/agents/{child}/presentation` (`crates/session/src/child.rs`, `crates/server/src/native/agents.rs`; presentation only, same ChildId/lineage) |
| Materialize → verify → land | IMPLEMENTED | durable `IntegrationRecord` with real final-root verification (`crates/session/src/ledger.rs` `IntegrationRecordRow`, `crates/orchestrator/src/task_executor.rs`); owner edits invalidate |
| Typed criterion proofs | IMPLEMENTED | `NoOpDisposition::RequiresCriterionProof` (`crates/core/src/state.rs`) + independent reviewer proof validated before completion steps run (`crates/orchestrator/src/task_executor.rs` `run_completion_steps_against_proof`) |
| OpenAI Responses family | IMPLEMENTED | native `OpenAiFamily::Responses` dispatch + `responses_body`/`responses_stream` (`crates/openai/src/lib.rs`), CLI `api=chat\|responses` (`crates/cli/src/config.rs` `OpenAiApi`) |
| Windows containment (Job Objects + ConPTY) | IMPLEMENTED | `crates/winjob/src/lib.rs`, `crates/terminal/src/lib.rs` (`JobGuard`), `crates/pty/src/windows.rs` (spawn suspended → assign → resume; no taskkill guarantee) |
| Certification evidence chain | IMPLEMENTED | `scripts/certification/evidence.mjs` (`faktor-cert-evidence/v1`, `verify-markers`), `scripts/certification/evidence.schema.json`, `scripts/certify-local.sh` (`evidence_gate`), v2 lane markers in `.woodpecker/untrusted/pr.yaml` |
| JetBrains 7.1.2 upstream assets | VENDORED (provenance only, never a parity claim) | pinned source `compat/jetbrains-712/` + per-file SHA-256 manifest `ui/upstream.json` (`jetbrains_712`) |
| JetBrains behavioral parity | IMPLEMENTED (HEAD-bound executable matrix) | `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt` writes `target/certification/jetbrains-parity.json`: 11/11 rows against canned frames and the fake daemon |
| JetBrains visual parity | IMPLEMENTED (offscreen render vs pinned baselines) | same matrix: 8 rendered panels against `apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json` |
| UI parity (`ui_parity`) | PARTIAL (derived in `target/certification/capabilities.json`) | JetBrains behavioral/visual + VS Code render axes; vendored files alone never flip it |
| Repo rename (faktor) | DONE (external) | `gh repo rename`; in-tree branding was already Faktor and is unchanged |
