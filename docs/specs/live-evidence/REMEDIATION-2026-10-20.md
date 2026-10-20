# Live residual remediation — 2026-10-20

Scope: the typed residuals recorded in `G01-G04.md` / `G05-G08.md`.
Evidence artifacts are retained verbatim (retired product-name/path tokens
sanitized to `faktor`; no credentials).

## Fixed at the source (regression test)

| Residual | Fix | Test |
|---|---|---|
| F068 / stop-scope (whole-run cancel after child stop → 500, run Running) | `crates/orchestrator/src/runtime.rs:1932` idempotent abort; `crates/orchestrator/src/task_executor/integration.rs:1113` idempotent per-child + root cancel | `tests_cancel_f068.rs:6` |
| F123/F130 index binds a shadow root deleted at settlement | `crates/index/src/service/mod.rs:948,1032` durable-root resolution + rebind/rebuild | `service/tests.rs:3099` |
| F132 template variables not blocked | `crates/foundation/src/instructions.rs:290` preview refusal | `instructions.rs:1444` |
| F165 no runtime tool-enablement authority | `crates/agent/src/runtime/tool_loop.rs:1899,1929`; admission writes the fact in `crates/server/src/native/task.rs` | `tool_loop_tests.rs:3184,3270` |
| F195 shadow has no `.git` | `crates/orchestrator/src/shadow.rs:273` local git view (headers/refs/index + object alternates; owner untouched) | `shadow_tests.rs:1479` |
| F200 no checkpoint coverage report naming shell effects | `crates/server/src/native/coverage_api.rs:10` | `session/src/ops.rs:2983` |
| F201/F202 no native diff / hunk acceptance | `changes_api.rs:35,160`; `shadow.rs:923` | `shadow_tests.rs:1542`; `foundation_rev8_contracts_tests.rs:481` |
| F205/F206/F207 worktree + mutation-lock surfaces | `crates/server/src/native/worktrees_api.rs` (create/discover/validate/repair/remove/transfer + held repo lease) | `router_misc_tests.rs:2041` |
| F064/F084 queue list/edit/remove | `queue_api.rs:24,72,128`; `store/src/queue_store.rs` | `session/src/ops.rs:3007` |
| F071 settings revision | `settings_api.rs:14` (revision fence, stale 409) | `router_misc_tests.rs:2245` |
| F073 durable fork lineage | `session/src/manager.rs` (lineage fact) + `plan_api.rs:219` | `router_misc_tests.rs:2245` |
| F045 compare mode | `task.rs:2221` (isolated sessions, split durable budgets) | `router_misc_tests.rs:2418` |
| F087/F088/F110 plan artifact + approval | `plan_api.rs:36,126,170` (only the named versioned artifact; approval grants no authority) | `router_misc_tests.rs:2245` |
| F048/F050/F090 receipts not recorded | `drive_call.rs:183`; `foundation.rs:1790` foundation sink | `tool_loop_tests.rs:3313` |
| F037/F042 saved connections not applied at boot | `serve.rs:1442`; `foundation/src/providers.rs` `mark_applied` | `serve.rs:2648` |
| F046/F093/F105/F106/F112 context/budget cap not induced | `drive_phases.rs:53`; `usage.rs:615`; prompt `max_turns` | `drive_tests.rs:2207`; `usage.rs:660` |
| 4 orchestrator settlement/verification failures | explicit no-op verifier-unavailable parks at `NeedsVerification`; configured verifier stays `Verifying`; empty candidate retention restored | `tests_core::multi_item…`, `crashed_drive…`, `failed_drive…`, `shadowed_mutating…`, `no_op_run…` |

## Gates

`cargo fmt --check`, `cargo check --workspace --all-targets`,
`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`doctor`, VS Code `npm run build` + `npm run selftest`, JetBrains
`:backend:smoke` + `:frontend:smoke` (all executable smokes pass) +
`:frontend:ideJourney` (real IntelliJ host) pass.

## Environment boundaries (not product residuals)

- Live paid-provider canaries (F092) need external credentials; the daemon
  exercises the same code paths against fake/local providers.
- `scripts/vscode-e2e.sh` is linux-x64 only; on this Darwin/arm64 host it
  exits typed: `linux-x64 only (got Darwin/arm64)`. The Node extension
  selftest (282 checks) and `tsc` build pass; a real VS Code host was already
  running but exposes no automation channel on macOS.
- `:frontend:smokeVisual` (JetBrainsHostMatrixSmoke) compares an offscreen
  visual baseline and drifts on this macOS host (`edition8-s20`), then does
  not exit; all non-visual frontend smokes and the real IntelliJ
  `:frontend:ideJourney` pass. The trusted lane pins the Linux fontconfig
  baseline; re-pinning a macOS render would launder the mismatch.
