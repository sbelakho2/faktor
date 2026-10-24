#!/usr/bin/env node
// DOGFOOD: one repo-wide self-verification coverage harness (node stdlib only).
//
// The coverage law enforced here: EVERY exposed repo surface must have a
// registered gate, and the high UI/UX bar must hold with file:line evidence.
// A surface without a gate is a silent gap; this checker turns it red.
//
// Surface classes and their gates:
//
//   production crates  tests/production-crates.json (cargo metadata snapshot):
//                      each crate must carry a unit-test site and a capability
//                      row in tests/invariant-coverage.json. The capability
//                      graph is validated by reusing
//                      check-invariant-coverage.mjs's own model (imported:
//                      parseInvariants/loadMutationSpecs/coverageProblems) so
//                      this checker cannot drift from the certifying one; the
//                      compiled-`--list` half runs in dogfood.sh §1.
//   CLI commands       every cli.* id from discover-capabilities.mjs must have
//   native routes      a manifest row (capability row / surface row /
//   builtin tools      discovered row). Discovery equality is the SAME check
//   provider kinds     the certificate lane runs; dogfood re-evaluates it and
//   JetBrains actions  additionally requires that the gate commands actually
//                      appear in scripts/dogfood.sh (registered gate).
//   VS Code surface    every command/view/configuration property/section/
//                      walkthrough/step from apps/vscode/package.json: command
//                      ids and configuration keys must be referenced by the VS
//                      Code test corpus (selftest/verify-vsix/render-webview);
//                      walkthrough step completion events must reference
//                      contributed commands; the packaged-extension lane
//                      (vsce + verify-vsix) covers the view surface.
//   JetBrains panels   every class ending in `Panel` in
//                      apps/jetbrains/frontend/src/main/kotlin/**/*Panel.kt
//                      plus FaktorChatPanel's leaf tab titles must appear in
//                      the JetBrainsHostMatrixSmoke panel list. A NEW panel
//                      fails until it is rendered/screenshotted by the host
//                      matrix zip lane.
//   visual identity    the two host Cut F SVGs, the 16-file docs/assets/brand
//                      kit, release metadata icons and the docs lockup headers
//                      must carry the pinned Faktor Visual Identity v3
//                      artwork, palette, state colors and mark geometry
//                      (contract-vi.md; audit-gates.md holes 4/5/18).
//   host-matrix states JetBrainsHostMatrixSmoke.kt must exercise
//                      state/<axis>/<panel> checks: empty for every
//                      panelFactories entry, error for the reviewed
//                      read-backed panels, loading for the not-read panels,
//                      plus state/blocked/blockers and
//                      state/reconnect/diagnostics (audit-gates.md hole 8).
//   UI/UX contract     every UIUX_CONTRACT row (contract-uiux.md I01-I10,
//                      T00-T33, §6 validation matrix, §8 copy law, §9
//                      prohibitions) must carry its mandated production
//                      needle in the named host file, and at least one
//                      needle must be pinned by that host's test corpus
//                      (VS Code selftest/render/verify-vsix; JetBrains
//                      src/test/kotlin + src/test/resources).
//                      UIUX_STATE_DISPOSITIONS then reviews every §2 state
//                      T00-T33 exactly once: enforced states must cite
//                      existing UIUX_CONTRACT rows (or their documented
//                      shipped representation), proposed states must record
//                      the mandated copy that is not shipped yet, and a
//                      proposed copy that a host starts rendering is stale.
//   scripts            every scripts/**/*.{sh,mjs,js,cjs,ps1,py} must be
//                      invoked by the dogfood run plan (scripts/dogfood.sh),
//                      reachable from a planned script (text reference or
//                      relative import), or allowlisted HERE with a non-empty
//                      reason. Stale allowlist entries are gaps too.
//   docs               every docs/*.md must be referenced from README.md or
//                      another docs/*.md (the docs index), or allowlisted.
//   CI workflows       .woodpecker/**/*.yaml are gated by the workflow-graph
//                      and image-pin checkers registered in dogfood.sh.
//   CI lane coverage   every PLAN_GATES id must be executed by a real CI lane
//                      command in .woodpecker/** (workflow file + exact
//                      command substring), or carry a reviewed
//                      CI_LANE_LOCAL_ONLY reason; a stale mapping or a stale
//                      local-only entry is a gap.
//
// UI/UX rules (all report file:line):
//
//   1. no raw java.awt Color(...) constructors outside PixelAgents.kt and the
//      reviewed token/fallback functions of PanelSupport.kt;
//   2. no `.split(' ')` argv in TerminalPanel;
//   3. no `text-transform: uppercase` on `.card` section headers in the VS
//      Code chat.css (the calm style);
//   4. visible JetBrains string literals must not expose `session_id`,
//      `worktree`, `snapshot_id`, `capability=` or `#<id>` unless the site is
//      reviewed in the INTERNAL_LABEL_ALLOWLIST below WITH a reason
//      (diagnostics/detail areas only). Count changes force re-review.
//   5. the visual identity pins (contract-vi.md MK-1.02/PL-3.01/ST-5.02/
//      HO-6.01): the host icons carry the canonical Cut F, docs/assets/brand
//      carries the reviewed 16-file kit, release metadata icons and docs
//      lockup headers stay pinned; artwork values are normative, a mismatch
//      is a gap and never a licence to edit artwork;
//   6. the JetBrains host matrix exercises the state coverage axes the audit
//      recorded as missing: empty for every panelFactories entry, error for
//      the reviewed read-backed panels, loading for the not-read panels,
//      blocked/reconnect for the recovery surfaces.
//   7. the UI/UX contract pins (UIUX_CONTRACT): shell/navigation clusters,
//      task options + per-run budget, the four-level finish booleans, the
//      Task/Ask modes, the mandated T-state label/copy/action law, the I03
//      diff-unavailable honesty, the I04 proof/integration/cost separation,
//      the I07 outcome labels, the I05/I06/I10 prohibitions and the
//      statically checkable validation-matrix rows (contract-uiux.md).
//   8. the UI/UX state dispositions (UIUX_STATE_DISPOSITIONS): exactly one
//      reviewed entry per §2 state T00-T33; enforced entries must resolve
//      against live UIUX_CONTRACT rows, proposed entries must carry their
//      unshipped mandated copy, and a proposed copy that appears in a host
//      production file is a stale proposal.
//
// Modes:
//   node scripts/dogfood.mjs --coverage   (default) full surface inventory
//   node scripts/dogfood.mjs --selftest   adversarial fixtures: a planted gap
//                                         in each rule class MUST fail the
//                                         corresponding check, and the real
//                                         tree must report zero gaps
//
// Exit codes: 0 pass; 1 gaps/violations; 2 usage.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { discover, normalizeCapabilityId } from './discover-capabilities.mjs';
import {
  coverageProblems,
  loadMutationSpecs,
  parseInvariants,
} from './check-invariant-coverage.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const PLAN_FILE = 'scripts/dogfood.sh';
const PRODUCTION_CRATES_FILE = 'tests/production-crates.json';
const COVERAGE_FILE = 'tests/invariant-coverage.json';
const REGISTRY_FILE = 'tests/invariants.toml';
const VSCODE_SELFTEST = 'apps/vscode/scripts/selftest.mjs';
const VSCODE_TEST_CORPUS = [
  VSCODE_SELFTEST,
  'apps/vscode/scripts/verify-vsix.mjs',
  'apps/vscode/scripts/render-webview.mjs',
  'apps/vscode/scripts/edition5-control-exercise.mjs',
];

// ---------------------------------------------------------------------------
// Registered gates: the dogfood run plan must actually execute each of these.
// A marker is a literal substring of scripts/dogfood.sh; a missing marker is a
// `missing-plan-gate` gap (the executor and this checker cannot drift).
// ---------------------------------------------------------------------------
const PLAN_GATES = [
  ['cargo-fmt', '§1', 'cargo fmt --check'],
  ['cargo-check', '§1', 'cargo check --workspace --all-features'],
  ['cargo-clippy', '§1', 'cargo clippy --workspace --all-targets --all-features -- -D warnings'],
  ['cargo-test', '§1', 'cargo test --workspace --all-features'],
  ['compiled-capability-tests', '§1', '\\"$CI_TOOLS\\" capability-tests'],
  ['invariants', '§2', 'check-invariants.mjs'],
  ['invariant-coverage-selftest', '§2', 'check-invariant-coverage.mjs selftest'],
  ['invariant-coverage-check', '§2', 'check-invariant-coverage.mjs --check'],
  ['invariant-coverage-release', '§2', 'check-invariant-coverage.mjs --release'],
  ['network-authority-singular', '§2', 'check-network-authority-singular.mjs'],
  ['ignored-tests', '§2', 'check-ignored-tests.mjs'],
  ['docs-sync', '§2', 'check-docs-sync.sh'],
  ['capabilities-selftest', '§2', 'capabilities-manifest.mjs --selftest'],
  ['capabilities-claims', '§2', 'capabilities-manifest.mjs --check-claims'],
  ['licenses', '§2', 'check-licenses.sh'],
  ['gradle-integrity', '§2', 'check-gradle-integrity.sh'],
  ['branding-scan', '§2', 'branding-scan.sh'],
  ['static-authority', '§2', 'faktor-tests-static-authority'],
  ['mutation-campaign', '§3', 'check-invariants.mjs --mutations'],
  ['doctor-deep', '§4', 'doctor --data-dir'],
  ['vscode-build', '§5', 'npm run build'],
  ['vscode-selftest', '§5', 'selftest.mjs'],
  ['vscode-render', '§5', 'render-webview.mjs --check'],
  ['vscode-vsix', '§5', 'verify-vsix.mjs'],
  ['vscode-e2e', '§5', 'vscode-e2e.sh'],
  ['jetbrains-smokes', '§6', 'smokeHostMatrixZip'],
  ['edition5', '§7', 'scripts/edition5/run.mjs'],
  ['edition8', '§7', 'tests/edition8/registry.mjs'],
  ['programme-registry-build', '§2', 'build-registry.mjs --check'],
  ['programme-registry-check', '§2', 'check-registry.mjs --check'],
  ['programme-registry-selftest', '§2', 'check-registry.mjs --selftest'],
  ['programme-selector-selftest', '§2', 'select-tests.mjs selftest'],
  ['programme-lane-selftest', '§2', 'run-lane.mjs --selftest'],
  ['programme-settlement', '§2', 'check-settlement.mjs'],
  ['programme-hostile-generators', '§2', 'generators/hostile.mjs --selftest'],
  ['programme-race-schedules', '§2', 'generators/schedules.mjs --selftest'],
  ['programme-ordering-probe', '§2', 'probes/ordering.mjs'],
  ['programme-agent-evals', '§2', 'evals.mjs --smoke'],
  ['programme-core-dumps', '§2', 'check-core-dumps.mjs'],
  ['blob-hygiene', '§2', 'check-blob-hygiene.mjs'],
  ['programme-release-selftest', '§2', 'release-gate.mjs --selftest'],
  ['programme-full-selection', '§2', 'select-tests.mjs --mode trusted'],
  ['dogfood-coverage', '§8', 'dogfood.mjs --coverage'],
];

// ---------------------------------------------------------------------------
// CI lane coverage: one entry per PLAN_GATES id. `lanes` pairs a workflow file
// with the substring of the lane command that executes the gate; at least one
// pair must be present in that file, so a lane that drops or renames the gate
// turns the mapping stale. Substrings are the lanes' own command lines (read
// from .woodpecker/untrusted/pr.yaml, .woodpecker/trusted/trusted.yaml and
// .woodpecker/trusted/nightly.yaml); a gate that no lane runs is reviewed in
// CI_LANE_LOCAL_ONLY below instead of being mapped to a command that does not
// exist.
// ---------------------------------------------------------------------------
const CI_LANE_MAP = [
  {
    id: 'cargo-fmt',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'cargo fmt --check'],
      ['.woodpecker/trusted/trusted.yaml', 'cargo fmt --check'],
    ],
  },
  {
    id: 'cargo-check',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'cargo check --workspace --all-features'],
      ['.woodpecker/trusted/trusted.yaml', 'cargo check --workspace --all-features'],
    ],
  },
  {
    id: 'cargo-clippy',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'cargo clippy --workspace --all-targets --all-features -- -D warnings'],
      ['.woodpecker/trusted/trusted.yaml', 'cargo clippy --workspace --all-targets --all-features -- -D warnings'],
    ],
  },
  {
    id: 'cargo-test',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'cargo test --workspace --all-features'],
      ['.woodpecker/trusted/trusted.yaml', 'cargo test --workspace --all-features'],
    ],
  },
  {
    // The compiled --list enumeration runs only in the trusted linux lane
    // (the PR linux lane deliberately keeps its reduced command list).
    id: 'compiled-capability-tests',
    lanes: [['.woodpecker/trusted/trusted.yaml', '$CARGO_TARGET_DIR/release/faktor-ci-tools capability-tests']],
  },
  {
    id: 'invariants',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'node scripts/check-invariants.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'node scripts/check-invariants.mjs'],
    ],
  },
  {
    id: 'invariant-coverage-selftest',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'check-invariant-coverage.mjs selftest']],
  },
  {
    // The trusted certificate lane runs the same checker in --release mode:
    // it re-runs every --check gate (discovery equality, unit existence,
    // binding, delegation validity) and additionally fails release debt, so
    // the stronger lane command subsumes the plan's --check invocation.
    id: 'invariant-coverage-check',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'check-invariant-coverage.mjs --release']],
  },
  {
    id: 'invariant-coverage-release',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'check-invariant-coverage.mjs --release']],
  },
  {
    id: 'network-authority-singular',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'check-network-authority-singular.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'check-network-authority-singular.mjs'],
    ],
  },
  {
    id: 'ignored-tests',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'check-ignored-tests.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'check-ignored-tests.mjs'],
      ['.woodpecker/trusted/nightly.yaml', 'check-ignored-tests.mjs'],
    ],
  },
  {
    id: 'docs-sync',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'check-docs-sync.sh'],
      ['.woodpecker/trusted/trusted.yaml', 'check-docs-sync.sh'],
    ],
  },
  {
    // Node-carrying certificate lane only: the rust lanes run the contracts
    // half and check-docs-sync.sh skips its node-only claim checks.
    id: 'capabilities-selftest',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'capabilities-manifest.mjs --selftest']],
  },
  {
    id: 'capabilities-claims',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'capabilities-manifest.mjs --check-claims']],
  },
  {
    id: 'licenses',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'check-licenses.sh']],
  },
  {
    id: 'gradle-integrity',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'check-gradle-integrity.sh'],
      ['.woodpecker/trusted/trusted.yaml', 'check-gradle-integrity.sh'],
    ],
  },
  {
    // Source scan in the docs lanes plus the packaged-artifact scans in the
    // vscode/jetbrains-build lanes; the substring matches every form.
    id: 'branding-scan',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'branding-scan.sh'],
      ['.woodpecker/trusted/trusted.yaml', 'branding-scan.sh'],
    ],
  },
  {
    id: 'static-authority',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'faktor-tests-static-authority'],
      ['.woodpecker/trusted/trusted.yaml', 'faktor-tests-static-authority'],
    ],
  },
  {
    // The executable planted-mutation campaign needs cargo + JDK/gradle +
    // Node together, so it runs only in the trusted jetbrains-smoke lane (the
    // node-only certificates deliberately do not run it).
    id: 'mutation-campaign',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'check-invariants.mjs --mutations']],
  },
  {
    id: 'vscode-build',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'npm run build'],
      ['.woodpecker/trusted/trusted.yaml', 'npm run build'],
      ['.woodpecker/trusted/nightly.yaml', 'npm run build'],
    ],
  },
  {
    id: 'vscode-selftest',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'node scripts/selftest.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'node scripts/selftest.mjs'],
      ['.woodpecker/trusted/nightly.yaml', 'node scripts/selftest.mjs'],
    ],
  },
  {
    // The chromium render gate is a real lane on the digest-pinned Playwright
    // image in the PR workflow and is re-run nightly; the trusted push
    // workflow covers it through vscode-e2e instead.
    id: 'vscode-render',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'render-webview.mjs --check'],
      ['.woodpecker/trusted/nightly.yaml', 'render-webview.mjs --check'],
    ],
  },
  {
    id: 'vscode-vsix',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'verify-vsix.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'verify-vsix.mjs'],
    ],
  },
  {
    id: 'vscode-e2e',
    lanes: [['.woodpecker/trusted/trusted.yaml', 'vscode-e2e.sh']],
  },
  {
    // The gradle host smokes: compile-and-smoke.sh runs :backend:smoke +
    // :frontend:smoke + :frontend:smokeHostMatrixZip in the PR and trusted
    // lanes; the nightly ui-smoke-jetbrains lane invokes the ZIP host matrix
    // task directly.
    id: 'jetbrains-smokes',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'compile-and-smoke.sh'],
      ['.woodpecker/trusted/trusted.yaml', 'compile-and-smoke.sh'],
      ['.woodpecker/trusted/nightly.yaml', 'smokeHostMatrixZip'],
    ],
  },
  {
    // Edition 5 gates: the PR lane consumes the JetBrains host raw evidence
    // produced by jetbrains-smoke and writes the gate summary; the trusted
    // certificate lane runs the same gates plus the isolated real-daemon
    // dogfood before verifying the evidence.
    id: 'edition5',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'node scripts/edition5/run.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'node scripts/edition5/run.mjs'],
    ],
  },
  {
    // Edition 8 gates: every host/visual/perf gate is verified by the PR lane
    // (linux, both hosts) and the trusted lane; the darwin-edition8 lane runs
    // the isolated both-host dogfood and the same verifier.
    id: 'edition8',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'node scripts/edition8/run.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'node scripts/edition8/run.mjs'],
      ['.woodpecker/trusted/trusted.yaml', 'node scripts/edition8/run.mjs --dogfood'],
    ],
  },
  {
    id: 'programme-registry-build',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'build-registry.mjs --check']],
  },
  {
    id: 'programme-registry-check',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'check-registry.mjs --check']],
  },
  {
    id: 'programme-registry-selftest',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'check-registry.mjs --selftest']],
  },
  {
    id: 'programme-selector-selftest',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'select-tests.mjs selftest']],
  },
  {
    id: 'programme-lane-selftest',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'run-lane.mjs --selftest']],
  },
  {
    id: 'programme-settlement',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'check-settlement.mjs']],
  },
  {
    id: 'programme-hostile-generators',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'generators/hostile.mjs --selftest']],
  },
  {
    id: 'programme-race-schedules',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'generators/schedules.mjs --selftest']],
  },
  {
    id: 'programme-ordering-probe',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'probes/ordering.mjs']],
  },
  {
    id: 'programme-agent-evals',
    lanes: [['.woodpecker/trusted/nightly.yaml', 'evals.mjs --smoke']],
  },
  {
    id: 'programme-core-dumps',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'check-core-dumps.mjs']],
  },
  {
    id: 'blob-hygiene',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'check-blob-hygiene.mjs']],
  },
  {
    id: 'programme-release-selftest',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'release-gate.mjs --selftest']],
  },
  {
    id: 'programme-full-selection',
    lanes: [['.woodpecker/untrusted/pr.yaml', 'select-tests.mjs --mode pull']],
  },
  {
    id: 'dogfood-coverage',
    lanes: [
      ['.woodpecker/untrusted/pr.yaml', 'dogfood.mjs --coverage'],
      ['.woodpecker/trusted/trusted.yaml', 'dogfood.mjs --coverage'],
    ],
  },
];

// PLAN_GATES ids that no CI lane command executes, reviewed one by one. A
// stale entry (the id no longer exists, or a lane now runs the gate) is a
// gap, and a mapping in CI_LANE_MAP wins over a local-only entry.
const CI_LANE_LOCAL_ONLY = new Map(
  Object.entries({
    'doctor-deep':
      'no CI lane runs `doctor --deep`: the lanes execute the bounded quick doctor on an ephemeral /tmp data dir, while the deep full-store integrity/CAS/durability sweep is the certificate/operator form over a lived-in data dir; mapping it to the plain lane command would over-claim, and the deep path (wedged sessions, corrupt CAS blobs) is executed under §1 cargo test by crates/cli main_doctor_tests.rs and tests/usage_scenarios.rs.',
  }),
);

// ---------------------------------------------------------------------------
// Reviewed allowlists. Every entry needs a concrete, non-empty reason; a
// stale entry (no matching surface) is itself a gap, and identifier-label
// count changes force re-review.
// ---------------------------------------------------------------------------

// Scripts not invoked (directly or transitively) by scripts/dogfood.sh.
const SCRIPT_ALLOWLIST = new Map(
  Object.entries({
    'scripts/certify-local.sh':
      'local certificate executor (fast/full profiles) driven explicitly by an operator; dogfood.sh is the CI-shaped local suite and does not run the whole certificate.',
    'scripts/site-check.sh':
      'website crate gate (fmt/check/test/clippy + Zola build + zero-JS); executed by the dedicated site workflow (.woodpecker/site.yaml) and locally, requires Zola and is not part of the offline Rust dogfood plan.',
    'scripts/site-zero-js.sh':
      'zero-JavaScript website gate invoked by scripts/site-check.sh and the dedicated site workflow; requires the built Zola output, not part of the offline Rust dogfood plan.',
    'scripts/site-visual.sh':
      'website screenshot/PDF parity gate invoked by the dedicated site visual lane; requires Zola, Playwright and ImageMagick and reviewed baselines, so it is not part of the offline Rust dogfood plan.',
    'scripts/install-matrix.mjs':
      'release install-matrix lane (clean-prefix extract + doctor + archive structure); requires built release artifacts and runs in the full certify-local profile.',
    'scripts/package-artifacts.sh':
      'release artifact packaging lane (certify.sh release-artifacts); requires built release binaries, out of scope for the local dogfood suite.',
    'scripts/supply-chain.sh':
      'static-lane supply-chain gate; needs SUPPLY_CHAIN_BUILD=1 and a release build, executed by the trusted static lane.',
    'scripts/update-iana-special-registry.sh':
      'operator data-refresh helper for the IANA special-registry table; requires network and the generated table is a frozen contract checked by cargo tests.',
    'scripts/windows-visual-baseline.ps1':
      'PowerShell implementation of the Windows visual baseline lane; runs on the self-hosted windows agent and is not executable in the linux dogfood suite.',
    'scripts/woodpecker/activate.sh':
      'operator helper for Woodpecker server/project setup (scripts/woodpecker/setup.md); not part of the repository build or test surface.',
    'scripts/programme/live-contracts.mjs':
      'weekly live lane driver: runs recorded provider fixtures offline and refuses typed without operator live keys/driver; it must not run inside the offline dogfood suite.',
    'scripts/edition8/fake-ollama.mjs':
      'fake provider used by the nightly scale-probe lane (sandboxed loopback server); the nightly campaign runs it with the probe.',
    'scripts/edition8/scale-probe.mjs':
      'nightly scale-probe lane (1,000 turns/100k messages/2k sessions with stored curves); too large for the offline dogfood suite.',
    'scripts/vscode-e2e/edition8-host-smoke.cjs':
      'invoked by the Extension Host e2e lane (scripts/vscode-e2e.sh / apps/vscode package scripts), not by the offline dogfood plan.',
    'scripts/programme/hil.mjs':
      'hardware-in-the-loop release lane driver: requires FAKTOR_HIL=1 and a rig command, so it is a typed refusal outside the release rig.',
    'scripts/woodpecker/verify-boundary.sh':
      'operator assertion for the trusted/untrusted Woodpecker project boundary; exercised during server setup, not by the dogfood suite.',
    'scripts/certification/prove-all.mjs':
      'certificate aggregate binding the checkout bytes; its component selftests (discovery, workflow graph, publisher, visual) each run in dogfood §2.',
    'scripts/check-fault-seams.sh':
      'static fault-seam checker executed directly by the PR static lane and the trusted static lane (sh scripts/check-fault-seams.sh); it is a lane command, not a dogfood-plan step.',
    'scripts/mutations/vscode-ts-loader.mjs':
      'node --experimental-loader shim referenced by the VS Code mutation gate commands in scripts/mutations/*.json, executed by check-invariants --mutations (dogfood §3).',
    'scripts/mutation-run.mjs':
      'planted-mutation runner; tests/invariants.toml mutation_command invokes it per invariant and check-invariants --mutations (dogfood §3) executes that registry.',
    'scripts/generate-iana-network-table':
      'thin wrapper for the Rust faktor-ci-tools iana-table generator of the frozen IANA table in crates/security/src/network.rs; invoked by scripts/update-iana-special-registry.sh (operator refresh) and the generated table is exercised by the security crate tests.',
    'scripts/edition5/dogfood.sh':
      'isolated real-daemon Edition 5 dogfood (git worktree + data dir + loopback fake provider); runs in the trusted certificate lane and locally because it needs the full daemon/JetBrains toolchains, not the node-only dogfood plan.',
    'scripts/edition5/fake-openai.mjs':
      'loopback-only fake OpenAI-compatible provider spawned by scripts/edition5/dogfood.sh; never network-reachable and never a production provider.',
    'scripts/edition5/acceptance-jetbrains.mjs':
      'converts the real JetBrains host observation overlays into the shared A01-A40 result document; invoked transitively by scripts/edition5/run.mjs.',
    'scripts/edition8/codegen.mjs':
      'generates the 72-screen/144-host-contract registries from docs/specs/edition8/registry.json for both hosts; generation is a source-authoring step and the generated files are committed and gated, so it is not run by CI.',
    'scripts/dogfood.sh':
      'executor root of the DOGFOOD program: it IS the run plan every other script is classified against, so it cannot invoke itself; it is run explicitly by operators and its registered gates run in the certificate/PR lanes through scripts/dogfood.mjs.',
    'scripts/intelligence/merge.mjs':
      'intelligence-eval result merger: post-processes external hidden-suite result files produced by run.mjs; requires operator eval artifacts and is not part of the offline dogfood suite.',
    'scripts/intelligence/report.mjs':
      'intelligence-eval result reporter: renders operator-owned run.mjs/merge.mjs result files; requires eval artifacts and is not part of the offline dogfood suite.',
  }),
);

// docs/*.md not linked from README.md / another docs/*.md.
const DOCS_ALLOWLIST = new Map(
  Object.entries({
    'docs/acquire.md':
      'normative commerce acquisition contract; cited from crates/cli (acquire_certification, tools_market) and tests/static-authority, not part of the architecture index.',
    'docs/ci-enforcement.md':
      'operator CI enforcement runbook; linked from scripts/woodpecker/setup.md rather than the architecture index.',
    'docs/query-plan-audit.md':
      'query-planner audit notes owned by the storage/query workstream; the normative contract lives in docs/architecture.md and the audit is intentionally not part of the product index.',
    'docs/ui-terminology.md':
      'normative Work/Inspect/History vocabulary law; enforced by the dogfood internal-identifier label rule below and by the panel wording; not yet linked from the architecture index.',
    'docs/engineering-receipt-attestation.md':
      'normative Engineering receipt-signing and model-applicability contract; enforced by crates/engineering-* tests and cited from their crate docs, not part of the architecture index.',
  }),
);

// JetBrains panels whose host-matrix entry is not the mechanical kebab name.
const PANEL_HOST_ALIASES = new Map(
  Object.entries({
    EvidenceNavigatorPanel: {
      host: 'evidence',
      reason: 'the Evidence tab hosts EvidenceNavigatorPanel; the matrix entry is named for the tab, not the class.',
    },
    AttachmentsPanel: {
      host: 'task',
      reason: 'AttachmentsPanel is the attachment half of the Task composer pane rendered by the "task" matrix entry (taskComposerPaneForTest).',
    },
  }),
);

// Host-matrix entries that render a surface OF the FaktorChatPanel container
// rather than a standalone Panel class or a chat leaf tab. Each entry is
// reviewed with a reason; an entry the matrix no longer renders is a gap.
const HOST_MATRIX_COMPONENT_CLAIMS = new Map(
  Object.entries({
    'task-options':
      'the bounded Task options sheet opened on the Work center surface by FaktorChatPanel; it is not a tab (it replaces the conversation until Back/Apply).',
    'work-approval':
      'the inline Work approval attention card rendered by FaktorChatPanel; the full request list remains the Agents/Approvals tab (permissions).',
    toolbar:
      'the wrapping utilities toolbar row owned by the FaktorChatPanel container, not a panel class or leaf tab.',
  }),
);

// Panel functions whose Color(...) constructors are reviewed as token/fallback
// sites. Outside this list, a Color constructor in PanelSupport.kt is a gap;
// every other production .kt file is exempt only via PixelAgents.kt.
const PANEL_SUPPORT_COLOR_FUNCTIONS = new Set([
  'textForeground',
  'panelSurface',
  'mutedForeground',
  'cardBorderColor',
  'borderColor',
  'mix',
  'luminance',
  'contrastRatio',
  'semanticForeground',
  'accent',
  'accentHover',
  'accentPressed',
  'accentTint',
  'onAccent',
  'towardContrast',
  'separator',
  'insetBorder',
  'isDarkSurface',
  'hover',
  'pressed',
  'cardSurface',
  'channel',
  'shade',
  'tint',
]);

// Reviewed visible-label hits for the internal-identifier rule. `file` is
// repo-relative, `token` one of the banned patterns, `count` the exact number
// of occurrences observed at review time (a change forces re-review), and
// `reason` why this is a legitimate diagnostics/detail site.
const INTERNAL_LABEL_ALLOWLIST = [
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition5CatalogData.kt',
    token: 'worktree',
    count: 3,
    reason: 'frozen control-catalog copy names the per-instance scope term "worktree" in advanced control descriptions; it is spec text, not a visible raw id label.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition5Panels.kt',
    token: 'session_id',
    count: 2,
    reason: 'diagnostics detail rows in the Edition 5 context/checkpoint panels surface the native session_id field as an inspect key; primary actions stay human-labeled.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/BlockersPanel.kt',
    token: 'capability=',
    count: 1,
    reason: 'permission card diagnostics body behind the human Allow/Deny actions: the numeric id is the reply handle and the capability is the risk key.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/BlockersPanel.kt',
    token: '#<id>',
    count: 1,
    reason: 'same permission diagnostics body: the id is the actionable permission handle, not a primary page title.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/EvidenceNavigatorPanel.kt',
    token: '#<id>',
    count: 1,
    reason: 'transcript echo line numbering (#seq role: text) inside the evidence detail list; the number is the visible message ordinal.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt',
    token: '#<id>',
    count: 2,
    reason: 'transcript line numbering and the board-post revision confirmation text; both are conversation ordinals, not internal session/entity ids.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/PermissionsPanel.kt',
    token: '#<id>',
    count: 3,
    reason: 'permissions list/detail diagnostics: the id is the reply handle of the Allow/Deny action and the session line is the inspect detail body.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/PermissionsPanel.kt',
    token: 'capability=',
    count: 1,
    reason: 'permissions inspect detail pane (unitLabel) surfaces the daemon capability key; primary actions remain human-labeled.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreePanel.kt',
    token: 'worktree',
    count: 1,
    reason: 'task-tree child detail label surfaces ownership/worktree ids as inspect fields, per docs/ui-terminology raw ids belong in advanced details.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TournamentPanel.kt',
    token: 'worktree',
    count: 1,
    reason: 'tournament candidate detail pane (describe) surfaces the candidate worktree as an inspect field; the primary actions stay human-labeled.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TournamentPanel.kt',
    token: '#<id>',
    count: 2,
    reason: 'verification-record number in the candidate detail pane and the winner chip ("#3 pass/fail") — the number is the visible verification ordinal.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8Daemon.kt',
    token: 'session_id',
    count: 2,
    reason: 'native path-template resolver and permission reply-handle detail: session_id is a wire placeholder/handle inside diagnostics, never a primary label; the visible permission card names the action and scope.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8Panels.kt',
    token: 'session_id',
    count: 1,
    reason: 'permission inspect detail body reads the daemon session_id reply handle; primary actions keep human labels and the id is never shown as task identity.',
  },
  {
    file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8ScreenRegistry.kt',
    token: 'worktree',
    count: 7,
    reason: 'generated Rev8 screen/control contract descriptions (screens.md/controls.json) surface worktree as an inspect/detail noun in help copy; no primary action is labeled with an internal id.',
  },
];

// ---------------------------------------------------------------------------
// Visual identity pins (Faktor / Visual Identity v3, 11 October 2026).
// Contract: contract-vi.md (extracted from Faktor_Visual_Identity.pdf), whose
// section markers and rule IDs are cited per pin below. These values are
// normative: the shipped artwork is the contract, so a mismatch is a gap and
// never a licence to edit product files. Gate for audit-gates.md holes 4
// (identity assets unguarded), 5 (release-metadata icons unguarded) and 18
// (icon dimensions unguarded).
// ---------------------------------------------------------------------------

// MK-1.02 "Cut F": one solid initial, straight edges only. The canonical path
// was read from the shipped apps/vscode/media/faktor.svg (HO-6.01 rollout
// target) and must equal the contract's path; Appendix B pins
// allowed_path_commands = M/V/H/L/Z, stem 16, arm 14, tip 6 units.
const CUT_F_PATH = 'M0 52 L0 0 L50 0 L50 8 L44 14 L16 14 L16 25 L41 25 L41 33 L35 39 L16 39 L16 52 Z';
// The docs masters (faktor-symbol*.svg) and the lockup/stacked masters state
// the identical closed contour with the final L0 52 written before Z (Z closes
// to the M0 52 origin either way). Both shipped spellings are accepted;
// changing either spelling is still reviewed as an artwork change.
const CUT_F_PATH_CLOSED = 'M0 52 L0 0 L50 0 L50 8 L44 14 L16 14 L16 25 L41 25 L41 33 L35 39 L16 39 L16 52 L0 52 Z';
// The contour vertices in order (final explicit close is redundant).
const CUT_F_POINTS = [
  [0, 52],
  [0, 0],
  [50, 0],
  [50, 8],
  [44, 14],
  [16, 14],
  [16, 25],
  [41, 25],
  [41, 33],
  [35, 39],
  [16, 39],
  [16, 52],
];
// MK-1.03/MK-1.04/PR-7.02/PR-7.33: no circle, curl, koru, arc, curve, round
// container, extra emblem or decorative element in the mark; these elements
// are the static tell in the shipped artwork.
const FORBIDDEN_MARK_ELEMENTS = ['circle', 'ellipse', 'text', 'image'];
const STRAIGHT_PATH_COMMANDS = new Set(['M', 'V', 'H', 'L', 'Z']);
// PL-3.01 palette table (verbatim names + hex; Appendix B "palette").
const BRAND_PALETTE = [
  ['ink', 'Ink', '#08110C'],
  ['forest', 'Forest', '#143D2B'],
  ['paper', 'Paper', '#F3F5EF'],
  ['sage', 'Sage', '#92AA99'],
  ['ochre', 'Ochre', '#D9BA75'],
  ['ochreDeep', 'Ochre Deep', '#72541E'],
];
// ST-5.02 state color families (light/dark hex; "Brand ochre is not success").
const BRAND_STATE_COLORS = [
  ['success', 'Success', '#145C39', '#68DC93'],
  ['pending', 'Pending', '#72541E', '#D9BA75'],
  ['failure', 'Failure', '#9D3131', '#FFB3AE'],
];
// MK-1.02 mark numbers: stem 16, arms 14, clipped tips 6 (units).
const CUT_F_UNITS = [
  ['16', 'stem'],
  ['14', 'arms'],
  ['6', 'clipped tips'],
];

// HO-6.01 rollout targets: one bare angular Cut F per host, straight edges
// only. Index 0 is the VS Code slot (currentColor/theme semantics); index 1 is
// the JetBrains slot (host monochrome constraint).
const HOST_ICON_FILES = [
  'apps/vscode/media/faktor.svg',
  'apps/jetbrains/frontend/src/main/resources/META-INF/faktor.svg',
];

// AS-2.02/AS-2.03 kit: the complete shipped docs/assets/brand tree at review
// time (16 files; AS-2.03 variant set {currentColor, Ink, white, accent}).
// `kind` drives the artwork rule: symbol/tile are straight-edge only;
// symbol/lockup/stacked/tile must carry the canonical Cut F (the stacked
// masters bake it at a uniform scale, verified geometrically); wordmark files
// are the bespoke lettering and carry no symbol requirement. An unlisted file
// in the directory is an unreviewed asset.
const BRAND_KIT = new Map(
  Object.entries({
    'faktor-symbol.svg': 'symbol',
    'faktor-symbol-accent.svg': 'symbol',
    'faktor-symbol-ink.svg': 'symbol',
    'faktor-symbol-white.svg': 'symbol',
    'faktor-lockup-accent.svg': 'lockup',
    'faktor-lockup-currentColor.svg': 'lockup',
    'faktor-lockup-ink.svg': 'lockup',
    'faktor-lockup-white.svg': 'lockup',
    'faktor-lockup-stacked-accent.svg': 'stacked',
    'faktor-lockup-stacked-currentColor.svg': 'stacked',
    'faktor-lockup-stacked-ink.svg': 'stacked',
    'faktor-lockup-stacked-white.svg': 'stacked',
    'faktor-wordmark-currentColor.svg': 'wordmark',
    'faktor-wordmark-ink.svg': 'wordmark',
    'faktor-app-tile.svg': 'tile',
    'faktor-tokens.json': 'tokens',
  }),
);

// Host-matrix file whose state/<axis>/<panel> checks this checker requires.
const HOST_MATRIX_FILE = 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsHostMatrixSmoke.kt';

// Host-matrix panels that consume a daemon read route and therefore need an
// exercised error/unavailable state (contract-uiux.md §2 state rows
// T15/T29/T30/T31 and §6; audit-gates.md §8 hole 8 records that only four
// panels had an error state). Reasons cite the panel's refusal presentation
// or read route at review time (line numbers are review-time evidence).
const HOST_MATRIX_READ_BACKED_PANELS = new Map(
  Object.entries({
    permissions: 'PermissionsPanel.setUnavailable (PermissionsPanel.kt:95) renders the typed refusal; T10 approval reads refuse per request.',
    terminal: 'TerminalPanel.setUnavailable (TerminalPanel.kt:358) renders the refused terminal route (T09/T26 read surface).',
    settings: 'SettingsPanel.setUnavailable (SettingsPanel.kt:248) renders the failed provider/settings read (T30).',
    history: 'HistoryPanel.setUnavailable (HistoryPanel.kt:183) renders T31 "History unavailable: the conversation list could not be read".',
    diagnostics: 'DiagnosticsPanel.setUnavailable (DiagnosticsPanel.kt:380) renders the refused diagnostics/readiness read (T02/T04/T28).',
    outcomes: 'OutcomesPanel model state unavailable (OutcomesPanel.kt:63,98) renders T29 "Cost unavailable" with no monetary value.',
    changes: 'ChangesPanel.setUnavailable (ChangesPanel.kt:228) renders the refused changes read (I03 source-diff evidence).',
    verification: 'VerificationPanel renders CriterionVerdictTone.UNAVAILABLE rows (VerificationPanel.kt:253; T15 "No verdict is served for this claim").',
    evidence: 'EvidenceNavigatorPanel bounded evidence retrieval can refuse; T15/T17 require the no-verdict presentation to stay honest.',
    usage: 'UsagePanel.setUnavailable (UsagePanel.kt:415) renders the refused usage/metering read (T29/T30).',
    agents: 'AgentsPanel roster read (roles/spends) can refuse; T09/T11 require an honest surface when it does.',
  }),
);

// Panels with an explicit not-read/connecting presentation: the loading axis
// must render before the first read completes (contract-uiux.md §2 T15/T29).
const HOST_MATRIX_NOT_READ_PANELS = new Map(
  Object.entries({
    diagnostics: 'DiagnosticsPanel.setConnection("connecting to daemon", "connecting", ...) streams the connecting state (DiagnosticsPanel.kt:380).',
    outcomes: 'OutcomesPanel model "not_read" is the honest pre-read state (OutcomesPanel.kt:63); nothing is invented before the read.',
  }),
);

// Fixed axis/panel pairs the host matrix must always exercise.
const HOST_MATRIX_FIXED_STATE_CHECKS = [
  ['blocked', 'blockers', 'T11/T13/T19: the blockers surface must render a blocked row (audit-gates.md §8 hole 8).'],
  ['reconnect', 'diagnostics', 'T27/T28: reconnect/restart affordances and the stopped connection must render honestly.'],
];

// ---------------------------------------------------------------------------
// UI/UX contract pins (contract-uiux.md, extracted from Faktor_IDE_UI_UX.pdf).
// Each row is one reviewed gate: `must` lists the literal production needles
// ([file, needle]) the mandate requires in the named host, and `tests` lists
// candidate needles for that host's test corpus (VS Code:
// selftest/render-webview/verify-vsix; JetBrains: src/test/kotlin +
// src/test/resources); at least one test needle must exist or the row is
// reported unpinned. Host scoping follows the host whose production file
// renders the group today (§1 file lists, §3 placements, §8 copy law). Rows
// stay at full contract strength: a missing needle is a gap for the product
// fix lane and is never lowered here (see the rule's gap wording).
// ---------------------------------------------------------------------------

const UIUX_CONTRACT = [
  // ------------------------------------------------------------------ shell / I01
  {
    id: 'UIUX-SHELL-VSC-SWITCHER',
    spec: 'I01 [L1394-1404] "Shared Work/Inspect/History shell ... VS Code visible History"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/webview.ts', 'id="nav-work"'],
      ['apps/vscode/src/webview.ts', 'id="nav-inspect"'],
      ['apps/vscode/src/webview.ts', 'id="nav-history"'],
    ],
    tests: { vscode: ['nav-work', 'nav-inspect', 'nav-history'] },
  },
  {
    id: 'UIUX-SHELL-VSC-INSPECT-SUBVIEWS',
    spec: 'DEC-VSC-NAV [L65] "Inspect subviews Overview / Plan / Changes / Verification / Agents"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/webview.ts', 'id="subtab-overview"'],
      ['apps/vscode/src/webview.ts', 'id="subtab-plan"'],
      ['apps/vscode/src/webview.ts', 'id="subtab-changes"'],
      ['apps/vscode/src/webview.ts', 'id="subtab-verification"'],
      ['apps/vscode/src/webview.ts', 'id="subtab-agents"'],
    ],
    tests: { vscode: ['subtab-overview'] },
  },
  {
    id: 'UIUX-SHELL-VSC-HISTORY-SWITCH',
    spec: 'I01 [L1394-1404] "add host open/switch action"; S26 [L569-580] "History proposed visible destination"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/webview.ts', 'id="history-panel"'],
      ['apps/vscode/src/sessionSwitch.ts', 'export function sessionSwitchDecision'],
      ['apps/vscode/src/extension.ts', "'faktor.switchSession'"],
    ],
    tests: { vscode: ['faktor.switchSession'] },
  },
  {
    id: 'UIUX-SHELL-JB-CLUSTERS',
    spec: 'I01 [L1394-1404]; DEC-JB-NAV [L103] "Keep the three clusters"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'tabs.addTab("Work", workPlane)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'tabs.addTab("Inspect", inspectTabs)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'tabs.addTab("History", historyTabs)'],
    ],
    tests: { jetbrains: ['Work is the default cluster'] },
  },
  {
    id: 'UIUX-SHELL-JB-INSPECT-SUBVIEWS',
    spec: 'DEC-JB-CURRENT [L86-88]; I01 [L1394-1404] "Inspect: Overview / Plan / Changes / Verification / Agents"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'inspectDetail("Overview", overviewPanel)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'inspectDetail("Plan", treePanel)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'inspectTabs.addTab("Changes", changesPanel)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'inspectDetail("Verification", verificationPanel)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'inspectTabs.addTab("Agents", agentsTabs)'],
    ],
    tests: { jetbrains: ['inspect selector: one detail surface per tab'] },
  },
  {
    id: 'UIUX-SHELL-JB-HISTORY-SWITCH',
    spec: 'I01 [L1394-1404]; S26 [L569-580] "History existing destination" + open conversation',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'historyTabs.addTab("Conversations", historyPanel)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'override fun onOpenSession(sessionId: String)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/HistoryPanel.kt', 'primaryButton("Open")'],
    ],
    tests: { jetbrains: ['history: durable session list + open'] },
  },
  // ------------------------------------------------------------------ I02
  {
    id: 'UIUX-I02-VSC-TASK-OPTIONS',
    spec: 'I02 [L1406-1415] "normalized task options ... draft capture"; DEC-VSC-COMPOSER [L68] "Task options disclose criteria, contract, finish, attachments and per-run budget"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/webview.ts', '<summary>Finish when done</summary>'],
      ['apps/vscode/src/webview.ts', 'Acceptance &amp; contract'],
      ['apps/vscode/src/webview.ts', 'id="contract-budget-tokens"'],
      ['apps/vscode/src/webview.ts', 'id="contract-budget-cost"'],
    ],
    tests: { vscode: ['contract-budget-tokens'] },
  },
  {
    id: 'UIUX-I02-VSC-FINISH-LEVELS',
    spec: 'I02 [L1406-1415] "hierarchical finish levels ... map exact booleans"; ID-VSC-FINISH [L57] "Local / Commit / Push / PR hierarchy"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/webview.ts', 'name="finish-level"'],
      ['apps/vscode/src/webview.ts', 'Leave changes locally'],
      ['apps/vscode/src/webview.ts', 'Commit changes'],
      ['apps/vscode/src/webview.ts', 'Push branch'],
      ['apps/vscode/src/webview.ts', 'Open pull request'],
      ['apps/vscode/src/taskStart.ts', 'include_commit'],
      ['apps/vscode/src/taskStart.ts', 'include_push'],
      ['apps/vscode/src/taskStart.ts', 'include_pr'],
    ],
    tests: { vscode: ['the finish choice is ONE ordered radio group'] },
  },
  {
    id: 'UIUX-I02-JB-FINISH-LEVELS',
    spec: 'I02 [L1406-1415]; DEC-JB-FINISH [L108] "Unify with four hierarchical levels; map to NativeCompletionContract explicitly"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'LOCAL(false, false, false'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'COMMIT(true, false, false'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'PUSH(true, true, false'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'PR(true, true, true'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'includeCommit'],
      ['apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt', 'include_commit'],
    ],
    tests: { jetbrains: ['finish levels:'] },
  },
  {
    id: 'UIUX-I02-JB-TASK-OPTIONS',
    spec: 'I02 [L1406-1415] "normalized task options ... draft capture"; DEC-JB-COMPOSER [L107] "Task options open in a bounded scrollable host sheet"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'FaktorToggleButton("Task options ▸")'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'sectionHeader("Task options")'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'sectionHeader("Per-run budget (empty = machine/run defaults)"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'budgetTokensField'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'budgetCostField'],
    ],
    tests: { jetbrains: ['Task options sheet: bounded, scrollable, draft-preserving, Escape never starts'] },
  },
  // ------------------------------------------------------------------ modes
  {
    id: 'UIUX-MODE-VSC',
    spec: 'COPY-MODES [L107] "clear Task/Ask mode" via DEC-VSC-COMPOSER "Run task primary, Ask as an explicit alternate mode"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/webview.ts', 'Ask Faktor…'],
      ['apps/vscode/src/webview.ts', '>Run task</button>'],
    ],
    tests: { vscode: ['>Ask Faktor…</label>'] },
  },
  {
    id: 'UIUX-MODE-JB',
    spec: 'COPY-MODES [L107] "clear Task/Ask mode"; DEC-JB-COMPOSER "clear Task/Ask mode"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'FaktorToggleButton("Task")'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'FaktorToggleButton("Ask")'],
    ],
    tests: { jetbrains: ['Task/Ask mode'] },
  },
  // ------------------------------------------------------------------ T00-T05 (VS renders the preflight matrix)
  {
    id: 'UIUX-T00-VSC',
    spec: 'T00 [L1003-1005] "Workspace unavailable" / "Open a project to choose where this run works." / "Open project"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Workspace unavailable'],
      ['apps/vscode/src/preflight.ts', 'Open a project to choose where this run works.'],
      ['apps/vscode/src/preflight.ts', 'Open project'],
    ],
    tests: { vscode: ['T00_WORKSPACE_UNAVAILABLE'] },
  },
  {
    id: 'UIUX-T01-VSC',
    spec: 'T01 [L1016-1018] "Faktor is stopped" / "Start the service when you are ready." / "Start service"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Faktor is stopped'],
      ['apps/vscode/src/preflight.ts', 'Start the service when you are ready.'],
      ['apps/vscode/src/preflight.ts', 'Start service'],
    ],
    tests: { vscode: ['T01_STOPPED'] },
  },
  {
    id: 'UIUX-T02-VSC',
    spec: 'T02 [L1021-1024] "Faktor could not find its service" / "Locate an application-level executable or install the service." / "Locate daemon…"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Faktor could not find its service'],
      ['apps/vscode/src/preflight.ts', 'Locate an application-level executable or install the service.'],
      ['apps/vscode/src/preflight.ts', 'Locate daemon…'],
    ],
    tests: { vscode: ['T02_MISSING'] },
  },
  {
    id: 'UIUX-T03-VSC',
    spec: 'T03 [L1027-1030] "Checksum record unavailable" / "Confirm the exact executable before starting." / "Start anyway / Cancel"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Checksum record unavailable'],
      ['apps/vscode/src/preflight.ts', 'Confirm the exact executable before starting.'],
      ['apps/vscode/src/preflight.ts', 'Start anyway'],
      ['apps/vscode/src/preflight.ts', "label: 'Cancel'"],
    ],
    tests: { vscode: ['T03_UNVERIFIED'] },
  },
  {
    id: 'UIUX-T04-VSC',
    spec: 'T04 [L1033-1035] "The service executable does not match its record" / "Choose another executable or repair the installation." / "Locate daemon…"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'The service executable does not match its record'],
      ['apps/vscode/src/preflight.ts', 'Choose another executable or repair the installation.'],
      ['apps/vscode/src/preflight.ts', 'Locate daemon…'],
    ],
    tests: { vscode: ['T04_REFUSED'] },
  },
  {
    id: 'UIUX-T05-VSC',
    spec: 'T05 [L1038-1041] "Starting Faktor" / "Resolving the confirmed executable and waiting for readiness." / "Cancel startup"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Starting Faktor'],
      ['apps/vscode/src/preflight.ts', 'Resolving the confirmed executable and waiting for readiness.'],
      ['apps/vscode/src/preflight.ts', 'Cancel startup'],
    ],
    tests: { vscode: ['T05_STARTING'] },
  },
  // ------------------------------------------------------------------ T06-T12 composer / run states (both hosts)
  {
    id: 'UIUX-T06-VSC',
    spec: 'T06 [L1044-1045] "Ready" / "Describe a task or ask a question." / "Run task"; renders the served ready composer strip (chat.js COMPOSER_STATES.ready only from a served ready preflight with no active run; renderComposerState); the useful action stays the EXISTING Run task control and the strip adds no duplicate button',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Ready'"],
      ['apps/vscode/media/chat.js', "copy: 'Describe a task or ask a question.'"],
      ['apps/vscode/media/chat.js', 'function renderComposerState() {'],
      ['apps/vscode/media/chat.js', "lastPreflight.state === 'ready' && !runActive"],
    ],
    tests: { vscode: ['T06 ready composer strip renders the exact label and copy from the served ready preflight'] },
  },
  {
    id: 'UIUX-T06-JB',
    spec: 'T06 [L1044-1045] "Ready" / "Describe a task or ask a question." / "Run task"; renders the composer empty-state hint (FaktorChatPanel composerReadyLabel/composerHint from RunStateCopy.READY_LABEL/READY_COPY); the useful action stays the EXISTING Run task control',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val READY_LABEL = "Ready"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val READY_COPY = "Describe a task or ask a question."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'sectionHeader(RunStateCopy.READY_LABEL, muted = true)'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'wrappedMutedLabel(RunStateCopy.READY_COPY)'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T07-VSC',
    spec: 'T07 [L1048-1049] "Starting the run" / "Your captured draft is being admitted once." / "Wait"; renders the single-flight submitting strip (chat.js COMPOSER_STATES.starting while the one logical submission is admitted; the Wait action has no affordance, so the strip renders label+copy only and adds no button)',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Starting the run'"],
      ['apps/vscode/media/chat.js', "copy: 'Your captured draft is being admitted once.'"],
      ['apps/vscode/media/chat.js', 'spec = COMPOSER_STATES.starting;'],
      ['apps/vscode/media/chat.js', 'var submitting = false;'],
    ],
    tests: { vscode: ['T07 starting strip renders during the single-flight admission and never twice'] },
  },
  {
    id: 'UIUX-T07-JB',
    spec: 'T07 [L1048-1049] "Starting the run" / "Your captured draft is being admitted once." / "Wait"; renders the startInFlight strip (TaskTreeModel runStateViewForStart with action null — the Wait action has no affordance, so the strip renders label+copy only; FaktorChatPanel startInFlight)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val STARTING_LABEL = "Starting the run"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val STARTING_COPY = "Your captured draft is being admitted once."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'fun runStateViewForStart'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'private val startInFlight = AtomicBoolean(false)'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions', 'task start single-flight: queued double click enqueues ONE request'] },
  },
  {
    id: 'UIUX-T08-VSC',
    spec: 'T08 [L1052-1055] "Start response unavailable" / "The draft was kept. Retry checks the same logical submission." / "Review and retry"; renders the retryable start-result strip (chat.js COMPOSER_STATES.startUnavailable ONLY when the host result classified the failure retryable); the useful action stays the EXISTING Review and retry control and the strip adds no duplicate button',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Start response unavailable'"],
      ['apps/vscode/media/chat.js', "copy: 'The draft was kept. Retry checks the same logical submission.'"],
      ['apps/vscode/media/chat.js', 'spec = COMPOSER_STATES.startUnavailable;'],
      ['apps/vscode/media/chat.js', 'startFailed = !ok && message.retryable === true;'],
    ],
    tests: { vscode: ['T08 start response unavailable keeps the kept draft strip until the next submission'] },
  },
  {
    id: 'UIUX-T08-JB',
    spec: 'T08 [L1052-1055] "Start response unavailable" / "The draft was kept. Retry checks the same logical submission." / "Review and retry"; renders the pendingTaskSubmission transport-failure strip (TaskTreeModel runStateViewForStart submissionRetained; FaktorChatPanel pendingTaskSubmission), keeping the draft',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val START_RESPONSE_UNAVAILABLE_LABEL = "Start response unavailable"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"The draft was kept. Retry checks the same logical submission."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'submissionRetained -> RunStateView('],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'pendingTaskSubmission != null'],
    ],
    tests: { jetbrains: ['task start single-flight: transport failure re-enables and keeps draft + attachments', 'run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T09-VSC',
    spec: 'T09 [L1058-1061] "Running" / "The candidate is separate from your workspace." / "Inspect / Stop current run"; renders the served run strip (chat.js RUN_STATES.running from the newest durable task-run row state running or the collapsed session tag reading running; runStateSpec); the useful actions stay the EXISTING Inspect / Cancel run controls, no duplicate button',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Running'"],
      ['apps/vscode/media/chat.js', "copy: 'The candidate is separate from your workspace.'"],
      ['apps/vscode/media/chat.js', 'function runStateSpec(snapshot) {'],
      ['apps/vscode/media/chat.js', "runState === 'running'"],
      ['apps/vscode/media/chat.js', "tag === 'running'"],
    ],
    tests: { vscode: ['T25/T26/T09 render from the newest durable task-run row, and T12 outranks a running row'] },
  },
  {
    id: 'UIUX-T09-JB',
    spec: 'T09 [L1058-1061] "Running" / "The candidate is separate from your workspace." / "Inspect / Stop current run"; renders the served Running tag strip (TaskTreeModel runStateViewForRunTag; FaktorChatPanel currentRunStateTag banner); the useful actions stay the EXISTING Inspect / Cancel run controls',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val RUNNING_LABEL = "Running"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val RUNNING_COPY = "The candidate is separate from your workspace."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'fun runStateViewForRunTag'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'private var currentRunStateTag: String? = null'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T11-VSC',
    spec: 'T11 [L1077-1079] "Needs your input" / "The run is waiting for a decision or clarification." / "Reply in composer"; renders ONLY from the served needs_user_input machine state (chat.js COMPOSER_STATES.needsInput); the composer itself is the reply surface, so the strip adds no Reply control',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Needs your input'"],
      ['apps/vscode/media/chat.js', "copy: 'The run is waiting for a decision or clarification.'"],
      ['apps/vscode/media/chat.js', "machineStateValue === 'needs_user_input'"],
    ],
    tests: { vscode: ['T11 needs your input renders only from the served needs_user_input machine state'] },
  },
  {
    id: 'UIUX-T11-JB',
    spec: 'T11 [L1077-1079] "Needs your input" / "The run is waiting for a decision or clarification." / "Reply in composer"; renders the served Blocked tag produced only by AgentState::NeedsUserInput (TaskTreeModel runStateViewForRunTag); the composer itself is the reply surface',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val NEEDS_INPUT_LABEL = "Needs your input"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val NEEDS_INPUT_COPY = "The run is waiting for a decision or clarification."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'tag.equals("Blocked", ignoreCase = true)'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T12-VSC',
    spec: 'T12 [L1082-1084] "Checking the change" / "Required results remain attached to this candidate." / "Inspect verification"; renders from the proof body task state verifying/needs_verification (chat.js RUN_STATES.checking from proofTaskState patched off GET /native/tasks/{id}/proof task.state); the useful action stays the EXISTING Inspect control',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Checking the change'"],
      ['apps/vscode/media/chat.js', "copy: 'Required results remain attached to this candidate.'"],
      ['apps/vscode/media/chat.js', "proofState === 'verifying' || proofState === 'needs_verification'"],
      ['apps/vscode/src/extension.ts', 'proofTaskState: proofRead.proof?.task?.state ?? null,'],
    ],
    tests: { vscode: ['T12 checking renders only from the proof body embedded task state'] },
  },
  {
    id: 'UIUX-T12-JB',
    spec: 'T12 [L1082-1084] "Checking the change" / "Required results remain attached to this candidate." / "Inspect verification"; parsed NativeTaskProof.taskState (NativeProtocol) driving TaskTreeModel runStateViewForProofTaskState; the useful action stays the EXISTING Inspect control',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val CHECKING_LABEL = "Checking the change"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val CHECKING_COPY = "Required results remain attached to this candidate."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'fun runStateViewForProofTaskState'],
      ['apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt', 'val taskState: String? = null,'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  // ------------------------------------------------------------------ T13-T17 verification states
  {
    id: 'UIUX-T13-VSC',
    spec: 'T13 [L1087-1089] "Completion pending" / "One required result is missing." / "View missing result"; renders T13 family as "Completion pending" (verificationVerdictOf criterion status line; the proposed supporting copy is not rendered)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "return 'Completion pending'"],
    ],
    tests: { vscode: ['Completion pending'] },
  },
  {
    id: 'UIUX-T14-VSC',
    spec: 'T14 [L1092-1094] "Required check failed" / "Inspect the result and repair the candidate." / "View evidence"; renders T14 family as "Required check failed — <facts>" (verificationVerdictOf)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', 'Required check failed'],
    ],
    tests: { vscode: ['Required check failed'] },
  },
  {
    id: 'UIUX-T15-VSC',
    spec: 'T15 [L1097-1099] "Verification unavailable" / "No verdict is served for this claim." / "Inspect reason / Refresh"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', 'Verification unavailable'],
      ['apps/vscode/media/chat.js', 'No verdict is served for this claim.'],
    ],
    tests: { vscode: ['No verdict is served for this claim.', 'Verification unavailable'] },
  },
  {
    id: 'UIUX-T15-JB',
    spec: 'T15 [L1097-1099] "Verification unavailable" / "No verdict is served for this claim." / "Inspect reason / Refresh"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"No verdict is served for this claim."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OverviewPanel.kt', 'Verification unavailable'],
    ],
    tests: { jetbrains: ['No verdict is served for this claim.'] },
  },
  {
    id: 'UIUX-T16-VSC',
    spec: 'T16 [L1102-1104] "Verification refused" / "A policy or contract prevents this check." / "Inspect reason"; renders T16 family as "Verification refused" (verificationVerdictOf)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "return 'Verification refused'"],
    ],
    tests: { vscode: ['Verification refused'] },
  },
  {
    id: 'UIUX-T17-VSC',
    spec: 'T17 [L1107-1111] "Candidate checks passed" / "The candidate’s required checks passed; integration and finish actions remain separate." / "Review changes"; renders T17 family as "Candidate checks passed — <facts>" (verificationVerdictOf)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', 'Candidate checks passed'],
    ],
    tests: { vscode: ['Candidate checks passed'] },
  },
  {
    id: 'UIUX-T17-JB',
    spec: 'T17 [L1107-1111] "Candidate checks passed"; JB renders "Candidate checks passed · <facts>" (OverviewPanel verification row)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OverviewPanel.kt', '"Candidate checks passed · "'],
    ],
    tests: { jetbrains: ['Candidate checks passed'] },
  },
  // ------------------------------------------------------------------ T10
  {
    id: 'UIUX-T10-VSC',
    spec: 'T10 [L1072-1074] "Faktor needs your approval" / "Review the action, target and stated reason." / "Allow / Deny"',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'Faktor needs your approval'],
      ['apps/vscode/src/webview.ts', 'Review the action, target and stated reason.'],
      ['apps/vscode/media/chat.js', "'Allow'"],
      ['apps/vscode/media/chat.js', "'Deny'"],
    ],
    tests: { vscode: ['Faktor needs your approval'] },
  },
  {
    id: 'UIUX-T10-JB',
    spec: 'T10 [L1072-1074] "Faktor needs your approval" / "Review the action, target and stated reason." / "Allow / Deny"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt', 'Faktor needs your approval'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt', 'Review the action, target and stated reason.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt', 'Allow'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt', 'Deny'],
    ],
    tests: { jetbrains: ['approval renderer: requester-stated reason is labelled, no inferred target, exact fallback'] },
  },
  // ------------------------------------------------------------------ T22
  {
    id: 'UIUX-T22-VSC',
    spec: 'T22 [L1143-1146] "Pull request integration unavailable" / "Configure the native SCM integration; no PR success is inferred." / "Open setup guidance" (native_pr_scm_not_configured path)',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'Pull request integration unavailable'],
      ['apps/vscode/media/chat.js', 'Configure the native SCM integration; no PR success is inferred.'],
      ['apps/vscode/media/chat.js', 'Open setup guidance'],
    ],
    tests: { vscode: ['T22: a requested PR step blocked by SCM config renders the exact copy and the guidance action'] },
  },
  {
    id: 'UIUX-T22-JB',
    spec: 'T22 [L1143-1146] "Pull request integration unavailable" / "Configure the native SCM integration; no PR success is inferred." / "Open setup guidance"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'Pull request integration unavailable'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'Configure the native SCM integration; no PR success is inferred.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'Open setup guidance'],
    ],
    tests: { jetbrains: ['Pull request integration unavailable'] },
  },
  // ------------------------------------------------------------------ T19/T20/T23/T24 integration + proof states
  {
    id: 'UIUX-T19-VSC',
    spec: 'T19 [L1127-1130] "Workspace changed" / "The current workspace no longer matches the integration basis." / "Inspect conflict"; renders in the cockpit Integration section ONLY for a served integration.state conflict (cockpit.ts proofIntegrationLines), beside the pinned ! Conflict facts; Inspect conflict has no affordance, so the row pins label+copy only',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "'! Workspace changed'"],
      ['apps/vscode/src/cockpit.ts', "'The current workspace no longer matches the integration basis.'"],
      ['apps/vscode/src/cockpit.ts', "integration.state === 'conflict'"],
    ],
    tests: { vscode: ['T19 workspace changed renders the exact label and copy beside the pinned Conflict facts'] },
  },
  {
    id: 'UIUX-T19-JB',
    spec: 'T19 [L1127-1130] "Workspace changed" / "The current workspace no longer matches the integration basis." / "Inspect conflict"; renders beside the pinned ! Conflict fact when TaskTreeModel conflictCount > 0; Inspect conflict has no affordance, so the row pins label+copy only',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val WORKSPACE_CHANGED_LABEL = "Workspace changed"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"The current workspace no longer matches the integration basis."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'if (conflictCount != null && conflictCount > 0L) {'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T20-VSC',
    spec: 'T20 [L1133-1135] "Integrating the candidate" / "An integration transaction is in progress." / "Inspect transaction"; renders T20 family as "Integrating" (proofIntegrationLines state text with the … glyph)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "'Integrating'"],
    ],
    tests: { vscode: ['Integrating'] },
  },
  {
    id: 'UIUX-T20-JB',
    spec: 'T20 [L1133-1135] "Integrating the candidate"; renders T20 family as "… Integrating" (TaskTreeModel integrationFacts)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '… Integrating'],
    ],
    tests: { jetbrains: ['Integrating'] },
  },
  {
    id: 'UIUX-T23-VSC',
    spec: 'T23 [L1149-1151] "The result needs verification" / "The landed state differs from the verified candidate." / "Inspect mismatch"; renders T23 family as "Mismatch" (proofIntegrationLines state text with the ✗ glyph)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "'Mismatch'"],
    ],
    tests: { vscode: ['Mismatch'] },
  },
  {
    id: 'UIUX-T23-JB',
    spec: 'T23 [L1149-1151] "The result needs verification"; renders T23 family as "✗ Mismatch" (TaskTreeModel integrationFacts)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '✗ Mismatch'],
    ],
    tests: { jetbrains: ['Mismatch'] },
  },
  {
    id: 'UIUX-T24-VSC',
    spec: 'T24 [L1154-1158] "Verified complete"; VAL-LAND-3 [L1243-1244] "The final label Verified complete must come from the current proof summary"; renders T24 family as "VERIFIED" (proofSummaryLine, strict proof only)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "? 'VERIFIED'"],
    ],
    tests: { vscode: ['cockpit renders the daemon VERIFIED view'] },
  },
  {
    id: 'UIUX-T24-JB',
    spec: 'T24 [L1154-1158]; VAL-LAND-3; renders T24 family as the strict-proof summary "VERIFIED" (TaskTreeModel summaryText)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"verified" -> "VERIFIED"'],
    ],
    tests: { jetbrains: ['verification summary renders VERIFIED only under the strict verified proof'] },
  },
  // ------------------------------------------------------------------ T21/T25/T26 finish + terminal states (both hosts)
  {
    id: 'UIUX-T21-VSC',
    spec: 'T21 [L1138-1140] "Finish action pending" / "A requested action lacks a succeeded durable result." / "View finish actions"; renders on the Completion contract section ONLY when a requested finish step lacks a succeeded durable row (cockpit.ts completionPending); the section is the EXISTING finish-actions inspection surface, no new button',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "'! Finish action pending'"],
      ['apps/vscode/src/cockpit.ts', "'A requested action lacks a succeeded durable result.'"],
      ['apps/vscode/src/cockpit.ts', 'completionPending(view.completion)'],
    ],
    tests: { vscode: ['T21 finish action pending renders only when a requested step lacks a succeeded durable result'] },
  },
  {
    id: 'UIUX-T21-JB',
    spec: 'T21 [L1138-1140] "Finish action pending" / "A requested action lacks a succeeded durable result." / "View finish actions"; renders on the Overview Finish actions row when CompletionView.finishActionPending (TaskTreeModel) holds; the row is the EXISTING finish-actions inspection surface',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val FINISH_ACTION_PENDING_LABEL = "Finish action pending"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val FINISH_ACTION_PENDING_COPY = "A requested action lacks a succeeded durable result."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'val finishActionPending: Boolean'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OverviewPanel.kt', 'if (completion.finishActionPending) {'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T25-VSC',
    spec: 'T25 [L1161-1164] "Run cancelled" / "Cancellation is a terminal run outcome, not verification success." / "Review recorded work / New task"; renders from the newest durable task-run row state cancelled (chat.js RUN_STATES.cancelled); the useful actions stay the EXISTING conversation / New task controls, no duplicate button',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Run cancelled'"],
      ['apps/vscode/media/chat.js', "copy: 'Cancellation is a terminal run outcome, not verification success.'"],
      ['apps/vscode/media/chat.js', "runState === 'cancelled'"],
    ],
    tests: { vscode: ['T25/T26/T09 render from the newest durable task-run row, and T12 outranks a running row'] },
  },
  {
    id: 'UIUX-T25-JB',
    spec: 'T25 [L1161-1164] "Run cancelled" / "Cancellation is a terminal run outcome, not verification success." / "Review recorded work / New task"; renders from the served Cancelled tag (TaskTreeModel runStateViewForRunTag); the T25 action has no control, so the strip renders label+copy only',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val RUN_CANCELLED_LABEL = "Run cancelled"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"Cancellation is a terminal run outcome, not verification success."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'tag.equals("Cancelled", ignoreCase = true)'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  {
    id: 'UIUX-T26-VSC',
    spec: 'T26 [L1167-1170] "Run failed" / "The result and reason remain available for inspection." / "Inspect result"; renders from the newest durable task-run row state failed (chat.js RUN_STATES.failed); the result and reason stay inspectable through the EXISTING surfaces',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', "label: 'Run failed'"],
      ['apps/vscode/media/chat.js', "copy: 'The result and reason remain available for inspection.'"],
      ['apps/vscode/media/chat.js', "runState === 'failed'"],
    ],
    tests: { vscode: ['T25/T26/T09 render from the newest durable task-run row, and T12 outranks a running row'] },
  },
  {
    id: 'UIUX-T26-JB',
    spec: 'T26 [L1167-1170] "Run failed" / "The result and reason remain available for inspection." / "Inspect result"; renders from the served Failed tag (TaskTreeModel runStateViewForRunTag); the strip renders label+copy only',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val RUN_FAILED_LABEL = "Run failed"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'const val RUN_FAILED_COPY = "The result and reason remain available for inspection."'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'tag.equals("Failed", ignoreCase = true)'],
    ],
    tests: { jetbrains: ['run-state copy: T06-T26 exact labels render only under their served conditions'] },
  },
  // ------------------------------------------------------------------ T27
  {
    id: 'UIUX-T27-VSC',
    spec: 'T27 [L1181-1183] "Reconnecting live updates" / "The last valid journal position is kept." / "Wait / Refresh"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Reconnecting live updates'],
      ['apps/vscode/src/preflight.ts', 'The last valid journal position is kept.'],
      ['apps/vscode/src/preflight.ts', "label: 'Wait'"],
      ['apps/vscode/src/preflight.ts', "label: 'Refresh'"],
    ],
    tests: { vscode: ['T27_RECONNECTING'] },
  },
  {
    id: 'UIUX-T27-JB',
    spec: 'T27 [L1181-1183] "Reconnecting live updates" / "The last valid journal position is kept." / "Wait / Refresh"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Reconnecting live updates'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'The last valid journal position is kept.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Wait'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Refresh'],
    ],
    tests: { jetbrains: ['Reconnecting live updates'] },
  },
  // ------------------------------------------------------------------ T28
  {
    id: 'UIUX-T28-VSC',
    spec: 'T28 [L1186-1190] "Live updates paused" / "The client cannot read the next durable event. Nothing is skipped." / "Reconnect / Refresh from snapshot"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Live updates paused'],
      ['apps/vscode/src/preflight.ts', 'The client cannot read the next durable event. Nothing is skipped.'],
      ['apps/vscode/src/preflight.ts', "label: 'Reconnect'"],
      ['apps/vscode/src/preflight.ts', "label: 'Refresh from snapshot'"],
    ],
    tests: { vscode: ['T28_PAUSED'] },
  },
  {
    id: 'UIUX-T28-JB',
    spec: 'T28 [L1186-1190] "Live updates paused" / "The client cannot read the next durable event. Nothing is skipped." / "Reconnect / Refresh from snapshot"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Live updates paused'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'The client cannot read the next durable event. Nothing is skipped.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Reconnect'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Refresh from snapshot'],
    ],
    tests: { jetbrains: ['The client cannot read the next durable event. Nothing is skipped.'] },
  },
  // ------------------------------------------------------------------ T29
  {
    id: 'UIUX-T29-VSC',
    spec: 'T29 [L1193-1195] "Cost unavailable" / "The current authority has not supplied a cost value." / "View details"',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'Cost unavailable'],
      ['apps/vscode/media/chat.js', 'The current authority has not supplied a cost value.'],
      ['apps/vscode/media/chat.js', 'View details'],
    ],
    tests: { vscode: ['The current authority has not supplied a cost value.'] },
  },
  {
    id: 'UIUX-T29-JB',
    spec: 'T29 [L1193-1195] "Cost unavailable" / "The current authority has not supplied a cost value." / "View details"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', 'Cost unavailable'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', 'The current authority has not supplied a cost value.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', 'View details'],
    ],
    tests: { jetbrains: ['The current authority has not supplied a cost value.'] },
  },
  // ------------------------------------------------------------------ T30 / T33
  {
    id: 'UIUX-T30-VSC',
    spec: 'T30 [L1198-1199] "Billing is disabled locally" / "This surface has no active billing data." / "Back"; renders T30 family as "billing disabled locally — <reason>" (cockpit usagePanelLines)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', 'billing disabled locally'],
    ],
    tests: { vscode: ['billing disabled locally'] },
  },
  {
    id: 'UIUX-T30-JB',
    spec: 'T30 [L1198-1199] "Billing is disabled locally"; renders T30 family as "Billing is disabled locally; no period numbers are served." (UsagePanel disabled card)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/UsagePanel.kt', 'Billing is disabled locally; no period numbers are served.'],
    ],
    tests: { jetbrains: ['Billing is disabled locally; no period numbers are served.'] },
  },
  {
    id: 'UIUX-T33-VSC',
    spec: 'T33 [L1212-1215] "Repository context is partial" / "The index is rebuilding or capped; no full coverage is implied." / "Inspect coverage"; renders T33 family as "index: partial <n>/<m> files" (nativeClient indexCoverageLabel)',
    host: 'vscode',
    must: [
      ['apps/vscode/src/nativeClient.ts', 'index: partial'],
    ],
    tests: { vscode: ['index: partial'] },
  },
  {
    id: 'UIUX-T33-JB',
    spec: 'T33 [L1212-1215] "Repository context is partial"; renders T33 family as "index: partial <n>/<m> files" (shared NativeProtocol indexCoverageLabel)',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt', 'index: partial'],
    ],
    tests: { jetbrains: ['index: partial'] },
  },
  // ------------------------------------------------------------------ T31
  {
    id: 'UIUX-T31-VSC',
    spec: 'T31 [L1202-1204] "History unavailable" / "The conversation list could not be read." / "Refresh history"',
    host: 'vscode',
    must: [
      ['apps/vscode/media/history-state.js', 'History unavailable'],
      ['apps/vscode/media/history-state.js', 'The conversation list could not be read.'],
      ['apps/vscode/media/history-state.js', 'Refresh history'],
    ],
    tests: { vscode: ['History unavailable'] },
  },
  {
    id: 'UIUX-T31-JB',
    spec: 'T31 [L1202-1204] "History unavailable" / "The conversation list could not be read." / "Refresh history"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/HistoryPanel.kt', 'History unavailable'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/HistoryPanel.kt', 'The conversation list could not be read.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/HistoryPanel.kt', 'Refresh history'],
    ],
    tests: { jetbrains: ['History unavailable'] },
  },
  // ------------------------------------------------------------------ T32
  {
    id: 'UIUX-T32-VSC',
    spec: 'T32 [L1207-1209] "Shell execution needs a decision" / "The secure default remains fail-closed on this platform." / "Open setup guidance"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'Shell execution needs a decision'],
      ['apps/vscode/src/preflight.ts', 'The secure default remains fail-closed on this platform.'],
      ['apps/vscode/src/preflight.ts', "label: 'Open setup guidance'"],
    ],
    tests: { vscode: ['SANDBOX_GUIDANCE'] },
  },
  {
    id: 'UIUX-T32-JB',
    spec: 'T32 [L1207-1209] "Shell execution needs a decision" / "The secure default remains fail-closed on this platform." / "Open setup guidance"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Shell execution needs a decision'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'The secure default remains fail-closed on this platform.'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'Open setup guidance'],
    ],
    tests: { jetbrains: ['Shell execution needs a decision'] },
  },
  // ------------------------------------------------------------------ I03
  {
    id: 'UIUX-I03-VSC',
    spec: 'I03 [L1417-1428] "immutable source basis ... do not use unrelated live contents"; F08 [L847] "show diff unavailable"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/changeReview.ts', 'DIFF_UNAVAILABLE_LABEL'],
      ['apps/vscode/src/changeReview.ts', 'The runtime does not serve an immutable candidate content read for this change.'],
      ['apps/vscode/media/chat.js', 'DIFF_UNAVAILABLE_COPY'],
    ],
    tests: { vscode: ['the adapter reads ONLY the two served evidence calls (no workspace read)', 'must never read a workspace file', 'Diff unavailable'] },
  },
  {
    id: 'UIUX-I03-JB',
    spec: 'I03 [L1417-1428] "immutable source basis"; FLOW-F08-DIFFUNAVAIL [L847] "show diff unavailable rather than compare unrelated live files"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ChangeReview.kt', 'DIFF_UNAVAILABLE_LABEL'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ChangeReview.kt', 'DIFF_UNAVAILABLE_COPY'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ChangesPanel.kt', 'DIFF_UNAVAILABLE_LABEL'],
    ],
    tests: { jetbrains: ['I03 no workspace file path is ever opened (adapter seam)', 'the I03 Diff unavailable state must be selectable'] },
  },
  // ------------------------------------------------------------------ I04
  {
    id: 'UIUX-I04-VSC',
    spec: 'I04 [L1430-1441] "Separate proof/acceptance/integration/finish and preserve unavailable fields"; VAL-LAND-2/3; PROH-42 "do not format null as zero"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', "? 'VERIFIED'"],
      ['apps/vscode/src/cockpit.ts', "title: 'Integration'"],
      ['apps/vscode/src/cockpit.ts', "'Not integrated'"],
      ['apps/vscode/src/cockpit.ts', "'Landed state matches verified state'"],
      ['apps/vscode/src/cockpit.ts', 'Conflict'],
      ['apps/vscode/src/outcomes.ts', 'COST_UNAVAILABLE_LABEL'],
    ],
    tests: { vscode: ['integration is its own section: served states render distinct facts, absence is unavailable', 'a missing spend never renders as zero', 'cockpit renders the daemon VERIFIED view'] },
  },
  {
    id: 'UIUX-I04-JB',
    spec: 'I04 [L1430-1441] "Separate proof/acceptance/integration/finish"; VAL-LAND-2 "Show Not integrated, Integrating, Conflict, Landed state matches verified state or Mismatch as separate facts"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"VERIFIED"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OverviewPanel.kt', '"Candidate checks passed · "'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OverviewPanel.kt', '"Cost unavailable"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'Not integrated'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', 'Landed state matches verified state'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreeModel.kt', '"! Conflict"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreePanel.kt', '"integration: " + fact'],
    ],
    tests: { jetbrains: ['verification summary renders VERIFIED only under the strict verified proof', 'overview keeps proof, spend and completed states honest', 'integration: ✓ Landed state matches verified state', '! Conflict'] },
  },
  // ------------------------------------------------------------------ I07
  {
    id: 'UIUX-I07-VSC',
    spec: 'I07 [L1471-1485] "current VCSC/local cohort outcome analytics; no unavailable-as-zero"; VAL-COST-4/5',
    host: 'vscode',
    must: [
      ['apps/vscode/src/outcomes.ts', "'Successful-Run Cost'"],
      ['apps/vscode/src/outcomes.ts', "'Verified Cost per Successful Change (VCSC)'"],
      ['apps/vscode/src/outcomes.ts', "'No verified changes yet'"],
      ['apps/vscode/src/outcomes.ts', "'not measured'"],
      ['apps/vscode/src/outcomes.ts', "'Cost unavailable'"],
    ],
    tests: { vscode: ['Successful-Run Cost', 'Verified Cost per Successful Change (VCSC)', 'not measured'] },
  },
  {
    id: 'UIUX-I07-JB',
    spec: 'I07 [L1471-1485] "current VCSC/local cohort outcome analytics; no unavailable-as-zero"; VAL-COST-4/5',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', '"Successful-Run Cost"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', '"Verified Cost per Successful Change (VCSC)"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', '"No verified changes yet"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', '"not measured"'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/OutcomesPanel.kt', '"Cost unavailable"'],
    ],
    tests: { jetbrains: ['Successful-Run Cost', 'Verified Cost per Successful Change (VCSC)', 'not measured'] },
  },
  // ------------------------------------------------------------------ I05 / I06 / I10
  {
    id: 'UIUX-I05-D14-VSC',
    spec: 'I05 [L1445-1454] + D14 [L250-261] "exact reason ... otherwise Action requested for this run."; PROH-14 "No inferred purpose"',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'Action requested for this run.'],
      ['apps/vscode/media/chat.js', 'permissionInFlight'],
    ],
    tests: { vscode: ['Action requested for this run.', 'one click posts exactly one reply'] },
  },
  {
    id: 'UIUX-I05-D14-JB',
    spec: 'I05 [L1445-1454] + D14 [L250-261]; PROH-13/14 "never silently retry", "No inferred purpose"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt', 'APPROVAL_FALLBACK_REASON'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ApprovalCards.kt', 'ApprovalReplyGate'],
    ],
    tests: { jetbrains: ['approval gating: one reply per request id, refusal keeps the card, rebuild keeps pending'] },
  },
  {
    id: 'UIUX-I05-TYPED-REFUSAL-VSC',
    spec: 'I05 [L1445-1454] "typed expired/wrong-session refusals"; VAL-APPR-3 "refusal remains visible and is not retried implicitly"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/errors.ts', "permission_expired"],
    ],
    tests: { vscode: ['unknown/expired id: the typed 409 conflict is surfaced and never retried'] },
  },
  {
    id: 'UIUX-I05-TYPED-REFUSAL-JB',
    spec: 'I05 [L1445-1454] "typed expired/wrong-session refusals"; VAL-APPR-3',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/PermissionsPanel.kt', 'setUnavailable'],
    ],
    tests: { jetbrains: ['permission reply: typed 409 refusals classify and render explicitly'] },
  },
  {
    id: 'UIUX-I06-NO-IMPLICIT-START-VSC',
    spec: 'I06 [L1456-1469] + F01 [L725-726] "Opening the view never implicitly grants binary or shell execution"; VAL-HOST-5 "Opening tool window does not launch the service"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'there is no implicit start path'],
      ['apps/vscode/src/extension.ts', 'never an implicit start'],
    ],
    tests: { vscode: ['opening the view posts only ready'] },
  },
  {
    id: 'UIUX-I06-NO-IMPLICIT-START-JB',
    spec: 'I06 [L1456-1469] + F01 [L725-726]; VAL-HOST-5 "Opening tool window does not launch the service"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorToolWindowFactory.kt', 'NO automatic daemon start'],
    ],
    tests: { jetbrains: ['I06: opening the tool window never starts the daemon implicitly'] },
  },
  {
    id: 'UIUX-I10-SANDBOX-VSC',
    spec: 'I10 [L1515-1524] "sandbox first-run guidance (guidance only)" + DEFER-1 "native choice write control deferred"; VAL-O03 "no promotions / UI-only bypass"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/preflight.ts', 'faktor sandbox choose'],
      ['apps/vscode/src/preflight.ts', 'cannot change shell'],
    ],
    tests: { vscode: ['SANDBOX_GUIDANCE'] },
  },
  {
    id: 'UIUX-I10-SANDBOX-JB',
    spec: 'I10 [L1515-1524] "guidance only"; DEFER-4 [L988-989] "a new IDE write route is not currently established"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'SANDBOX_GUIDANCE_COMMAND'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/DiagnosticsPanel.kt', 'UI write control'],
    ],
    tests: { jetbrains: ['diagnostics setup: T32 copy + exact sandbox guidance command'] },
  },
  // ------------------------------------------------------------------ validation matrix rows
  {
    id: 'UIUX-VAL-TEXT-GLYPH-VSC',
    spec: 'VAL-A11Y-8/9 + §6.2 [L1369,L1375] "Text+glyph for every semantic state", "no color-only success or failure"',
    host: 'vscode',
    must: [
      ['apps/vscode/src/cockpit.ts', 'const glyph ='],
      ['apps/vscode/src/cockpit.ts', 'the glyph is never the only signal'],
    ],
    tests: { vscode: ['integration is its own section: served states render distinct facts, absence is unavailable'] },
  },
  {
    id: 'UIUX-VAL-TEXT-GLYPH-JB',
    spec: 'VAL-A11Y-8/9 "Text+glyph for every semantic state", "no color-only success or failure"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/VerificationPanel.kt', 'WrappedLabel("$marker ${row.criterionKey}")'],
    ],
    tests: { jetbrains: ['theme/semantic-colors/', 'criterion state must be carried by the leading verdict text'] },
  },
  {
    id: 'UIUX-VAL-REDUCED-MOTION-VSC',
    spec: 'VAL-A11Y-12 [L1381-1382] "Reduced motion keeps static state cues; screen-reader mode avoids ornamental animation"',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.css', '@media (prefers-reduced-motion: reduce)'],
      ['apps/vscode/media/chat.css', 'animation: none'],
    ],
    tests: { vscode: ['prefers-reduced-motion disables every .pixel-* animation with static cues'] },
  },
  {
    id: 'UIUX-VAL-REDUCED-MOTION-JB',
    spec: 'VAL-A11Y-12 "Reduced motion keeps static state cues"; PROH-59 "screen-reader mode avoids ornamental animation"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/PixelAgents.kt', 'fun reducedMotion(): Boolean'],
    ],
    tests: { jetbrains: ['reduced motion must not animate'] },
  },
  {
    id: 'UIUX-VAL-KEYBOARD-VSC',
    spec: 'VAL-KB-SEND/CLUSTERS/CLOSE [L1336-1348] "Ctrl+Enter ... Enter remains newline; IME composition never submits", tablist arrow keys, Escape closes',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'function wireTablist'],
      ['apps/vscode/media/chat.js', 'composing'],
    ],
    tests: { vscode: ['the selected tab is the roving tabindex entry', 'ArrowLeft wraps to the last subview'] },
  },
  {
    id: 'UIUX-VAL-KEYBOARD-JB',
    spec: 'VAL-KB-CLOSE [L1348] "Escape returns to exact trigger"; KEEP-27; VAL-A11Y-11 "Native keyboard traversal; no focus trap"',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'faktor.closeOptions'],
    ],
    tests: { jetbrains: ['check("keyboard/'] },
  },
  {
    id: 'UIUX-VAL-EMPTY-HONESTY-VSC',
    spec: 'VAL-A11Y-10 + §6 "empty/unavailable honesty"; S13 "No candidate changes are served for this run yet."',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'No candidate changes are served for this run yet.'],
    ],
    tests: { vscode: ['webview changes: empty and unavailable states are exact, with no stale rows'] },
  },
  {
    id: 'UIUX-VAL-EMPTY-HONESTY-JB',
    spec: '§6 empty/unavailable honesty; host-matrix empty axis must render the honest empty state',
    host: 'jetbrains',
    must: [
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/ChangesPanel.kt', 'CHANGES_EMPTY_COPY'],
    ],
    tests: { jetbrains: ['state/empty/'] },
  },
];

// ---------------------------------------------------------------------------
// UI/UX state dispositions (contract-uiux.md §2 T00-T33). Exactly one
// reviewed entry per state:
//   - `enforced`: a host renders the state's mandated label/copy group (or
//     its documented shipped representation, mapped in the note) and `rows`
//     references the UIUX_CONTRACT ids that pin it; a dangling reference or
//     a lost production needle is a gap.
//   - `proposed`: no host renders the mandated copy yet; `copy` records the
//     exact mandated sentence(s) so a later product fix that ships one turns
//     the entry stale (`uiux-state-proposal-stale`) instead of silent, and
//     the note carries the spec citation plus the citing screens' status
//     classes. The PDF marks every layout a design proposal; T24 is the only
//     state whose "Verified complete?" cell reads "Yes, only current full
//     proof". Cross-cutting §6 rows (reduced motion, keyboard, empty
//     honesty) stay pinned in UIUX_CONTRACT; the statically checkable
//     text+glyph rows are folded into the dispositions they anchor.
// ---------------------------------------------------------------------------
const UIUX_STATE_IDS = Array.from({ length: 34 }, (_, index) => `T${String(index).padStart(2, '0')}`);

const UIUX_STATE_DISPOSITIONS = [
  {
    state: 'T00',
    spec: 'T00 [L1003-1005] "Workspace unavailable" / "Open a project to choose where this run works." / "Open project"',
    disposition: 'enforced',
    rows: ['UIUX-T00-VSC'],
    note: 'VS Code preflight.ts renders the mandated label, copy and Open project action; pinned by UIUX-T00-VSC and its T00_WORKSPACE_UNAVAILABLE test.',
  },
  {
    state: 'T01',
    spec: 'T01 [L1016-1018] "Faktor is stopped" / "Start the service when you are ready." / "Start service"',
    disposition: 'enforced',
    rows: ['UIUX-T01-VSC'],
    note: 'VS Code preflight.ts renders the mandated label, copy and Start service action; pinned by UIUX-T01-VSC and its T01_STOPPED test.',
  },
  {
    state: 'T02',
    spec: 'T02 [L1021-1024] "Faktor could not find its service" / "Locate an application-level executable or install the service." / "Locate daemon…"',
    disposition: 'enforced',
    rows: ['UIUX-T02-VSC'],
    note: 'VS Code preflight.ts renders the mandated label, copy and Locate daemon… action; pinned by UIUX-T02-VSC and its T02_MISSING test.',
  },
  {
    state: 'T03',
    spec: 'T03 [L1027-1030] "Checksum record unavailable" / "Confirm the exact executable before starting." / "Start anyway / Cancel"',
    disposition: 'enforced',
    rows: ['UIUX-T03-VSC'],
    note: 'VS Code preflight.ts renders the mandated label, copy and Start anyway / Cancel actions; pinned by UIUX-T03-VSC and its T03_UNVERIFIED test.',
  },
  {
    state: 'T04',
    spec: 'T04 [L1033-1035] "The service executable does not match its record" / "Choose another executable or repair the installation." / "Locate daemon…"',
    disposition: 'enforced',
    rows: ['UIUX-T04-VSC'],
    note: 'VS Code preflight.ts renders the mandated label, copy and Locate daemon… action; pinned by UIUX-T04-VSC and its T04_REFUSED test.',
  },
  {
    state: 'T05',
    spec: 'T05 [L1038-1041] "Starting Faktor" / "Resolving the confirmed executable and waiting for readiness." / "Cancel startup"',
    disposition: 'enforced',
    rows: ['UIUX-T05-VSC'],
    note: 'VS Code preflight.ts renders the mandated label, copy and Cancel startup action; pinned by UIUX-T05-VSC and its T05_STARTING test.',
  },
  {
    state: 'T06',
    spec: 'T06 [L1044-1045] "Ready" / "Describe a task or ask a question." / "Run task"',
    disposition: 'enforced',
    rows: ['UIUX-T06-VSC', 'UIUX-T06-JB'],
    note: 'both hosts render the mandated label and copy from a SERVED ready condition only: VS composer strip (chat.js COMPOSER_STATES.ready from a served ready preflight with no active run) and JB composer empty-state hint (RunStateCopy.READY_LABEL/READY_COPY); the existing Run task control carries the action; pinned by UIUX-T06-VSC/UIUX-T06-JB.',
  },
  {
    state: 'T07',
    spec: 'T07 [L1048-1049] "Starting the run" / "Your captured draft is being admitted once." / "Wait"',
    disposition: 'enforced',
    rows: ['UIUX-T07-VSC', 'UIUX-T07-JB'],
    note: 'both hosts render the mandated label and copy under the single-flight admission: VS submitting strip (chat.js COMPOSER_STATES.starting) and JB startInFlight strip (runStateViewForStart); the Wait action has no affordance in either host, so only label+copy render; pinned by UIUX-T07-VSC/UIUX-T07-JB.',
  },
  {
    state: 'T08',
    spec: 'T08 [L1052-1055] "Start response unavailable" / "The draft was kept. Retry checks the same logical submission." / "Review and retry"',
    disposition: 'enforced',
    rows: ['UIUX-T08-VSC', 'UIUX-T08-JB'],
    note: 'both hosts render the mandated label and copy after a RETRYABLE/lost start response: VS retryable start-result strip (chat.js COMPOSER_STATES.startUnavailable only when the host classified the failure retryable) and JB pendingTaskSubmission transport-failure strip; the kept draft and the existing Review and retry control carry the action; pinned by UIUX-T08-VSC/UIUX-T08-JB.',
  },
  {
    state: 'T09',
    spec: 'T09 [L1058-1061] "Running" / "The candidate is separate from your workspace." / "Inspect / Stop current run"',
    disposition: 'enforced',
    rows: ['UIUX-T09-VSC', 'UIUX-T09-JB'],
    note: 'both hosts render the mandated label and copy from a served running state: VS newest durable task-run row Running / collapsed session tag running (chat.js RUN_STATES.running, runStateSpec) and JB served Running tag banner (runStateViewForRunTag); the existing Inspect / Cancel run controls carry the actions; pinned by UIUX-T09-VSC/UIUX-T09-JB.',
  },
  {
    state: 'T10',
    spec: 'T10 [L1072-1074] "Faktor needs your approval" / "Review the action, target and stated reason." / "Allow / Deny"',
    disposition: 'enforced',
    rows: ['UIUX-T10-VSC', 'UIUX-T10-JB'],
    note: 'both hosts render the mandated label and copy and per-request Allow / Deny: VS chat.js cards + webview.ts card markup, JB ApprovalCards.kt; pinned by UIUX-T10-VSC and UIUX-T10-JB.',
  },
  {
    state: 'T11',
    spec: 'T11 [L1077-1079] "Needs your input" / "The run is waiting for a decision or clarification." / "Reply in composer"',
    disposition: 'enforced',
    rows: ['UIUX-T11-VSC', 'UIUX-T11-JB'],
    note: 'both hosts render the mandated label and copy from the SERVED awaiting-input condition: VS needs_user_input machine state (chat.js COMPOSER_STATES.needsInput) and JB served Blocked tag produced only by AgentState::NeedsUserInput (runStateViewForRunTag); the composer itself is the reply surface; pinned by UIUX-T11-VSC/UIUX-T11-JB.',
  },
  {
    state: 'T12',
    spec: 'T12 [L1082-1084] "Checking the change" / "Required results remain attached to this candidate." / "Inspect verification"',
    disposition: 'enforced',
    rows: ['UIUX-T12-VSC', 'UIUX-T12-JB'],
    note: 'both hosts render the mandated label and copy from the proof body task state verifying/needs_verification: VS chat.js RUN_STATES.checking from proofTaskState patched off GET /native/tasks/{id}/proof task.state and JB parsed NativeTaskProof.taskState (runStateViewForProofTaskState); the existing Inspect control carries the action; pinned by UIUX-T12-VSC/UIUX-T12-JB.',
  },
  {
    state: 'T13',
    spec: 'T13 [L1087-1089] "Completion pending" / "One required result is missing." / "View missing result"',
    disposition: 'enforced',
    rows: ['UIUX-T13-VSC'],
    note: 'renders T13 family as "Completion pending" (VS cockpit verificationVerdictOf criterion status line); the proposed supporting copy is not rendered; pinned by UIUX-T13-VSC.',
  },
  {
    state: 'T14',
    spec: 'T14 [L1092-1094] "Required check failed" / "Inspect the result and repair the candidate." / "View evidence"',
    disposition: 'enforced',
    rows: ['UIUX-T14-VSC'],
    note: 'renders T14 family as "Required check failed — <facts>" (VS cockpit verificationVerdictOf); the proposed supporting copy is not rendered; pinned by UIUX-T14-VSC.',
  },
  {
    state: 'T15',
    spec: 'T15 [L1097-1099] "Verification unavailable" / "No verdict is served for this claim." / "Inspect reason / Refresh"',
    disposition: 'enforced',
    rows: ['UIUX-T15-VSC', 'UIUX-T15-JB', 'UIUX-VAL-TEXT-GLYPH-JB'],
    note: 'both hosts render the mandated label and the exact sentence: VS cockpit label + chat.js criterion card, JB Overview label + TaskTreeModel NO_VERDICT_COPY rendered by VerificationPanel/TaskTreePanel; pinned by UIUX-T15-VSC/UIUX-T15-JB and the JB criterion text+glyph row.',
  },
  {
    state: 'T16',
    spec: 'T16 [L1102-1104] "Verification refused" / "A policy or contract prevents this check." / "Inspect reason"',
    disposition: 'enforced',
    rows: ['UIUX-T16-VSC'],
    note: 'renders T16 family as "Verification refused" (VS cockpit verificationVerdictOf); the proposed supporting copy is not rendered; pinned by UIUX-T16-VSC.',
  },
  {
    state: 'T17',
    spec: 'T17 [L1107-1111] "Candidate checks passed" / "The candidate’s required checks passed; integration and finish actions remain separate." / "Review changes"',
    disposition: 'enforced',
    rows: ['UIUX-T17-VSC', 'UIUX-T17-JB'],
    note: 'renders T17 family as "Candidate checks passed — <facts>" (VS cockpit verificationVerdictOf) and "Candidate checks passed · <facts>" (JB OverviewPanel); the proposed supporting copy is not rendered; pinned by UIUX-T17-VSC/UIUX-T17-JB.',
  },
  {
    state: 'T18',
    spec: 'T18 [L1114-1116] "Not integrated" / "The verified candidate has not landed." / "Review changes"',
    disposition: 'enforced',
    rows: ['UIUX-I04-VSC', 'UIUX-I04-JB', 'UIUX-VAL-TEXT-GLYPH-VSC'],
    note: 'renders T18 family as the exact "Not integrated" integration fact in both hosts (VS cockpit proofIntegrationLines, JB TaskTreeModel integrationFacts, rendered text+glyph); pinned by UIUX-I04-VSC/UIUX-I04-JB.',
  },
  {
    state: 'T19',
    spec: 'T19 [L1127-1130] "Workspace changed" / "The current workspace no longer matches the integration basis." / "Inspect conflict"',
    disposition: 'enforced',
    rows: ['UIUX-T19-VSC', 'UIUX-T19-JB'],
    note: 'both hosts render the mandated label and copy beside the pinned ! Conflict facts from a served conflict: VS cockpit Integration section on integration.state conflict (proofIntegrationLines) and JB TaskTreeModel conflictCount > 0 (integrationFacts); Inspect conflict has no affordance in either host, so only label+copy render; pinned by UIUX-T19-VSC/UIUX-T19-JB.',
  },
  {
    state: 'T20',
    spec: 'T20 [L1133-1135] "Integrating the candidate" / "An integration transaction is in progress." / "Inspect transaction"',
    disposition: 'enforced',
    rows: ['UIUX-T20-VSC', 'UIUX-T20-JB', 'UIUX-VAL-TEXT-GLYPH-VSC'],
    note: 'renders T20 family as "Integrating" (VS cockpit proofIntegrationLines, glyph …; JB "… Integrating" TaskTreeModel), text+glyph; pinned by UIUX-T20-VSC/UIUX-T20-JB.',
  },
  {
    state: 'T21',
    spec: 'T21 [L1138-1140] "Finish action pending" / "A requested action lacks a succeeded durable result." / "View finish actions"',
    disposition: 'enforced',
    rows: ['UIUX-T21-VSC', 'UIUX-T21-JB'],
    note: 'both hosts render the mandated label and copy only when a requested finish step lacks a succeeded durable result: VS cockpit completionPending on the Completion contract section and JB CompletionView.finishActionPending on the Overview Finish actions row; both surfaces are the existing finish-actions inspection home; pinned by UIUX-T21-VSC/UIUX-T21-JB.',
  },
  {
    state: 'T22',
    spec: 'T22 [L1143-1146] "Pull request integration unavailable" / "Configure the native SCM integration; no PR success is inferred." / "Open setup guidance"',
    disposition: 'enforced',
    rows: ['UIUX-T22-VSC', 'UIUX-T22-JB'],
    note: 'both hosts render the mandated label, copy and Open setup guidance action (VS chat.js pr-scm-setup notice from the native_pr_scm_not_configured step, JB FaktorChatPanel); pinned by UIUX-T22-VSC/UIUX-T22-JB.',
  },
  {
    state: 'T23',
    spec: 'T23 [L1149-1151] "The result needs verification" / "The landed state differs from the verified candidate." / "Inspect mismatch"',
    disposition: 'enforced',
    rows: ['UIUX-T23-VSC', 'UIUX-T23-JB', 'UIUX-VAL-TEXT-GLYPH-VSC'],
    note: 'renders T23 family as "Mismatch" (VS cockpit proofIntegrationLines, glyph ✗; JB "✗ Mismatch" TaskTreeModel), text+glyph; pinned by UIUX-T23-VSC/UIUX-T23-JB.',
  },
  {
    state: 'T24',
    spec: 'T24 [L1154-1158] "Verified complete" / "The current proof summary supports completion and requested finish actions." / "Review result / New task"; "Verified complete?" cell "Yes, only current full proof"',
    disposition: 'enforced',
    rows: ['UIUX-T24-VSC', 'UIUX-T24-JB', 'UIUX-I04-VSC', 'UIUX-I04-JB'],
    note: 'renders T24 family as "VERIFIED" only from the current strict proof summary (VS cockpit proofSummaryLine, JB TaskTreeModel summaryText; VAL-LAND-3); pinned by UIUX-T24-VSC/UIUX-T24-JB and the I04 proof rows.',
  },
  {
    state: 'T25',
    spec: 'T25 [L1161-1164] "Run cancelled" / "Cancellation is a terminal run outcome, not verification success." / "Review recorded work / New task"',
    disposition: 'enforced',
    rows: ['UIUX-T25-VSC', 'UIUX-T25-JB'],
    note: 'both hosts render the mandated label and copy from a SERVED terminal cancellation: VS newest durable task-run row Cancelled (chat.js RUN_STATES.cancelled) and JB served Cancelled tag (runStateViewForRunTag); the existing conversation / New task controls carry the recorded-work action and the JB strip renders label+copy only; pinned by UIUX-T25-VSC/UIUX-T25-JB.',
  },
  {
    state: 'T26',
    spec: 'T26 [L1167-1170] "Run failed" / "The result and reason remain available for inspection." / "Inspect result"',
    disposition: 'enforced',
    rows: ['UIUX-T26-VSC', 'UIUX-T26-JB'],
    note: 'both hosts render the mandated label and copy from a SERVED terminal failure: VS newest durable task-run row Failed (chat.js RUN_STATES.failed) and JB served Failed tag (runStateViewForRunTag); the result and reason stay inspectable through the existing surfaces and the JB strip renders label+copy only; pinned by UIUX-T26-VSC/UIUX-T26-JB.',
  },
  {
    state: 'T27',
    spec: 'T27 [L1181-1183] "Reconnecting live updates" / "The last valid journal position is kept." / "Wait / Refresh"',
    disposition: 'enforced',
    rows: ['UIUX-T27-VSC', 'UIUX-T27-JB'],
    note: 'both hosts render the mandated label and copy and Wait / Refresh (VS preflight + chat.js, JB DiagnosticsPanel); pinned by UIUX-T27-VSC/UIUX-T27-JB.',
  },
  {
    state: 'T28',
    spec: 'T28 [L1186-1190] "Live updates paused" / "The client cannot read the next durable event. Nothing is skipped." / "Reconnect / Refresh from snapshot"',
    disposition: 'enforced',
    rows: ['UIUX-T28-VSC', 'UIUX-T28-JB'],
    note: 'both hosts render the mandated label and copy and Reconnect / Refresh from snapshot (VS preflight + chat.js, JB DiagnosticsPanel); pinned by UIUX-T28-VSC/UIUX-T28-JB.',
  },
  {
    state: 'T29',
    spec: 'T29 [L1193-1195] "Cost unavailable" / "The current authority has not supplied a cost value." / "View details"',
    disposition: 'enforced',
    rows: ['UIUX-T29-VSC', 'UIUX-T29-JB'],
    note: 'both hosts render the mandated label, copy and View details (VS outcomes/chat.js, JB OutcomesPanel); pinned by UIUX-T29-VSC/UIUX-T29-JB.',
  },
  {
    state: 'T30',
    spec: 'T30 [L1198-1199] "Billing is disabled locally" / "This surface has no active billing data." / "Back"',
    disposition: 'enforced',
    rows: ['UIUX-T30-VSC', 'UIUX-T30-JB'],
    note: 'renders T30 family as "billing disabled locally — <reason>" (VS cockpit usagePanelLines) and "Billing is disabled locally; no period numbers are served." (JB UsagePanel disabled card); the proposed supporting copy is not rendered; pinned by UIUX-T30-VSC/UIUX-T30-JB.',
  },
  {
    state: 'T31',
    spec: 'T31 [L1202-1204] "History unavailable" / "The conversation list could not be read." / "Refresh history"',
    disposition: 'enforced',
    rows: ['UIUX-T31-VSC', 'UIUX-T31-JB'],
    note: 'both hosts render the mandated label and copy and Refresh history (VS history-state.js, JB HistoryPanel); pinned by UIUX-T31-VSC/UIUX-T31-JB.',
  },
  {
    state: 'T32',
    spec: 'T32 [L1207-1209] "Shell execution needs a decision" / "The secure default remains fail-closed on this platform." / "Open setup guidance"',
    disposition: 'enforced',
    rows: ['UIUX-T32-VSC', 'UIUX-T32-JB'],
    note: 'both hosts render the mandated label and copy and Open setup guidance (VS preflight + webview, JB DiagnosticsPanel); pinned by UIUX-T32-VSC/UIUX-T32-JB.',
  },
  {
    state: 'T33',
    spec: 'T33 [L1212-1215] "Repository context is partial" / "The index is rebuilding or capped; no full coverage is implied." / "Inspect coverage"',
    disposition: 'enforced',
    rows: ['UIUX-T33-VSC', 'UIUX-T33-JB'],
    note: 'renders T33 family as "index: partial <n>/<m> files" (VS nativeClient indexCoverageLabel, JB shared NativeProtocol indexCoverageLabel); the proposed label/copy are not rendered; pinned by UIUX-T33-VSC/UIUX-T33-JB.',
  },
];

// Product directories the proposed-copy staleness scan reads (the host
// surfaces that render the state matrix).
const UIUX_PRODUCTION_DIRS = [
  'apps/vscode/src',
  'apps/vscode/media',
  'apps/jetbrains/frontend/src/main',
  'apps/jetbrains/backend/src',
];

// ---------------------------------------------------------------------------
// Generic filesystem helpers.
// ---------------------------------------------------------------------------

const SKIP_DIRS = new Set(['.git', 'node_modules', 'target', 'build', '.gradle', 'ui']);

function walkFiles(dir, filter, out = []) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name)) continue;
      walkFiles(path, filter, out);
    } else if (entry.isFile() && filter(path)) {
      out.push(path);
    }
  }
  return out;
}

function readIf(path) {
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

function rel(root, path) {
  return relative(root, path).split(sep).join('/');
}

function lineOf(text, index) {
  return text.slice(0, index).split('\n').length;
}

/** Strip // and block comments, preserving line count (line numbers remain true). */
function stripComments(text) {
  let out = '';
  let i = 0;
  while (i < text.length) {
    if (text[i] === '/' && text[i + 1] === '/') {
      while (i < text.length && text[i] !== '\n') i += 1;
      continue;
    }
    if (text[i] === '/' && text[i + 1] === '*') {
      i += 2;
      while (i < text.length && !(text[i] === '*' && text[i + 1] === '/')) {
        if (text[i] === '\n') out += '\n';
        i += 1;
      }
      i += 2;
      continue;
    }
    out += text[i];
    i += 1;
  }
  return out;
}

/** Kotlin string literals (comments excluded) with source line numbers. */
function kotlinStringLiterals(text) {
  const out = [];
  let line = 1;
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === '\n') {
      line += 1;
      i += 1;
      continue;
    }
    if (ch === '/' && text[i + 1] === '/') {
      while (i < text.length && text[i] !== '\n') i += 1;
      continue;
    }
    if (ch === '/' && text[i + 1] === '*') {
      i += 2;
      while (i < text.length && !(text[i] === '*' && text[i + 1] === '/')) {
        if (text[i] === '\n') line += 1;
        i += 1;
      }
      i += 2;
      continue;
    }
    if (ch === '"') {
      const startLine = line;
      i += 1;
      let literal = '';
      while (i < text.length && text[i] !== '"') {
        if (text[i] === '\\') {
          literal += text[i + 1] ?? '';
          i += 2;
          continue;
        }
        if (text[i] === '\n') line += 1;
        literal += text[i];
        i += 1;
      }
      i += 1;
      out.push({ line: startLine, literal });
      continue;
    }
    i += 1;
  }
  return out;
}

function kebab(title) {
  return title
    .replace(/([a-z0-9])([A-Z])/g, '$1-$2')
    .replace(/[^a-zA-Z0-9]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '')
    .toLowerCase();
}

function fixtureRoot() {
  return mkdtempSync(join(tmpdir(), 'faktor-dogfood-selftest-'));
}

function writeFixture(root, relPath, content) {
  const path = join(root, relPath);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, content);
}

// ---------------------------------------------------------------------------
// Surface checks. Each returns a list of gap strings for its surface class.
// ---------------------------------------------------------------------------

/** Keep only executable plan lines: a `#` comment can neither gate nor invoke. */
function executablePlanText(text) {
  return text
    .split('\n')
    .filter((line) => !/^\s*#/.test(line))
    .join('\n');
}

function planGateGaps(root, planText) {
  const gaps = [];
  if (planText === '') {
    gaps.push(`missing-plan: ${PLAN_FILE} is missing or empty; the dogfood run plan is the gate registry`);
    return gaps;
  }
  const executable = executablePlanText(planText);
  for (const [id, section, marker] of PLAN_GATES) {
    if (!executable.includes(marker)) {
      gaps.push(`missing-plan-gate: ${id} (${section}) — ${PLAN_FILE} never runs ${JSON.stringify(marker)}`);
    }
  }
  return gaps;
}

/**
 * CI lane coverage: every PLAN_GATES id must be executed by a real CI lane
 * command somewhere, or be reviewed in CI_LANE_LOCAL_ONLY with a reason.
 * A mapped id whose command substring is absent from every named lane file is
 * stale (the lane no longer runs the gate); an unmapped id without a reviewed
 * local-only reason is a gap; stale/contradictory table entries are gaps.
 * `options` (gates/map/localOnly) exists for the adversarial selftest and
 * defaults to the real tables this checker ships.
 */
export function ciLaneCoverageGaps(root, options = {}) {
  const gates = options.gates ?? PLAN_GATES;
  const map = options.map ?? CI_LANE_MAP;
  const localOnly = options.localOnly ?? CI_LANE_LOCAL_ONLY;
  const byId = new Map(map.map((entry) => [entry.id, entry]));
  const known = new Set(gates.map(([id]) => id));
  const texts = new Map();
  const textOf = (file) => {
    if (!texts.has(file)) texts.set(file, readIf(join(root, file)));
    return texts.get(file);
  };
  const gaps = [];
  for (const [id, section] of gates) {
    const entry = byId.get(id);
    const reason = localOnly.get(id);
    if (entry) {
      if (typeof reason === 'string') {
        gaps.push(`ci-lane-stale-local-only: ${id} is reviewed local-only in scripts/dogfood.mjs but also has a CI lane mapping; a gate that runs in a lane must not be allowlisted`);
      }
      const lanes = entry.lanes ?? [];
      const matched = lanes.some(([file, needle]) => textOf(file).includes(needle));
      if (!matched) {
        const named = lanes.map(([file, needle]) => `${file}: ${JSON.stringify(needle)}`).join(', ');
        gaps.push(`ci-lane-stale: ${id} maps to a lane command that no longer exists (${named}); re-point it at the real lane command or review it local-only`);
      }
      continue;
    }
    if (typeof reason === 'string' && reason.trim().length >= 30) continue;
    gaps.push(`ci-lane-gap: ${id} (${section}) is not mapped to any CI lane command; map it or add a reviewed reason`);
  }
  for (const entry of map) {
    if (!known.has(entry.id)) {
      gaps.push(`ci-lane-stale-map: ${entry.id} is mapped in scripts/dogfood.mjs but is not a PLAN_GATES id; re-review CI_LANE_MAP`);
    }
  }
  for (const id of localOnly.keys()) {
    if (!known.has(id)) {
      gaps.push(`ci-lane-stale-local-only: ${id} is reviewed local-only in scripts/dogfood.mjs but is not a PLAN_GATES id; re-review CI_LANE_LOCAL_ONLY`);
    }
  }
  return gaps;
}

function crateDirMap(root) {
  const map = new Map();
  for (const file of walkFiles(join(root, 'crates'), (path) => path.endsWith('Cargo.toml'))) {
    const name = /^\s*name\s*=\s*"([^"]+)"/m.exec(readIf(file));
    if (name) map.set(name[1], dirname(file));
  }
  for (const file of walkFiles(join(root, 'tests'), (path) => path.endsWith('Cargo.toml'))) {
    const name = /^\s*name\s*=\s*"([^"]+)"/m.exec(readIf(file));
    if (name) map.set(name[1], dirname(file));
  }
  return map;
}

function crateGaps(root, crates) {
  const gaps = [];
  const dirs = crateDirMap(root);
  for (const crate of crates) {
    const dir = dirs.get(crate);
    if (dir === undefined) {
      gaps.push(`missing-crate-dir: ${crate} is in the production snapshot but no Cargo.toml names it under crates/ or tests/`);
      continue;
    }
    const sources = walkFiles(dir, (path) => path.endsWith('.rs'));
    const hasTests = sources.some((file) => /#\[(?:tokio::|async_std::)?test\]/.test(readIf(file)));
    if (!hasTests) {
      gaps.push(`crate-without-unit-test: ${crate} has no #[test]/#[tokio::test] site under ${rel(root, dir)}`);
    }
  }
  return gaps;
}

/**
 * Rebuild the capability world with check-invariant-coverage.mjs's own model
 * (parse + validate + discovery equality) so dogfood cannot disagree with the
 * certifying checker.
 */
function capabilityGraph(root) {
  const snapshotRaw = readIf(join(root, PRODUCTION_CRATES_FILE));
  const coverageRaw = readIf(join(root, COVERAGE_FILE));
  let crates = [];
  let coverage;
  try {
    crates = snapshotRaw === '' ? [] : (JSON.parse(snapshotRaw).crates ?? []);
    if (coverageRaw === '') {
      return { problems: [`missing-manifest: ${COVERAGE_FILE} not found`], discovered: { ids: [] }, crates };
    }
    coverage = JSON.parse(coverageRaw);
  } catch (error) {
    return {
      problems: [`unreadable-manifest: ${error.message}`],
      discovered: { ids: [] },
      crates,
    };
  }
  let programmeRows = new Map();
  try {
    const programme = JSON.parse(readIf(join(root, 'tests/programme/registry.json')));
    programmeRows = new Map((programme.rows ?? []).map((row) => [row.id, row]));
  } catch {
    programmeRows = new Map();
  }
  const world = {
    crates,
    invariants: parseInvariants(readIf(join(root, REGISTRY_FILE))),
    coverage,
    discovered: discover(root),
    specs: loadMutationSpecs(join(root, 'scripts/mutations')),
    programmeRows,
    root,
  };
  return { problems: coverageProblems(world), discovered: world.discovered, crates };
}

function vscodeGaps(root, corpus) {
  const pkgPath = join(root, 'apps/vscode/package.json');
  if (!existsSync(pkgPath)) return ['missing-vscode: apps/vscode/package.json not found'];
  const gaps = [];
  let pkg;
  try {
    pkg = JSON.parse(readIf(pkgPath));
  } catch (error) {
    return [`bad-vscode-manifest: apps/vscode/package.json does not parse (${error.message})`];
  }
  const contributes = pkg.contributes ?? {};
  const grep = (needle) => corpus.some((text) => text.includes(needle));
  const discovered = new Set(discover(root).ids);
  const requireManifest = (id) => {
    const normalized = normalizeCapabilityId(id);
    if (!discovered.has(normalized)) {
      gaps.push(`vscode-without-manifest-row: ${id} (${normalized}) is contributed but has no capability/discovered row`);
    }
  };

  for (const command of contributes.commands ?? []) {
    if (typeof command?.command !== 'string') continue;
    requireManifest(`vscode.command.${command.command}`);
    if (!grep(command.command)) {
      gaps.push(
        `vscode-command-without-test: ${command.command} is contributed but is referenced by no VS Code test corpus file (${VSCODE_TEST_CORPUS.join(', ')})`,
      );
    }
  }
  for (const [container, views] of Object.entries(contributes.views ?? {})) {
    requireManifest(`vscode.view.${container}`);
    for (const view of views ?? []) {
      if (typeof view?.id === 'string') requireManifest(`vscode.view.${view.id}`);
    }
  }
  const configuration = contributes.configuration ?? {};
  const sections = Array.isArray(configuration) ? configuration : [configuration];
  for (const section of sections) {
    if (typeof section?.title === 'string' && section.title.trim()) {
      requireManifest(`vscode.configuration.${section.title.trim()}`);
    }
    for (const key of Object.keys(section?.properties ?? {})) {
      requireManifest(`vscode.configuration.${key}`);
      if (!grep(key)) {
        gaps.push(`vscode-configuration-without-test: ${key} is contributed but is referenced by no VS Code test corpus file`);
      }
    }
  }
  for (const menu of Object.keys(contributes.menus ?? {})) {
    requireManifest(`vscode.menu.${menu}`);
  }
  const commandIds = new Set((contributes.commands ?? []).map((entry) => entry.command));
  for (const walkthrough of contributes.walkthroughs ?? []) {
    if (typeof walkthrough?.id === 'string' && !grep(walkthrough.id)) {
      gaps.push(`vscode-walkthrough-without-test: ${walkthrough.id} is contributed but ${VSCODE_SELFTEST} never pins it`);
    }
    const steps = walkthrough?.steps ?? [];
    if (steps.length > 0 && !grep('steps.length')) {
      gaps.push(`vscode-walkthrough-steps-unpinned: ${walkthrough.id} has ${steps.length} step(s) but ${VSCODE_SELFTEST} does not pin the step count`);
    }
    for (const step of steps) {
      for (const event of step.completionEvents ?? []) {
        const command = /^onCommand:(.+)$/.exec(event);
        if (command && !commandIds.has(command[1])) {
          gaps.push(`vscode-walkthrough-event-uncontributed: step ${step.id ?? '?'} of ${walkthrough.id} completes on ${command[1]}, which is not a contributed command`);
        }
      }
    }
  }
  return gaps;
}

function parsePanelClasses(root) {
  const main = join(root, 'apps/jetbrains/frontend/src/main/kotlin');
  const classes = [];
  for (const file of walkFiles(main, (path) => /Panel\.kt$/.test(path))) {
    readIf(file)
      .split('\n')
      .forEach((line, index) => {
        if (/^\s*(?:private|protected)\s/.test(line)) return;
        const match = /^\s*(?:(?:public|internal|open|data|abstract|sealed|final)\s+)*class\s+([A-Za-z0-9_]+Panel)\b/.exec(line);
        if (match) classes.push({ name: match[1], file: rel(root, file), line: index + 1 });
      });
  }
  return classes;
}

function hostMatrixPanels(root) {
  const file = join(root, HOST_MATRIX_FILE);
  const text = readIf(file);
  const block = /private val panelFactories:[\s\S]*?listOf\(([\s\S]*?)\n\s*\)/.exec(text);
  if (!block) return null;
  return [...block[1].matchAll(/"([a-z0-9-]+)"\s+to\s*\{/g)].map((match) => match[1]);
}

function chatPanelLeafTabs(root) {
  const file = join(root, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt');
  const titles = [];
  for (const match of readIf(file).matchAll(/(workTabs|inspectTabs|historyTabs)\.addTab\(\s*"([^"]+)"/g)) {
    titles.push(match[2]);
  }
  return titles;
}

function panelGaps(root, options = {}) {
  const gaps = [];
  const componentClaims = options.componentClaims ?? new Map();
  const classes = parsePanelClasses(root);
  const matrix = hostMatrixPanels(root);
  if (matrix === null) return ['missing-host-matrix: JetBrainsHostMatrixSmoke.kt has no panelFactories list'];
  const matrixSet = new Set(matrix);
  const claimed = new Set();
  for (const panel of classes) {
    if (panel.name === 'FaktorChatPanel') continue; // the host container: checked via its leaf tabs below
    const alias = PANEL_HOST_ALIASES.get(panel.name);
    const host = alias ? alias.host : kebab(panel.name.replace(/Panel$/, ''));
    claimed.add(host);
    if (!matrixSet.has(host)) {
      gaps.push(`panel-without-host-matrix-entry: ${panel.name} (${panel.file}:${panel.line}) maps to "${host}", which is absent from the JetBrainsHostMatrixSmoke panelFactories list; render and screenshot it before it can ship`);
    }
  }
  for (const tab of chatPanelLeafTabs(root)) {
    const host = kebab(tab);
    claimed.add(host);
    if (!matrixSet.has(host)) {
      gaps.push(`chat-tab-without-host-matrix-entry: FaktorChatPanel leaf tab "${tab}" maps to "${host}", which is absent from the JetBrainsHostMatrixSmoke panelFactories list`);
    }
  }
  for (const entry of matrix) {
    if (!claimed.has(entry) && !componentClaims.has(entry)) {
      gaps.push(`stale-host-matrix-entry: the host matrix renders "${entry}" but no panel class or chat tab claims it`);
    }
  }
  for (const entry of componentClaims.keys()) {
    if (!matrixSet.has(entry)) {
      gaps.push(`stale-host-matrix-claim: "${entry}" is reviewed as a container-surface host-matrix claim in scripts/dogfood.mjs but the matrix no longer renders it`);
    }
  }
  return gaps;
}

export function scriptFiles(root) {
  return walkFiles(join(root, 'scripts'), (path) => {
    if (/\.(sh|mjs|js|cjs|ps1)$/.test(path)) return true;
    // Extensionless executable scripts (e.g. scripts/generate-iana-network-table)
    // are part of the surface too: a shebang'd file must not hide behind its
    // missing extension.
    const base = path.split('/').pop();
    if (base.includes('.')) return false;
    try {
      return readIf(path).startsWith('#!');
    } catch {
      return false;
    }
  });
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * Planned scripts are text-referenced (the executor prints every command)
 * plus a transitive closure over references and relative imports, so a helper
 * invoked only by an invoked helper stays gated.
 */
export function plannedScripts(root, planText) {
  const executable = executablePlanText(planText);
  const all = scriptFiles(root).map((path) => rel(root, path));
  const byPath = new Set(all);
  const reachable = new Map(); // rel -> invoker rel ('' means the dogfood plan)
  const queue = [];
  for (const script of all) {
    if (executable.includes(script)) {
      reachable.set(script, '');
      queue.push(script);
    }
  }
  while (queue.length > 0) {
    const current = queue.shift();
    // The dogfood checker itself documents every allowlist entry; its prose
    // must not count as an invocation of the scripts it classifies.
    if (current === 'scripts/dogfood.mjs') continue;
    const file = join(root, current);
    const text = readIf(file);
    for (const candidate of all) {
      if (reachable.has(candidate)) continue;
      const base = candidate.split('/').pop();
      const referenced =
        text.includes(candidate) ||
        new RegExp(`(^|[^A-Za-z0-9_.-])${escapeRegExp(base)}(?![A-Za-z0-9_-])`).test(text);
      if (referenced) {
        reachable.set(candidate, current);
        queue.push(candidate);
      }
    }
    if (/\.(mjs|js|cjs)$/.test(current)) {
      for (const match of text.matchAll(/(?:from|import)\s*\(?\s*['"](\.[^'"]+)['"]/g)) {
        let resolved = resolve(dirname(file), match[1]);
        if (!/\.[a-z]+$/.test(resolved)) resolved += '.mjs';
        const candidate = rel(root, resolved);
        if (byPath.has(candidate) && !reachable.has(candidate)) {
          reachable.set(candidate, current);
          queue.push(candidate);
        }
      }
    }
  }
  return reachable;
}

function scriptGaps(root, planText, allowlist = SCRIPT_ALLOWLIST) {
  const gaps = [];
  const scripts = scriptFiles(root).map((path) => rel(root, path)).sort();
  const executable = executablePlanText(planText);
  const reachable = plannedScripts(root, planText);
  const present = new Set(scripts);
  for (const script of scripts) {
    if (executable.includes(script)) continue; // invoked by the dogfood run plan
    if (typeof allowlist.get(script) === 'string' && allowlist.get(script).trim().length >= 30) {
      continue; // reviewed allowlist exception (reported by --coverage)
    }
    if (reachable.has(script)) continue; // invoked by a planned script
    gaps.push(`ungated-script: ${script} is neither invoked by ${PLAN_FILE} (directly or transitively), nor self-tested by the plan, nor allowlisted with a reason in scripts/dogfood.mjs`);
  }
  for (const entry of allowlist.keys()) {
    if (!present.has(entry)) {
      gaps.push(`stale-allowlist-entry: ${entry} is allowlisted in scripts/dogfood.mjs but no longer exists`);
    }
  }
  return gaps;
}

/** docs/*.md not linked from README.md or another docs/*.md. */
function unreferencedDocs(root) {
  const docsDir = join(root, 'docs');
  const docs = existsSync(docsDir) ? readdirSync(docsDir).filter((name) => name.endsWith('.md')).sort() : [];
  const indexText = readIf(join(root, 'README.md'));
  const others = docs.map((name) => readIf(join(docsDir, name)));
  return docs.filter(
    (name, index) => !(indexText.includes(name) || others.some((text, other) => other !== index && text.includes(name))),
  );
}

function docsGaps(root, allowlist = DOCS_ALLOWLIST) {
  const gaps = [];
  const docs = existsSync(join(root, 'docs')) ? readdirSync(join(root, 'docs')).filter((name) => name.endsWith('.md')).sort() : [];
  for (const name of unreferencedDocs(root)) {
    const reason = allowlist.get(`docs/${name}`);
    if (typeof reason === 'string' && reason.trim().length >= 30) continue;
    gaps.push(`unreferenced-doc: docs/${name} is linked from neither README.md nor another docs/*.md, and has no allowlist reason in scripts/dogfood.mjs`);
  }
  const present = new Set(docs.map((name) => `docs/${name}`));
  for (const entry of allowlist.keys()) {
    if (!present.has(entry)) {
      gaps.push(`stale-allowlist-entry: ${entry} is allowlisted in scripts/dogfood.mjs but no longer exists`);
    }
  }
  return gaps;
}

function workflowFiles(root) {
  return walkFiles(join(root, '.woodpecker'), (path) => path.endsWith('.yaml'))
    .map((path) => rel(root, path))
    .sort();
}

/**
 * Test-support crates under tests/ (direct children only: `tests/<name>/
 * Cargo.toml`; fixture corpora nested deeper are corpus inputs, not
 * crates). They must be workspace members so `cargo test --workspace` (§1)
 * compiles and runs them. A crate that is not a member is a gap unless the
 * workspace-membership decision is explicitly reviewed here WITH a reason; an
 * exception that is no longer needed (the crate became a member) is stale.
 */
const TEST_CRATE_ALLOWLIST = new Map();

function testCrateSurface(root, allowlist = TEST_CRATE_ALLOWLIST) {
  const names = [];
  const gaps = [];
  const exceptions = [];
  const workspace = readIf(join(root, 'Cargo.toml'));
  const seen = new Set();
  const needed = new Set();
  for (const file of walkFiles(join(root, 'tests'), (path) => path.endsWith('Cargo.toml'))) {
    const dir = rel(root, dirname(file));
    if (dir.split('/').length !== 2) continue; // tests/<name> only
    const name = /^\s*name\s*=\s*"([^"]+)"/m.exec(readIf(file));
    if (!name) continue;
    names.push(name[1]);
    seen.add(dir);
    if (workspace.includes(`"${dir}"`)) continue;
    needed.add(dir);
    const reason = allowlist.get(dir);
    if (typeof reason === 'string' && reason.trim().length >= 30) {
      exceptions.push(`${dir}: ${reason}`);
      continue;
    }
    gaps.push(`test-crate-outside-workspace: ${name[1]} (${dir}) is not a workspace member, so §1 cargo test --workspace never runs it`);
  }
  for (const entry of allowlist.keys()) {
    if (!seen.has(entry)) {
      gaps.push(`stale-allowlist-entry: ${entry} is allowlisted in scripts/dogfood.mjs but is no longer a tests/ crate`);
    } else if (!needed.has(entry)) {
      gaps.push(`stale-allowlist-entry: ${entry} is allowlisted in scripts/dogfood.mjs but is now a workspace member (drop the exception)`);
    }
  }
  return { names, gaps, exceptions };
}

// ---------------------------------------------------------------------------
// UI/UX rules.
// ---------------------------------------------------------------------------

function colorViolations(root) {
  const violations = [];
  const main = join(root, 'apps/jetbrains/frontend/src/main/kotlin');
  for (const file of walkFiles(main, (path) => path.endsWith('.kt'))) {
    const relPath = rel(root, file);
    if (relPath.endsWith('/PixelAgents.kt')) continue;
    const text = stripComments(readIf(file));
    const isPanelSupport = relPath.endsWith('/PanelSupport.kt');
    for (const match of text.matchAll(/(?<![A-Za-z0-9_$.])(?:java\.awt\.)?Color\s*\(/g)) {
      if (!isPanelSupport) {
        violations.push(`raw-color: ${relPath}:${lineOf(text, match.index)} constructs a raw java.awt Color outside PixelAgents.kt; use a PanelSupport token/fallback`);
        continue;
      }
      if (!nearTokenFunction(text, match.index)) {
        violations.push(`raw-color: ${relPath}:${lineOf(text, match.index)} constructs a raw java.awt Color outside the reviewed token/fallback functions (nearest function: ${enclosingFunction(text, match.index) ?? 'none'})`);
      }
    }
  }
  return violations;
}

/** Any token/fallback function declared near `index` owns the constructor. */
function nearTokenFunction(text, index) {
  const start = Math.max(0, index - 4000);
  for (const match of text.slice(start, index).matchAll(/\bfun\s+([A-Za-z0-9_]+)\s*\(/g)) {
    if (PANEL_SUPPORT_COLOR_FUNCTIONS.has(match[1])) return true;
  }
  return false;
}

function enclosingFunction(text, index) {
  const matches = [...text.slice(0, index).matchAll(/\bfun\s+([A-Za-z0-9_]+)\s*\(/g)];
  return matches.length === 0 ? null : matches[matches.length - 1][1];
}

function terminalArgvViolations(root) {
  const violations = [];
  const file = join(root, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TerminalPanel.kt');
  if (!existsSync(file)) return violations;
  const text = stripComments(readIf(file));
  for (const match of text.matchAll(/\.split\(\s*(['"])\s+\1\s*\)/g)) {
    violations.push(`terminal-argv: ${rel(root, file)}:${lineOf(text, match.index)} splits terminal argv on a single space; run a shell or use an explicit argv model`);
  }
  return violations;
}

function cssHeaderViolations(root) {
  const violations = [];
  const file = join(root, 'apps/vscode/media/chat.css');
  if (!existsSync(file)) return violations;
  const text = readIf(file);
  for (const match of text.matchAll(/([^{}]+)\{([^}]*)\}/g)) {
    const selector = match[1].trim();
    if (!/\.card\b[^{,]*\bh[1-6]\b/.test(selector)) continue;
    if (/text-transform\s*:\s*uppercase/.test(match[2])) {
      violations.push(`uppercase-header: ${rel(root, file)}:${lineOf(text, match.index)} "${selector.replace(/\s+/g, ' ')}" sets text-transform: uppercase; card section headers stay calm`);
    }
  }
  return violations;
}

const INTERNAL_LABEL_TOKENS = [
  ['session_id', /session_id/],
  ['worktree', /worktree/],
  ['snapshot_id', /snapshot_id/],
  ['capability=', /capability=/],
  ['#<id>', /#(?:\$\{|\d)/],
];

function internalLabelViolations(root, allowlist = INTERNAL_LABEL_ALLOWLIST) {
  const violations = [];
  const main = join(root, 'apps/jetbrains/frontend/src/main/kotlin');
  const observed = new Map();
  for (const file of walkFiles(main, (path) => path.endsWith('.kt'))) {
    const relPath = rel(root, file);
    for (const { line, literal } of kotlinStringLiterals(readIf(file))) {
      for (const [token, regex] of INTERNAL_LABEL_TOKENS) {
        if (!regex.test(literal)) continue;
        const key = `${relPath}|${token}`;
        if (!observed.has(key)) observed.set(key, []);
        observed.get(key).push({ line, literal: literal.slice(0, 80) });
      }
    }
  }
  const reviewed = new Map(allowlist.map((entry) => [`${entry.file}|${entry.token}`, entry]));
  for (const [key, hits] of observed) {
    const entry = reviewed.get(key);
    const [file, token] = key.split('|');
    if (!entry) {
      for (const hit of hits) {
        violations.push(`internal-identifier: ${file}:${hit.line} visible string exposes ${token} (${JSON.stringify(hit.literal)}); review it and either re-author the label or add an INTERNAL_LABEL_ALLOWLIST entry with a reason`);
      }
      continue;
    }
    if (entry.count !== hits.length) {
      for (const hit of hits) {
        violations.push(`internal-identifier-unreviewed: ${file}:${hit.line} exposes ${token} but the reviewed allowlist expects ${entry.count} occurrence(s), observed ${hits.length}; re-review`);
      }
    }
  }
  for (const entry of allowlist) {
    if (!observed.has(`${entry.file}|${entry.token}`)) {
      violations.push(`stale-label-allowlist-entry: ${entry.file} ${entry.token} is allowlisted in scripts/dogfood.mjs but no longer observed`);
    }
  }
  return violations;
}

function uiViolations(root, allowlist = INTERNAL_LABEL_ALLOWLIST) {
  return [
    ...colorViolations(root),
    ...terminalArgvViolations(root),
    ...cssHeaderViolations(root),
    ...internalLabelViolations(root, allowlist),
  ];
}

// ---------------------------------------------------------------------------
// Visual identity rules (contract-vi.md; audit-gates.md holes 4/5/18).
// ---------------------------------------------------------------------------

/** All `d` path-data strings inside an SVG text. */
function svgPathData(text) {
  return [...text.matchAll(/\bd\s*=\s*"([^"]*)"/g)].map((match) => match[1]);
}

/** Every command letter used by the SVG's path data (unknown letters kept). */
function pathCommandLetters(text) {
  const letters = new Set();
  for (const d of svgPathData(text)) {
    for (const match of d.matchAll(/[A-Za-z]/g)) letters.add(match[0]);
  }
  return letters;
}

/** Forbidden mark elements present in the SVG text (MK-1.03/MK-1.04). */
function forbiddenMarkElements(text) {
  const hits = [];
  for (const name of FORBIDDEN_MARK_ELEMENTS) {
    if (new RegExp(`<${name}\\b`, 'i').test(text)) hits.push(name);
  }
  return hits;
}

function normalizePathData(d) {
  return d.replace(/\s+/g, ' ').trim();
}

/** Absolute M/L/Z polyline parser: vertices, or null for any other command. */
function absolutePolyline(d) {
  const segments = d.match(/[A-Za-z][^A-Za-z]*/g);
  if (segments === null) return null;
  const points = [];
  let closed = false;
  for (const segment of segments) {
    const command = segment[0];
    const rest = segment.slice(1).trim();
    const numbers = rest === '' ? [] : rest.split(/[\s,]+/).map(Number);
    if (numbers.some((value) => !Number.isFinite(value))) return null;
    if (command === 'M' || command === 'L') {
      if (numbers.length !== 2) return null;
      points.push([numbers[0], numbers[1]]);
    } else if (command === 'Z') {
      if (numbers.length !== 0) return null;
      closed = true;
    } else {
      return null;
    }
  }
  return closed && points.length > 0 ? points : null;
}

function pointsNear(a, b) {
  return Math.abs(a[0] - b[0]) <= 0.01 && Math.abs(a[1] - b[1]) <= 0.01;
}

/**
 * True when a path is the canonical Cut F contour: either shipped spelling
 * verbatim (whitespace-insensitive) or an axis-aligned uniform-scale copy, as
 * baked into the stacked lockup masters (scale ~0.4056 at review time). A
 * distorted or re-drawn mark stays a gap.
 */
function cutFPathMatches(d) {
  const normalized = normalizePathData(d);
  if (normalized === CUT_F_PATH || normalized === CUT_F_PATH_CLOSED) return true;
  const vertices = absolutePolyline(d);
  if (vertices === null) return false;
  if (vertices.length === CUT_F_POINTS.length + 1 && pointsNear(vertices[0], vertices[vertices.length - 1])) {
    vertices.pop();
  }
  if (vertices.length !== CUT_F_POINTS.length) return false;
  const xs = vertices.map(([x]) => x);
  const ys = vertices.map(([, y]) => y);
  const minX = Math.min(...xs);
  const maxX = Math.max(...xs);
  const minY = Math.min(...ys);
  const maxY = Math.max(...ys);
  const scaleX = (maxX - minX) / 50;
  const scaleY = (maxY - minY) / 52;
  if (!(scaleX > 0) || !(scaleY > 0)) return false;
  // Uniform scale modulo the shipped 3-decimal rasterisation (0.02% on the
  // stacked masters); a genuinely distorted mark must stay a gap.
  if (Math.abs(scaleX - scaleY) > 0.01 * Math.max(scaleX, scaleY)) return false;
  const tolerance = Math.max(0.01, 0.002 * Math.max(maxX - minX, maxY - minY));
  for (let i = 0; i < vertices.length; i += 1) {
    const [x, y] = CUT_F_POINTS[i];
    if (Math.abs(vertices[i][0] - (minX + x * scaleX)) > tolerance) return false;
    if (Math.abs(vertices[i][1] - (minY + y * scaleY)) > tolerance) return false;
  }
  return true;
}

/** True when any path in the SVG is the canonical Cut F contour. */
function containsCutF(text) {
  return svgPathData(text).some((d) => cutFPathMatches(d));
}

/** PNG signature + IHDR dimension pins (first 33 bytes; pure Buffer math). */
function pngIconGaps(root, relPath, size) {
  let bytes;
  try {
    bytes = readFileSync(join(root, relPath));
  } catch {
    return [`vscode-icon-png: ${relPath} is missing; the release icon must ship at ${size}x${size} (audit-gates.md hole 18)`];
  }
  if (bytes.length < 33) {
    return [`vscode-icon-png: ${relPath} is ${bytes.length} bytes, too short for a PNG signature + IHDR (the first 33 bytes must parse)`];
  }
  const header = bytes.subarray(0, 33);
  const signature = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (!signature.every((value, index) => header[index] === value)) {
    return [`vscode-icon-png: ${relPath} does not start with the PNG signature; the release icon must be a real PNG`];
  }
  if (header.subarray(12, 16).toString('ascii') !== 'IHDR') {
    return [`vscode-icon-png: ${relPath} first chunk is not IHDR; cannot verify the release icon dimensions`];
  }
  const width = header.readUInt32BE(16);
  const height = header.readUInt32BE(20);
  if (width !== size || height !== size) {
    return [`vscode-icon-png: ${relPath} IHDR is ${width}x${height}, expected ${size}x${size} (release icon pin; audit-gates.md hole 18)`];
  }
  return [];
}

/**
 * Visual identity contract gaps. Every gap names the file and the rule it
 * breaks; artwork/copy is normative, so the fix is re-review or re-export,
 * never editing the product files to fit the checker.
 */
function visualIdentityGaps(root) {
  const gaps = [];

  // (a) Host icons: the same bare angular Cut F, straight edges only.
  for (const file of HOST_ICON_FILES) {
    const text = readIf(join(root, file));
    if (text === '') {
      gaps.push(`host-icon-missing: ${file} is missing (HO-6.01 rollout target)`);
      continue;
    }
    if (!containsCutF(text)) {
      gaps.push(`host-icon-path: ${file} does not carry the canonical Cut F path (MK-1.02 / Appendix B mark pin; keep the shipped path exactly)`);
    }
    const elements = forbiddenMarkElements(text);
    if (elements.length > 0) {
      gaps.push(`host-icon-element: ${file} contains <${elements[0]}>; the mark is straight-edge only, no circle/ellipse/text/image (MK-1.03/MK-1.04/PR-7.33)`);
    }
    const curveCommands = [...pathCommandLetters(text)].filter((letter) => !STRAIGHT_PATH_COMMANDS.has(letter.toUpperCase()));
    if (curveCommands.length > 0) {
      gaps.push(`host-icon-command: ${file} path data uses ${curveCommands.join(', ')}; Cut F allows only M, V, H, L, Z (MK-1.02 / Appendix B allowed_path_commands)`);
    }
  }
  const vscodeHost = readIf(join(root, HOST_ICON_FILES[0]));
  if (vscodeHost !== '' && !vscodeHost.includes('currentColor')) {
    gaps.push(`host-icon-currentColor: ${HOST_ICON_FILES[0]} no longer uses currentColor (HO-6.01: preserve currentColor/theme semantics)`);
  }
  const jetbrainsHost = readIf(join(root, HOST_ICON_FILES[1]));
  if (jetbrainsHost !== '') {
    const hexes = new Set([...jetbrainsHost.matchAll(/#[0-9a-fA-F]{6}\b/g)].map((match) => match[0].toLowerCase()));
    if (hexes.size !== 1) {
      gaps.push(`host-icon-monochrome: ${HOST_ICON_FILES[1]} carries ${hexes.size} distinct #rrggbb tokens (${[...hexes].join(', ') || 'none'}); the JetBrains host slot must stay exactly one monochrome token (HO-6.01 host constraints; audit-gates.md hole 5)`);
    }
  }

  // (b) docs/assets/brand kit: files, straight artwork, symbol presence.
  const brandDir = 'docs/assets/brand';
  for (const [name, kind] of BRAND_KIT) {
    const relPath = `${brandDir}/${name}`;
    const text = readIf(join(root, relPath));
    if (text === '') {
      gaps.push(`brand-kit-missing: ${relPath} is missing from the shipped 16-file kit (AS-2.02/AS-2.03)`);
      continue;
    }
    if (kind === 'symbol' || kind === 'tile') {
      const curveCommands = [...pathCommandLetters(text)].filter((letter) => !STRAIGHT_PATH_COMMANDS.has(letter.toUpperCase()));
      if (curveCommands.length > 0) {
        gaps.push(`brand-kit-straight: ${relPath} path data uses ${curveCommands.join(', ')}; symbol and app-tile artwork must stay straight-edge (MK-1.02: M/V/H/L/Z only)`);
      }
    }
    if (kind === 'symbol' || kind === 'lockup' || kind === 'stacked' || kind === 'tile') {
      if (!containsCutF(text)) {
        gaps.push(`brand-kit-symbol: ${relPath} no longer carries the canonical Cut F contour (MK-1.02; lockups embed the master symbol verbatim or at a uniform scale)`);
      }
    }
    if (kind === 'tile') {
      for (const [fill, role] of [
        ['#08110C', 'tile background (Ink)'],
        ['#D9BA75', 'Cut F symbol (Ochre)'],
      ]) {
        if (!new RegExp(`fill\\s*=\\s*"${fill}"`, 'i').test(text)) {
          gaps.push(`brand-kit-tile-fill: ${relPath} does not fill the ${role} with ${fill} (PL-3.01 shipped app tile)`);
        }
      }
    }
  }
  const brandEntries = existsSync(join(root, brandDir)) ? readdirSync(join(root, brandDir)) : [];
  for (const name of brandEntries) {
    if (!BRAND_KIT.has(name)) {
      gaps.push(`brand-kit-unreviewed: ${brandDir}/${name} is not in the reviewed 16-file kit; review the asset and extend BRAND_KIT in scripts/dogfood.mjs`);
    }
  }

  // (b) tokens: parse + exact palette, state colors and mark numbers.
  const tokensText = readIf(join(root, `${brandDir}/faktor-tokens.json`));
  if (tokensText !== '') {
    let tokens = null;
    try {
      tokens = JSON.parse(tokensText);
    } catch (error) {
      gaps.push(`brand-tokens-parse: ${brandDir}/faktor-tokens.json does not parse (${error.message}); it is the canonical token record`);
    }
    if (tokens !== null) {
      const palette = tokens.palette ?? {};
      for (const [key, label, hex] of BRAND_PALETTE) {
        const value = palette[key];
        if (typeof value !== 'string' || value.toUpperCase() !== hex) {
          gaps.push(`brand-tokens-palette: ${brandDir}/faktor-tokens.json ${label} is ${JSON.stringify(value ?? null)}, expected ${hex} (PL-3.01 / Appendix B palette)`);
        }
      }
      const state = tokens.state ?? {};
      for (const [key, label, light, dark] of BRAND_STATE_COLORS) {
        const family = state[key] ?? {};
        if (family.light?.toUpperCase?.() !== light || family.dark?.toUpperCase?.() !== dark) {
          gaps.push(`brand-tokens-state: ${brandDir}/faktor-tokens.json ${label} is ${JSON.stringify(family.light ?? null)}/${JSON.stringify(family.dark ?? null)}, expected ${light}/${dark} (ST-5.02 state color families)`);
        }
      }
      const unit = tokens.mark?.geometry?.unit;
      const numbers = typeof unit === 'string' ? [...unit.matchAll(/\d+/g)].map((match) => match[0]) : [];
      for (const [pin, what] of CUT_F_UNITS) {
        if (!numbers.includes(pin)) {
          gaps.push(`brand-tokens-mark: ${brandDir}/faktor-tokens.json mark geometry does not pin the ${what} at ${pin} units (MK-1.02 / Appendix B: stem 16, arms 14, tips 6)`);
        }
      }
    }
  }

  // (c) Release metadata icons (audit-gates.md hole 5).
  const vscodePkg = 'apps/vscode/package.json';
  const pkgText = readIf(join(root, vscodePkg));
  let pkg = null;
  if (pkgText === '') {
    gaps.push(`vscode-release-icon: ${vscodePkg} is missing; cannot verify the release icon pins`);
  } else {
    try {
      pkg = JSON.parse(pkgText);
    } catch (error) {
      gaps.push(`vscode-release-icon: ${vscodePkg} does not parse (${error.message})`);
    }
  }
  if (pkg !== null) {
    if (pkg.icon !== 'media/faktor-icon.png') {
      gaps.push(`vscode-release-icon: ${vscodePkg} icon is ${JSON.stringify(pkg.icon ?? null)}, expected "media/faktor-icon.png" (release metadata master; HO-6.01 release row)`);
    } else if (!existsSync(join(root, 'apps/vscode/media/faktor-icon.png'))) {
      gaps.push(`vscode-release-icon: apps/vscode/media/faktor-icon.png is declared by ${vscodePkg} but does not exist`);
    }
    const activityIcons = (pkg.contributes?.viewsContainers?.activitybar ?? []).map((container) => container?.icon);
    if (!activityIcons.includes('media/faktor.svg')) {
      gaps.push(`vscode-activitybar-icon: ${vscodePkg} activitybar container icon is ${JSON.stringify(activityIcons)}, expected "media/faktor.svg" (HO-6.01: bare Cut F in the host slot)`);
    }
  }
  const pluginMeta = 'apps/jetbrains/frontend/src/main/resources/META-INF';
  for (const icon of ['pluginIcon.svg', 'pluginIcon_dark.svg']) {
    const relPath = `${pluginMeta}/${icon}`;
    const text = readIf(join(root, relPath));
    if (text === '') {
      gaps.push(`plugin-icon-missing: ${relPath} is missing; the JetBrains plugin icon must exist (audit-gates.md hole 5)`);
      continue;
    }
    for (const [fill, role] of [
      ['#08110C', 'app-tile background (Ink)'],
      ['#D9BA75', 'Cut F symbol (Ochre)'],
    ]) {
      if (!new RegExp(`fill\\s*=\\s*"${fill}"`, 'i').test(text)) {
        gaps.push(`plugin-icon-fill: ${relPath} does not carry the ${role} ${fill} (shipped app-tile treatment)`);
      }
    }
  }
  const pluginXml = readIf(join(root, `${pluginMeta}/plugin.xml`));
  if (pluginXml === '') {
    gaps.push(`plugin-xml-icon: ${pluginMeta}/plugin.xml is missing`);
  } else if (!/icon\s*=\s*"\/META-INF\/faktor\.svg"/.test(pluginXml)) {
    gaps.push(`plugin-xml-icon: ${pluginMeta}/plugin.xml no longer registers icon="/META-INF/faktor.svg" (HO-6.01 JetBrains rollout target)`);
  }

  // (d) Release PNG: signature + IHDR dimensions.
  gaps.push(...pngIconGaps(root, 'apps/vscode/media/faktor-icon.png', 128));

  // (e) Docs headers reference the shipped lockups (HO-6.01 docs row).
  const docLockups = new Map(
    Object.entries({
      'README.md': ['docs/assets/brand/faktor-lockup-ink.svg', 'docs/assets/brand/faktor-lockup-accent.svg'],
      'apps/README.md': ['../docs/assets/brand/faktor-lockup-ink.svg', '../docs/assets/brand/faktor-lockup-accent.svg'],
      'apps/jetbrains/README.md': ['../../docs/assets/brand/faktor-lockup-ink.svg', '../../docs/assets/brand/faktor-lockup-accent.svg'],
    }),
  );
  for (const [file, needles] of docLockups) {
    const text = readIf(join(root, file));
    for (const needle of needles) {
      if (!text.includes(needle)) {
        gaps.push(`docs-lockup: ${file} no longer references ${needle} (HO-6.01 docs row: custom wordmark/lockup headers)`);
      }
    }
  }
  return gaps;
}

// ---------------------------------------------------------------------------
// Host-matrix state coverage (audit-gates.md §8 hole 8).
// ---------------------------------------------------------------------------

/** Literal check("state/<axis>/<panel>") names in the host-matrix smoke. */
function hostMatrixStateChecks(root) {
  const checks = new Set();
  for (const match of readIf(join(root, HOST_MATRIX_FILE)).matchAll(/check\(\s*"(state\/[a-z0-9-]+\/[a-z0-9-]+)"/g)) {
    checks.add(match[1]);
  }
  return checks;
}

/**
 * The host matrix must exercise the state axes the UI/UX audit recorded as
 * missing (audit-gates.md §8 hole 8) with per-panel checks named
 * state/<axis>/<panel> (contract-uiux.md §2 state rows T00-T33 and §6):
 * empty for every panelFactories entry, error for the reviewed read-backed
 * panels, loading for the not-read panels, plus the fixed blocked/reconnect
 * recovery pairs. The factories list itself must keep parsing (reuses
 * hostMatrixPanels) or the rule would silently pass on a gutted file.
 */
function hostMatrixStateGaps(root) {
  const panels = hostMatrixPanels(root);
  if (panels === null) {
    return [`host-matrix-state-unparseable: ${HOST_MATRIX_FILE} no longer parses a panelFactories list, so the state/<axis>/<panel> coverage rule cannot run (audit-gates.md §8 hole 8)`];
  }
  const present = new Set(panels);
  const checks = hostMatrixStateChecks(root);
  const gaps = [];
  const missing = (axis, panel, why) => {
    if (!checks.has(`state/${axis}/${panel}`)) {
      gaps.push(`host-matrix-state-missing: state/${axis}/${panel} — ${why}; add check("state/${axis}/${panel}") to ${HOST_MATRIX_FILE}`);
    }
  };
  for (const panel of panels) {
    missing('empty', panel, 'every panelFactories entry must exercise its honest empty state (contract-uiux.md §2/§6)');
  }
  for (const [panel, why] of HOST_MATRIX_READ_BACKED_PANELS) {
    if (!present.has(panel)) {
      gaps.push(`host-matrix-state-stale-panel: "${panel}" is reviewed as read-backed in scripts/dogfood.mjs but is not a panelFactories entry; re-review HOST_MATRIX_READ_BACKED_PANELS`);
      continue;
    }
    missing('error', panel, `read-backed panel: ${why}`);
  }
  for (const [panel, why] of HOST_MATRIX_NOT_READ_PANELS) {
    if (!present.has(panel)) {
      gaps.push(`host-matrix-state-stale-panel: "${panel}" is reviewed as not-read in scripts/dogfood.mjs but is not a panelFactories entry; re-review HOST_MATRIX_NOT_READ_PANELS`);
      continue;
    }
    missing('loading', panel, `not-read presentation: ${why}`);
  }
  for (const [axis, panel, why] of HOST_MATRIX_FIXED_STATE_CHECKS) {
    missing(axis, panel, why);
  }
  return gaps;
}

// ---------------------------------------------------------------------------
// UI/UX contract gate (contract-uiux.md I01-I10, T00-T33, §6, §8, §9).
// ---------------------------------------------------------------------------

/** The JetBrains test corpus: every test source plus test resources. */
function jetbrainsTestCorpus(root) {
  const corpus = [
    ...walkFiles(join(root, 'apps/jetbrains/frontend/src/test/kotlin'), (path) => path.endsWith('.kt')),
    ...walkFiles(join(root, 'apps/jetbrains/frontend/src/test/resources'), () => true),
  ];
  return corpus.map((path) => readIf(path)).join('\n');
}

/**
 * The UIUX_CONTRACT gate: each reviewed row must carry its mandated
 * production needles in the named host file, and at least one configured
 * test needle must exist in that host's test corpus. Two gap classes, exact
 * wording (never lowered):
 *   - `uiux-contract-missing`:  a production needle is absent from its file;
 *   - `uiux-contract-unpinned`: the host's test corpus pins none of the row's
 *     candidate needles.
 * A row may be both missing and unpinned; both findings are reported (the
 * product may not render the group yet AND no test pins it).
 */
export function uiuxContractGaps(root, contract = UIUX_CONTRACT) {
  const gaps = [];
  const corpora = {
    vscode: VSCODE_TEST_CORPUS.map((path) => readIf(join(root, path))).join('\n'),
    jetbrains: jetbrainsTestCorpus(root),
  };
  for (const entry of contract) {
    for (const [file, needle] of entry.must) {
      if (!readIf(join(root, file)).includes(needle)) {
        gaps.push(`uiux-contract-missing: ${entry.id} (${entry.spec}): ${JSON.stringify(needle)} is absent from ${file}`);
      }
    }
    const hosts = entry.host === 'both' ? ['vscode', 'jetbrains'] : [entry.host];
    for (const host of hosts) {
      const needles = entry.tests?.[host] ?? [];
      if (!needles.some((needle) => corpora[host].includes(needle))) {
        gaps.push(`uiux-contract-unpinned: ${entry.id} (${entry.spec}): no ${host} test corpus file pins it (looked for: ${needles.join(', ')})`);
      }
    }
  }
  return gaps;
}

/**
 * The UIUX_STATE_DISPOSITIONS gate: every contract-uiux.md §2 state T00-T33
 * must have exactly one reviewed disposition entry. `enforced` entries must
 * cite UIUX_CONTRACT ids that still carry their production needles;
 * `proposed` entries must record their mandated copy, and the moment any host
 * production file renders that copy the proposal is stale (the product fix
 * landed without an enforced row). Gap kinds:
 *   - `uiux-state-undispositioned`: a state has no entry;
 *   - `uiux-state-dangling`: an enforced entry cites an unknown contract row;
 *   - `uiux-state-disposition-stale`: a cited row's production needle is gone;
 *   - `uiux-state-proposal-stale`: a proposed copy now exists in production.
 * `options` (dispositions/contract/states/productionDirs) exists for the
 * adversarial selftest and defaults to the real tables this checker ships.
 */
export function uiuxStateGaps(root, options = {}) {
  const dispositions = options.dispositions ?? UIUX_STATE_DISPOSITIONS;
  const contract = options.contract ?? UIUX_CONTRACT;
  const states = options.states ?? UIUX_STATE_IDS;
  const productionDirs = options.productionDirs ?? UIUX_PRODUCTION_DIRS;
  const gaps = [];
  const contractById = new Map(contract.map((entry) => [entry.id, entry]));
  const known = new Set(states);

  const entriesByState = new Map();
  for (const entry of dispositions) {
    if (!entriesByState.has(entry.state)) entriesByState.set(entry.state, []);
    entriesByState.get(entry.state).push(entry);
  }
  for (const state of states) {
    const entries = entriesByState.get(state) ?? [];
    if (entries.length === 0) {
      gaps.push(`uiux-state-undispositioned: ${state} has no disposition entry`);
    } else if (entries.length > 1) {
      gaps.push(`uiux-state-undispositioned: ${state} has ${entries.length} disposition entries; exactly one reviewed entry per state is required`);
    }
  }

  const production = new Map();
  if (dispositions.some((entry) => entry.disposition === 'proposed' && (entry.copy ?? []).length > 0)) {
    for (const dir of productionDirs) {
      for (const file of walkFiles(join(root, dir), (path) => /\.(ts|js|css|kt)$/.test(path))) {
        production.set(rel(root, file), readIf(file));
      }
    }
  }

  for (const entry of dispositions) {
    if (!known.has(entry.state)) {
      gaps.push(`uiux-state-undispositioned: ${entry.state} is not a contract-uiux.md §2 state (T00-T33); re-review UIUX_STATE_DISPOSITIONS`);
      continue;
    }
    if (entry.disposition === 'enforced') {
      const rows = Array.isArray(entry.rows) ? entry.rows : [];
      if (rows.length === 0) {
        gaps.push(`uiux-state-disposition-stale: ${entry.state} is enforced without a contract row reference; cite the UIUX_CONTRACT ids that pin it`);
        continue;
      }
      for (const id of rows) {
        const row = contractById.get(id);
        if (row === undefined) {
          gaps.push(`uiux-state-dangling: ${entry.state} references unknown contract row ${id}`);
          continue;
        }
        for (const [file, needle] of row.must ?? []) {
          if (!readIf(join(root, file)).includes(needle)) {
            gaps.push(`uiux-state-disposition-stale: ${entry.state} is enforced via ${id} but ${JSON.stringify(needle)} is no longer present in ${file}`);
          }
        }
      }
      continue;
    }
    if (entry.disposition !== 'proposed') {
      gaps.push(`uiux-state-disposition-stale: ${entry.state} has disposition ${JSON.stringify(entry.disposition)}; expected 'enforced' or 'proposed'`);
      continue;
    }
    if (typeof entry.note !== 'string' || entry.note.trim().length < 30) {
      gaps.push(`uiux-state-proposal-unreviewed: ${entry.state} is proposed without a reviewed reason (>= 30 chars citing the spec and the status class)`);
    }
    const copies = Array.isArray(entry.copy) ? entry.copy : [];
    if (copies.length === 0) {
      gaps.push(`uiux-state-proposal-unreviewed: ${entry.state} is proposed but records no mandated copy; no copy may be silently omitted`);
    }
    for (const needle of copies) {
      const match = [...production.entries()].find(([, text]) => text.includes(needle));
      if (match !== undefined) {
        gaps.push(`uiux-state-proposal-stale: ${entry.state} is reviewed as proposed but its mandated copy ${JSON.stringify(needle)} now exists in ${match[0]} — add an enforced row`);
      }
    }
  }
  return gaps;
}

// ---------------------------------------------------------------------------
// Full analysis (used by --coverage and by the real-tree half of --selftest).
// ---------------------------------------------------------------------------

export function analyze(root = ROOT, options = {}) {
  const planText = options.planText ?? readIf(join(root, PLAN_FILE));
  const classes = [];

  const graph = capabilityGraph(root);
  const crates = graph.crates;
  // Route every capability-graph problem to the surface class it names, so the
  // per-class table shows exactly which surface lost its gate.
  const assigned = new Set();
  const claim = (...prefixes) => {
    const out = [];
    for (const problem of graph.problems) {
      if (assigned.has(problem)) continue;
      if (prefixes.some((prefix) => problem.includes(`: ${prefix}`))) {
        assigned.add(problem);
        out.push(`capability-graph: ${problem}`);
      }
    }
    return out;
  };
  const nativeProblemGaps = claim('native.');
  const toolsProblemGaps = claim('tool.');
  const cliProblemGaps = claim('cli.');
  const providerProblemGaps = claim('provider.');
  const vscodeProblemGaps = claim('vscode.');
  const jetbrainsProblemGaps = claim('jetbrains.');
  const crateProblemGaps = graph.problems.filter((problem) => !assigned.has(problem)).map((problem) => `capability-graph: ${problem}`);

  classes.push({
    surface: 'production crates',
    items: crates.length,
    gate: 'unit test site per crate + capability rows (§1 cargo test --workspace + §2 check-invariant-coverage --check/--release)',
    gaps: [...crateGaps(root, crates), ...crateProblemGaps],
  });

  const discovered = graph.discovered?.ids ?? [];
  const manifestGate = 'manifest row (discovered/capability) + check-invariant-coverage --check/--release (§2)';
  classes.push({ surface: 'native routes', items: discovered.filter((id) => id.startsWith('native.')).length, gate: manifestGate, gaps: nativeProblemGaps });
  classes.push({ surface: 'builtin tools', items: discovered.filter((id) => id.startsWith('tool.')).length, gate: manifestGate, gaps: toolsProblemGaps });
  classes.push({ surface: 'CLI commands', items: discovered.filter((id) => id.startsWith('cli.')).length, gate: manifestGate, gaps: cliProblemGaps });
  classes.push({ surface: 'provider kinds', items: discovered.filter((id) => id.startsWith('provider.')).length, gate: manifestGate, gaps: providerProblemGaps });
  const jetbrainsIds = discovered.filter((id) => id.startsWith('jetbrains.'));
  classes.push({ surface: 'JetBrains contributions', items: jetbrainsIds.length, gate: manifestGate, gaps: jetbrainsProblemGaps });

  const corpus = VSCODE_TEST_CORPUS.map((path) => readIf(join(root, path)));
  classes.push({
    surface: 'VS Code contributions',
    items: vscodeContributionCount(root),
    gate: 'manifest rows + VS Code test corpus references + walkthrough pins (§5 selftest/render/verify-vsix)',
    gaps: [...vscodeProblemGaps, ...vscodeGaps(root, corpus)],
  });

  const panels = parsePanelClasses(root);
  classes.push({
    surface: 'JetBrains panels',
    items: panels.length + chatPanelLeafTabs(root).length,
    gate: 'JetBrainsHostMatrixSmoke panelFactories list + :frontend:smokeHostMatrixZip (§6)',
    gaps: panelGaps(root, { componentClaims: HOST_MATRIX_COMPONENT_CLAIMS }),
  });

  classes.push({
    surface: 'visual identity',
    items: BRAND_KIT.size + HOST_ICON_FILES.length + 3,
    gate: 'v3 identity pins: host Cut F SVGs + docs/assets/brand kit + release icons + docs headers (contract-vi MK-1.02/PL-3.01/ST-5.02/HO-6.01; audit holes 4/5/18)',
    gaps: visualIdentityGaps(root),
  });

  classes.push({
    surface: 'JetBrains host-matrix states',
    items: hostMatrixPanels(root)?.length ?? 0,
    gate: 'state/<axis>/<panel> checks in JetBrainsHostMatrixSmoke (§2 T00-T33 / §6 validation matrix; audit hole 8)',
    gaps: hostMatrixStateGaps(root),
  });

  classes.push({
    surface: 'UI/UX contract',
    items: UIUX_CONTRACT.length + UIUX_STATE_DISPOSITIONS.length,
    gate: 'UIUX_CONTRACT production needles per host file + host test-corpus pins, one reviewed disposition per §2 state T00-T33 (contract-uiux.md I01-I10/T00-T33/§6/§8/§9)',
    gaps: [...uiuxContractGaps(root), ...uiuxStateGaps(root)],
  });

  const testCrates = testCrateSurface(root);
  classes.push({
    surface: 'test-support crates',
    items: testCrates.names.length,
    gate: 'workspace member + cargo test --workspace --all-features (§1)',
    gaps: testCrates.gaps,
  });
  classes.push({
    surface: 'scripts files',
    items: scriptFiles(root).length,
    gate: 'invoked by scripts/dogfood.sh (direct/transitive) or reviewed allowlist with reason',
    gaps: scriptGaps(root, planText, options.scriptAllowlist ?? SCRIPT_ALLOWLIST),
  });

  classes.push({
    surface: 'docs markdown',
    items: existsSync(join(root, 'docs')) ? readdirSync(join(root, 'docs')).filter((name) => name.endsWith('.md')).length : 0,
    gate: 'referenced from README.md/docs index or reviewed allowlist with reason',
    gaps: docsGaps(root, options.docsAllowlist ?? DOCS_ALLOWLIST),
  });

  classes.push({
    surface: 'CI workflows',
    items: workflowFiles(root).length,
    gate: 'workflow-graph + image-pin checkers (§2 selftests; CI status-publish --check)',
    gaps: [],
  });

  classes.push({
    surface: 'dogfood run plan',
    items: PLAN_GATES.length,
    gate: 'scripts/dogfood.sh registers and executes every gate below',
    gaps: planGateGaps(root, planText),
  });

  classes.push({
    surface: 'CI lane coverage',
    items: PLAN_GATES.length,
    gate: 'every PLAN_GATES id maps to a present lane command in .woodpecker/** or a reviewed local-only reason (CI_LANE_MAP / CI_LANE_LOCAL_ONLY)',
    gaps: ciLaneCoverageGaps(root),
  });

  const violations = uiViolations(root, options.internalAllowlist ?? INTERNAL_LABEL_ALLOWLIST);
  const gaps = classes.flatMap((entry) => entry.gaps);
  return { classes, gaps, violations, discovered: discovered.length };
}

function vscodeContributionCount(root) {
  const pkgPath = join(root, 'apps/vscode/package.json');
  if (!existsSync(pkgPath)) return 0;
  let pkg;
  try {
    pkg = JSON.parse(readIf(pkgPath));
  } catch {
    return 0;
  }
  const contributes = pkg.contributes ?? {};
  const configuration = contributes.configuration ?? {};
  const sections = Array.isArray(configuration) ? configuration : [configuration];
  let count = (contributes.commands ?? []).length;
  for (const views of Object.values(contributes.views ?? {})) count += 1 + (views ?? []).length;
  for (const section of sections) {
    if (section?.title) count += 1;
    count += Object.keys(section?.properties ?? {}).length;
  }
  count += Object.keys(contributes.menus ?? {}).length;
  for (const walkthrough of contributes.walkthroughs ?? []) {
    count += 1 + (walkthrough.steps ?? []).length;
  }
  return count;
}

// ---------------------------------------------------------------------------
// Coverage mode.
// ---------------------------------------------------------------------------

/**
 * Every reviewed allowlist exception that is actually load-bearing right now
 * (a stale entry is a gap, not an exception). Printed by --coverage so an
 * allowlisted surface is visible, never silent.
 */
function reviewedExceptions(root, planText) {
  const out = [];
  const executable = executablePlanText(planText);
  for (const script of scriptFiles(root).map((path) => rel(root, path)).sort()) {
    if (!executable.includes(script) && SCRIPT_ALLOWLIST.has(script)) {
      out.push(`${script} — ${SCRIPT_ALLOWLIST.get(script)}`);
    }
  }
  for (const name of unreferencedDocs(root)) {
    const reason = DOCS_ALLOWLIST.get(`docs/${name}`);
    if (reason) out.push(`docs/${name} — ${reason}`);
  }
  for (const line of testCrateSurface(root).exceptions) out.push(line);
  const mappedGateIds = new Set(CI_LANE_MAP.map((entry) => entry.id));
  const gateIds = new Set(PLAN_GATES.map(([id]) => id));
  for (const [id, reason] of CI_LANE_LOCAL_ONLY) {
    if (gateIds.has(id) && !mappedGateIds.has(id) && reason.trim().length >= 30) {
      out.push(`ci-lane-local-only ${id} — ${reason}`);
    }
  }
  return out;
}

function runCoverage() {
  const { classes, gaps, violations, discovered } = analyze(ROOT);
  console.log('DOGFOOD surface coverage (every surface must carry a registered gate)');
  console.log('');
  const width = Math.max(24, ...classes.map((entry) => entry.surface.length));
  console.log(`${'surface'.padEnd(width)}  items  status  gate`);
  for (const entry of classes) {
    const status = entry.gaps.length === 0 ? 'PASS' : 'GAP';
    console.log(`${entry.surface.padEnd(width)}  ${String(entry.items).padStart(5)}  ${status.padEnd(4)}    ${entry.gate}`);
  }
  console.log('');
  console.log(`discovered product ids cross-checked: ${discovered}`);
  console.log('UI/UX rules: raw color, terminal argv, calm card headers, internal-identifier labels, visual identity pins, host-matrix state coverage, UI/UX contract pins, UI/UX state dispositions');
  const exceptions = reviewedExceptions(ROOT, readIf(join(ROOT, PLAN_FILE)));
  if (exceptions.length > 0) {
    console.log('');
    console.log(`reviewed allowlist exceptions (${exceptions.length}; no silent gaps):`);
    for (const exception of exceptions) console.log(`  - ${exception}`);
  }
  for (const violation of violations) console.error(`dogfood: ${violation}`);
  for (const gap of gaps) console.error(`dogfood: ${gap}`);
  const failures = gaps.length + violations.length;
  if (failures > 0) {
    console.error(`dogfood coverage: FAIL (${gaps.length} gate gap(s), ${violations.length} UI/UX violation(s))`);
    return 1;
  }
  console.log('dogfood coverage: PASS (0 gaps, 0 UI/UX violations)');
  return 0;
}

// ---------------------------------------------------------------------------
// Selftest: adversarial fixtures. Each planted gap MUST be detected, and the
// real tree must report zero gaps.
// ---------------------------------------------------------------------------

function selftest() {
  let failures = 0;
  const check = (name, ok, detail = '') => {
    if (ok) {
      console.log(`selftest ok: ${name}`);
    } else {
      console.error(`selftest FAIL: ${name}${detail ? ` — ${detail}` : ''}`);
      failures += 1;
    }
  };
  const withFixture = (body) => {
    const fixture = fixtureRoot();
    try {
      body(fixture);
    } finally {
      rmSync(fixture, { recursive: true, force: true });
    }
  };

  // (a) a JetBrains panel removed from the host-matrix list must be a gap.
  withFixture((fixture) => {
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/GhostPanel.kt', 'package dev.faktor.frontend\nclass GhostPanel\n');
    const matrix = (names) =>
      [
        'package dev.faktor.frontend',
        'object JetBrainsHostMatrixSmoke {',
        '    private val panelFactories: List<Pair<String, () -> JComponent>> = listOf(',
        ...names.map((name) => `        "${name}" to { ghost() },`),
        '    )',
        '}',
      ].join('\n');
    writeFixture(fixture, 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsHostMatrixSmoke.kt', matrix(['ghost']));
    check('a panel rendered in the host matrix passes', panelGaps(fixture).length === 0, panelGaps(fixture).join('; '));
    writeFixture(fixture, 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsHostMatrixSmoke.kt', matrix([]));
    check('a panel removed from the host-matrix list is detected', panelGaps(fixture).some((gap) => gap.includes('GhostPanel')), panelGaps(fixture).join('; '));
  });

  // (b) a scripts file added without a gate or allowlist must be a gap.
  withFixture((fixture) => {
    writeFixture(fixture, 'scripts/ghost.mjs', 'console.log("ghost");\n');
    const noPlan = 'node scripts/check-invariants.mjs\n';
    check('a scripts file without a gate is detected', scriptGaps(fixture, noPlan, new Map()).some((gap) => gap.includes('ungated-script: scripts/ghost.mjs')), scriptGaps(fixture, noPlan, new Map()).join('; '));
    check('a scripts file invoked by the run plan passes', scriptGaps(fixture, 'node scripts/ghost.mjs\n', new Map()).length === 0, scriptGaps(fixture, 'node scripts/ghost.mjs\n', new Map()).join('; '));
    check(
      'a scripts file allowlisted with a reason passes',
      scriptGaps(fixture, noPlan, new Map([['scripts/ghost.mjs', 'reviewed operator helper, exercised outside the dogfood plan by the release lane']])).length === 0,
    );
    check('a scripts file with an empty allowlist reason is still a gap', scriptGaps(fixture, noPlan, new Map([['scripts/ghost.mjs', 'short']])).some((gap) => gap.includes('ungated-script')));
    check(
      'a transitively invoked scripts file is gated',
      (() => {
        writeFixture(fixture, 'scripts/invoker.sh', 'node scripts/ghost.mjs\n');
        return scriptGaps(fixture, 'bash scripts/invoker.sh\n', new Map()).length === 0;
      })(),
    );
  });

  // (c) a planted raw Color( in a panel must fail the color rule.
  withFixture((fixture) => {
    const panel = 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TournamentPanel.kt';
    writeFixture(fixture, panel, 'package dev.faktor.frontend\nimport java.awt.Color\nfun badge() { val c = Color(1, 2, 3) }\n');
    check('a planted raw Color( in a panel is detected with file:line', colorViolations(fixture).some((violation) => violation.includes('TournamentPanel.kt:3')), colorViolations(fixture).join('; '));
    writeFixture(fixture, panel, 'package dev.faktor.frontend\nfun badge() { val c = cardBorderColor() }\n');
    check('a token-based panel color passes', colorViolations(fixture).length === 0, colorViolations(fixture).join('; '));
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/PixelAgents.kt', 'package dev.faktor.frontend\nfun sprite() { val c = Color(9, 9, 9) }\n');
    check('PixelAgents.kt keeps its raw-color exemption', colorViolations(fixture).length === 0, colorViolations(fixture).join('; '));
  });

  // (d) a VS Code command added with no test reference must be a gap.
  withFixture((fixture) => {
    writeFixture(
      fixture,
      'apps/vscode/package.json',
      JSON.stringify({ name: 'ghost', contributes: { commands: [{ command: 'faktor.ghost', title: 'Ghost' }] } }, null, 2),
    );
    check('a VS Code command without a test reference is detected', vscodeGaps(fixture, ['const selftest = true;']).some((gap) => gap.includes('vscode-command-without-test: faktor.ghost')), vscodeGaps(fixture, ['const selftest = true;']).join('; '));
    check('a VS Code command pinned by the test corpus passes the reference half', !vscodeGaps(fixture, ['assert("faktor.ghost")']).some((gap) => gap.includes('vscode-command-without-test')));
  });

  // (e) terminal argv, calm headers and internal-identifier rules.
  withFixture((fixture) => {
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TerminalPanel.kt', "package dev.faktor.frontend\nfun parse(line: String) { line.split(' ') }\n");
    check('a space-separated TerminalPanel argv split is detected', terminalArgvViolations(fixture).some((violation) => violation.includes('TerminalPanel.kt:2')), terminalArgvViolations(fixture).join('; '));
    writeFixture(fixture, 'apps/vscode/media/chat.css', '.card h2 {\n  text-transform: uppercase;\n}\n');
    check('an uppercase card header is detected', cssHeaderViolations(fixture).some((violation) => violation.includes('uppercase-header')), cssHeaderViolations(fixture).join('; '));
    writeFixture(fixture, 'apps/vscode/media/chat.css', '.card h2 {\n  text-transform: none;\n}\n');
    check('the calm card header style passes', cssHeaderViolations(fixture).length === 0);
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/StatusPanel.kt', 'package dev.faktor.frontend\nval label = "session_id=42"\n');
    check('an unreviewed internal-identifier label is detected', internalLabelViolations(fixture, []).some((violation) => violation.includes('session_id')), internalLabelViolations(fixture, []).join('; '));
    check(
      'a reviewed internal-identifier label passes and a count change fails',
      internalLabelViolations(fixture, [
        {
          file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/StatusPanel.kt',
          token: 'session_id',
          count: 1,
          reason: 'synthetic selftest fixture',
        },
      ]).length === 0 &&
        internalLabelViolations(fixture, [
          {
            file: 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/StatusPanel.kt',
            token: 'session_id',
            count: 2,
            reason: 'synthetic selftest fixture',
          },
        ]).some((violation) => violation.includes('internal-identifier-unreviewed')),
    );
  });

  // (f) plan-gate drift: a registered gate missing from dogfood.sh fails.
  withFixture((fixture) => {
    check('a missing plan gate is detected', planGateGaps(fixture, '#!/usr/bin/env bash\nnode scripts/check-invariants.mjs\n').some((gap) => gap.includes('missing-plan-gate: cargo-fmt')));
  });

  // (g) visual identity: the canonical fixture must pass and each planted
  // artwork/metadata mutation must be detected.
  const brandSymbolSvg = (fill) =>
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 50 52" role="img" aria-label="Faktor">\n  <path fill="${fill}" fill-rule="nonzero" d="${CUT_F_PATH_CLOSED}"/>\n</svg>\n`;
  const hostIconSvg = (extra = '') =>
    `<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="-4 -4 58 60" fill="currentColor" role="img" aria-label="Faktor">\n  <path d="${CUT_F_PATH}"/>${extra}\n</svg>\n`;
  const pluginIconSvg = `<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40" viewBox="0 0 40 40">\n  <rect width="40" height="40" rx="9" fill="#08110C"/>\n  <g transform="translate(10.63 9.84) scale(0.39)">\n    <path fill="#D9BA75" d="${CUT_F_PATH}"/>\n  </g>\n</svg>\n`;
  // The stacked masters bake the same contour at a uniform scale (~0.4056).
  const stackedSymbolPath = 'M294.268 33.336 L294.268 12.244 L314.544 12.244 L314.544 15.492 L312.112 17.924 L300.756 17.924 L300.756 22.384 L310.896 22.384 L310.896 25.628 L308.46 28.064 L300.756 28.064 L300.756 33.336 L294.268 33.336 Z';
  const pngBytes = () => {
    const bytes = Buffer.alloc(33);
    [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a].forEach((value, index) => {
      bytes[index] = value;
    });
    bytes.write('IHDR', 12, 'ascii');
    bytes.writeUInt32BE(128, 16);
    bytes.writeUInt32BE(128, 20);
    return bytes;
  };
  const writeBrandFixture = (fixture) => {
    for (const [name, kind] of BRAND_KIT) {
      if (name === 'faktor-tokens.json') continue;
      let content;
      if (kind === 'symbol') {
        content = brandSymbolSvg(name.includes('accent') ? '#D9BA75' : name.includes('white') ? '#FFFFFF' : name.includes('ink') ? '#08110C' : 'currentColor');
      } else if (kind === 'wordmark') {
        content = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 340 63"><path fill="currentColor" d="M70.824 46.94 L70.824 11.648 C70.824 6.548 73.764 4 79.648 4 Z"/></svg>\n';
      } else if (kind === 'tile') {
        content = pluginIconSvg;
      } else if (kind === 'stacked') {
        content = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 57.908 47.032"><path fill="#08110C" fill-rule="nonzero" d="${stackedSymbolPath}"/></svg>\n`;
      } else {
        content = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 340 63"><path fill="currentColor" d="${CUT_F_PATH_CLOSED}"/></svg>\n`;
      }
      writeFixture(fixture, `docs/assets/brand/${name}`, content);
    }
    writeFixture(
      fixture,
      'docs/assets/brand/faktor-tokens.json',
      JSON.stringify(
        {
          palette: { ink: '#08110C', forest: '#143D2B', paper: '#F3F5EF', sage: '#92AA99', ochre: '#D9BA75', ochreDeep: '#72541E' },
          state: {
            success: { light: '#145C39', dark: '#68DC93' },
            pending: { light: '#72541E', dark: '#D9BA75' },
            failure: { light: '#9D3131', dark: '#FFB3AE' },
          },
          mark: { geometry: { unit: '1 viewBox unit; stem 16 units wide, arms 14 units thick, clipped tips 6 units' } },
        },
        null,
        2,
      ),
    );
    writeFixture(fixture, HOST_ICON_FILES[0], hostIconSvg());
    writeFixture(fixture, HOST_ICON_FILES[1], hostIconSvg().replace('fill="currentColor"', 'fill="#6E6E6E"'));
    writeFixture(
      fixture,
      'apps/vscode/package.json',
      JSON.stringify({ name: 'faktor', icon: 'media/faktor-icon.png', contributes: { viewsContainers: { activitybar: [{ id: 'faktor', icon: 'media/faktor.svg' }] } } }, null, 2),
    );
    writeFixture(fixture, 'apps/vscode/media/faktor-icon.png', pngBytes());
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/resources/META-INF/pluginIcon.svg', pluginIconSvg);
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/resources/META-INF/pluginIcon_dark.svg', pluginIconSvg);
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml', '<idea-plugin><extensions><toolWindow icon="/META-INF/faktor.svg"/></extensions></idea-plugin>\n');
    writeFixture(fixture, 'README.md', '# Faktor\n<img src="docs/assets/brand/faktor-lockup-ink.svg"><source srcset="docs/assets/brand/faktor-lockup-accent.svg">\n');
    writeFixture(fixture, 'apps/README.md', '# Apps\n<img src="../docs/assets/brand/faktor-lockup-ink.svg"><source srcset="../docs/assets/brand/faktor-lockup-accent.svg">\n');
    writeFixture(fixture, 'apps/jetbrains/README.md', '# JetBrains\n<img src="../../docs/assets/brand/faktor-lockup-ink.svg"><source srcset="../../docs/assets/brand/faktor-lockup-accent.svg">\n');
  };

  withFixture((fixture) => {
    writeBrandFixture(fixture);
    check('the canonical visual-identity fixture passes', visualIdentityGaps(fixture).length === 0, visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    writeFixture(fixture, HOST_ICON_FILES[0], hostIconSvg().replace('L16 52 Z', 'C16 52 Z'));
    check('a Cut F host path switching an L command to C is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('host-icon-path') || gap.includes('host-icon-command')), visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    writeFixture(fixture, HOST_ICON_FILES[1], `<svg xmlns="http://www.w3.org/2000/svg"><circle cx="8" cy="8" r="4"/><path fill="#6E6E6E" d="${CUT_F_PATH}"/></svg>\n`);
    check('a circle added to a host mark is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('host-icon-element')), visualIdentityGaps(fixture).join('; '));
    writeFixture(fixture, HOST_ICON_FILES[1], `<svg xmlns="http://www.w3.org/2000/svg"><path fill="#6E6E6E" d="${CUT_F_PATH}"/><path fill="#D9BA75" d="${CUT_F_PATH}"/></svg>\n`);
    check('a second hex in the JetBrains host icon is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('host-icon-monochrome')), visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    writeFixture(fixture, 'docs/assets/brand/faktor-lockup-ink.svg', '<svg xmlns="http://www.w3.org/2000/svg"><path fill="#08110C" d="M0 52 C0 0 50 0 50 52 Z"/></svg>\n');
    check('a lockup that lost the canonical Cut F is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('brand-kit-symbol')), visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    rmSync(join(fixture, 'docs/assets/brand'), { recursive: true, force: true });
    check('an empty brand kit is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('brand-kit-missing')), visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    writeFixture(fixture, 'docs/assets/brand/faktor-tokens.json', JSON.stringify({ palette: { ink: '#000000' } }));
    const gaps = visualIdentityGaps(fixture);
    check(
      'a wrong-palette tokens.json is detected',
      gaps.some((gap) => gap.includes('brand-tokens-palette')) && gaps.some((gap) => gap.includes('brand-tokens-state')) && gaps.some((gap) => gap.includes('brand-tokens-mark')),
      gaps.join('; '),
    );
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    writeFixture(fixture, 'apps/vscode/package.json', JSON.stringify({ name: 'faktor', contributes: { viewsContainers: { activitybar: [{ id: 'faktor', icon: 'media/faktor.svg' }] } } }));
    check('a missing release icon field is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('vscode-release-icon')), visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    writeFixture(fixture, 'apps/vscode/package.json', JSON.stringify({ name: 'faktor', icon: 'media/faktor-icon.png', contributes: { viewsContainers: { activitybar: [{ id: 'faktor', icon: 'media/other.svg' }] } } }));
    check('a re-pointed activitybar icon is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('vscode-activitybar-icon')), visualIdentityGaps(fixture).join('; '));
  });
  withFixture((fixture) => {
    writeBrandFixture(fixture);
    const wrongPng = pngBytes();
    wrongPng.writeUInt32BE(64, 16);
    wrongPng.writeUInt32BE(64, 20);
    writeFixture(fixture, 'apps/vscode/media/faktor-icon.png', wrongPng);
    check('a release PNG with a wrong IHDR size is detected', visualIdentityGaps(fixture).some((gap) => gap.includes('vscode-icon-png')), visualIdentityGaps(fixture).join('; '));
  });

  // (h) host-matrix state coverage (audit-gates.md §8 hole 8).
  const matrixFixture = (factories, checks) =>
    [
      'package dev.faktor.frontend',
      'object JetBrainsHostMatrixSmoke {',
      '    private val panelFactories: List<Pair<String, () -> JComponent>> = listOf(',
      ...factories.map((name) => `        "${name}" to { panel() },`),
      '    )',
      '',
      '    private fun stateChecks() {',
      ...checks.map((name) => `        check("${name}") { }`),
      '    }',
      '}',
    ].join('\n');
  const stateFixtureFactories = ['task', ...HOST_MATRIX_READ_BACKED_PANELS.keys()];
  const stateFixtureChecks = [
    ...stateFixtureFactories.map((name) => `state/empty/${name}`),
    ...[...HOST_MATRIX_READ_BACKED_PANELS.keys()].map((name) => `state/error/${name}`),
    ...[...HOST_MATRIX_NOT_READ_PANELS.keys()].map((name) => `state/loading/${name}`),
    ...HOST_MATRIX_FIXED_STATE_CHECKS.map(([axis, panel]) => `state/${axis}/${panel}`),
  ];
  withFixture((fixture) => {
    writeFixture(fixture, HOST_MATRIX_FILE, matrixFixture(stateFixtureFactories, stateFixtureChecks));
    check('the canonical host-matrix state fixture passes', hostMatrixStateGaps(fixture).length === 0, hostMatrixStateGaps(fixture).join('; '));
    writeFixture(fixture, HOST_MATRIX_FILE, matrixFixture(stateFixtureFactories, stateFixtureChecks.filter((name) => name !== 'state/empty/task')));
    check('a host matrix missing state/empty/<panel> is detected', hostMatrixStateGaps(fixture).some((gap) => gap.includes('host-matrix-state-missing: state/empty/task')), hostMatrixStateGaps(fixture).join('; '));
    writeFixture(fixture, HOST_MATRIX_FILE, matrixFixture(stateFixtureFactories, stateFixtureChecks.filter((name) => name !== 'state/error/outcomes')));
    check('a host matrix missing state/error/outcomes is detected', hostMatrixStateGaps(fixture).some((gap) => gap.includes('host-matrix-state-missing: state/error/outcomes')), hostMatrixStateGaps(fixture).join('; '));
    writeFixture(fixture, HOST_MATRIX_FILE, 'package dev.faktor.frontend\nobject JetBrainsHostMatrixSmoke\n');
    check('a host matrix without a panelFactories list is detected', hostMatrixStateGaps(fixture).some((gap) => gap.includes('host-matrix-state-unparseable')), hostMatrixStateGaps(fixture).join('; '));
  });

  // (i) the UI/UX contract gate (contract-uiux.md): a fixture tree that
  // satisfies every production needle and host corpus pin passes; a removed
  // production marker reports uiux-contract-missing; lost host test needles
  // report uiux-contract-unpinned.
  const contractFixture = [
    {
      id: 'UIUX-FIXTURE-VSC',
      spec: 'fixture §2 T00',
      host: 'vscode',
      must: [['apps/vscode/src/fixture.ts', 'FIXTURE_VS_PRODUCTION_MARKER']],
      tests: { vscode: ['FIXTURE_VS_TEST_MARKER'] },
    },
    {
      id: 'UIUX-FIXTURE-JB',
      spec: 'fixture §2 T10',
      host: 'jetbrains',
      must: [['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FixturePanel.kt', 'FIXTURE_JB_PRODUCTION_MARKER']],
      tests: { jetbrains: ['FIXTURE_JB_TEST_MARKER'] },
    },
  ];
  const writeContractFixture = (fixture, options = {}) => {
    const vscodeMarker = options.vscodeMarker ?? 'FIXTURE_VS_PRODUCTION_MARKER';
    const vscodeTest = options.vscodeTest ?? 'FIXTURE_VS_TEST_MARKER';
    writeFixture(fixture, 'apps/vscode/src/fixture.ts', `export const marker = '${vscodeMarker}';\n`);
    writeFixture(fixture, VSCODE_SELFTEST, `// fixture test corpus\n${vscodeTest}\n`);
    writeFixture(fixture, 'apps/vscode/scripts/render-webview.mjs', '// fixture test corpus\n');
    writeFixture(fixture, 'apps/vscode/scripts/verify-vsix.mjs', '// fixture test corpus\n');
    writeFixture(fixture, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FixturePanel.kt', 'package dev.faktor.frontend\nval marker = "FIXTURE_JB_PRODUCTION_MARKER"\n');
    writeFixture(fixture, 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/FixtureSmoke.kt', 'package dev.faktor.frontend\nval test = "FIXTURE_JB_TEST_MARKER"\n');
  };
  withFixture((fixture) => {
    writeContractFixture(fixture);
    const gaps = uiuxContractGaps(fixture, contractFixture);
    check('the canonical UI/UX contract fixture passes', gaps.length === 0, gaps.join('; '));
  });
  withFixture((fixture) => {
    writeContractFixture(fixture, { vscodeMarker: 'DIFFERENT_MARKER' });
    const gaps = uiuxContractGaps(fixture, contractFixture);
    check(
      'a removed UI/UX contract production marker is detected',
      gaps.some(
        (gap) =>
          gap.startsWith('uiux-contract-missing: UIUX-FIXTURE-VSC') &&
          gap.includes('"FIXTURE_VS_PRODUCTION_MARKER" is absent from apps/vscode/src/fixture.ts'),
      ),
      gaps.join('; '),
    );
  });
  withFixture((fixture) => {
    writeContractFixture(fixture, { vscodeTest: 'SOME_OTHER_TEST' });
    const gaps = uiuxContractGaps(fixture, contractFixture);
    check(
      'a UI/UX contract row with no host test pin is detected',
      gaps.some((gap) => gap.startsWith('uiux-contract-unpinned: UIUX-FIXTURE-VSC')) && !gaps.some((gap) => gap.startsWith('uiux-contract-missing')),
      gaps.join('; '),
    );
  });

  // (j) CI lane coverage: a mapped gate with its lane command present passes;
  // removing the command is stale; an unmapped gate is a gap; a local-only
  // entry needs a reviewed reason and must not shadow a real lane.
  const laneGates = [
    ['ghost-a', '§T', 'ghost-a-command'],
    ['ghost-b', '§T', 'ghost-b-command'],
  ];
  const laneMap = [
    { id: 'ghost-a', lanes: [['.woodpecker/untrusted/pr.yaml', 'ghost-a-command']] },
    { id: 'ghost-b', lanes: [['.woodpecker/trusted/nightly.yaml', 'ghost-b-command']] },
  ];
  const laneOptions = (overrides = {}) => ({ gates: laneGates, map: laneMap, localOnly: new Map(), ...overrides });
  const reviewedReason = 'requires an interactive desktop host that no CI container lane can provide';
  withFixture((fixture) => {
    writeFixture(fixture, '.woodpecker/untrusted/pr.yaml', 'steps:\n  - ghost-a-command\n');
    writeFixture(fixture, '.woodpecker/trusted/nightly.yaml', 'steps:\n  - ghost-b-command\n');
    check(
      'a gate mapped to a present CI lane command passes',
      ciLaneCoverageGaps(fixture, laneOptions()).length === 0,
      ciLaneCoverageGaps(fixture, laneOptions()).join('; '),
    );
    writeFixture(fixture, '.woodpecker/untrusted/pr.yaml', 'steps:\n  - ghost-a-renamed\n');
    check(
      'a gate whose CI lane command was removed is detected as ci-lane-stale',
      ciLaneCoverageGaps(fixture, laneOptions()).some((gap) => gap.startsWith('ci-lane-stale: ghost-a')),
      ciLaneCoverageGaps(fixture, laneOptions()).join('; '),
    );
    writeFixture(fixture, '.woodpecker/untrusted/pr.yaml', 'steps:\n  - ghost-a-command\n');
    check(
      'an unmapped gate is detected as ci-lane-gap',
      ciLaneCoverageGaps(fixture, laneOptions({ map: [laneMap[0]] })).some(
        (gap) => gap.startsWith('ci-lane-gap: ghost-b (') && gap.includes('not mapped to any CI lane command'),
      ),
      ciLaneCoverageGaps(fixture, laneOptions({ map: [laneMap[0]] })).join('; '),
    );
    check(
      'a local-only entry with a short reason is still a gap',
      ciLaneCoverageGaps(fixture, laneOptions({ map: [], localOnly: new Map([['ghost-a', 'short']]) })).some((gap) =>
        gap.startsWith('ci-lane-gap: ghost-a'),
      ),
      ciLaneCoverageGaps(fixture, laneOptions({ map: [], localOnly: new Map([['ghost-a', 'short']]) })).join('; '),
    );
    check(
      'a local-only entry with a reviewed reason passes',
      ciLaneCoverageGaps(fixture, laneOptions({ gates: [laneGates[0]], map: [], localOnly: new Map([['ghost-a', reviewedReason]]) })).length === 0,
      ciLaneCoverageGaps(fixture, laneOptions({ gates: [laneGates[0]], map: [], localOnly: new Map([['ghost-a', reviewedReason]]) })).join('; '),
    );
    check(
      'a stale local-only entry is detected',
      ciLaneCoverageGaps(fixture, laneOptions({ localOnly: new Map([['ghost-gone', reviewedReason]]) })).some((gap) =>
        gap.startsWith('ci-lane-stale-local-only: ghost-gone'),
      ),
      ciLaneCoverageGaps(fixture, laneOptions({ localOnly: new Map([['ghost-gone', reviewedReason]]) })).join('; '),
    );
    check(
      'a local-only entry that also runs in a lane is refused',
      ciLaneCoverageGaps(fixture, laneOptions({ localOnly: new Map([['ghost-a', reviewedReason]]) })).some((gap) =>
        gap.startsWith('ci-lane-stale-local-only: ghost-a'),
      ),
      ciLaneCoverageGaps(fixture, laneOptions({ localOnly: new Map([['ghost-a', reviewedReason]]) })).join('; '),
    );
  });

  // (k) the UI/UX state dispositions (contract-uiux.md §2 T00-T33): exactly
  // one reviewed entry per state; enforced entries must resolve against live
  // contract rows; a proposed copy that appears in a host production file is
  // a stale proposal.
  const dispositionStates = ['T00', 'T01'];
  const dispositionContract = [
    {
      id: 'UIUX-DISPOSITION-FIXTURE-1',
      spec: 'fixture §2 T00',
      host: 'vscode',
      must: [['apps/vscode/src/dispositionFixture.ts', 'DISPOSITION_FIXTURE_MARKER']],
      tests: { vscode: ['DISPOSITION_FIXTURE_TEST'] },
    },
    {
      id: 'UIUX-DISPOSITION-FIXTURE-2',
      spec: 'fixture §2 T01',
      host: 'vscode',
      must: [['apps/vscode/src/dispositionFixture2.ts', 'DISPOSITION_FIXTURE_MARKER_TWO']],
      tests: { vscode: ['DISPOSITION_FIXTURE_TEST'] },
    },
  ];
  const dispositionTable = [
    {
      state: 'T00',
      spec: 'fixture T00',
      disposition: 'enforced',
      rows: ['UIUX-DISPOSITION-FIXTURE-1'],
      note: 'fixture enforced entry with a resolved contract row',
    },
    {
      state: 'T01',
      spec: 'fixture T01',
      disposition: 'proposed',
      rows: [],
      copy: ['FIXTURE_PROPOSED_COPY: no host renders this sentence yet'],
      note: 'fixture proposed reason (fixture T01, presentation_refinement) long enough to count as reviewed',
    },
  ];
  const dispositionOptions = (overrides = {}) => ({
    dispositions: dispositionTable,
    contract: dispositionContract,
    states: dispositionStates,
    productionDirs: ['apps/vscode/src'],
    ...overrides,
  });
  withFixture((fixture) => {
    const writeDispositionFixture = () => {
      writeFixture(fixture, 'apps/vscode/src/dispositionFixture.ts', 'export const marker = "DISPOSITION_FIXTURE_MARKER";\n');
      writeFixture(fixture, 'apps/vscode/src/dispositionFixture2.ts', 'export const marker = "DISPOSITION_FIXTURE_MARKER_TWO";\n');
    };
    writeDispositionFixture();
    check(
      'the canonical UI/UX state-disposition fixture passes',
      uiuxStateGaps(fixture, dispositionOptions()).length === 0,
      uiuxStateGaps(fixture, dispositionOptions()).join('; '),
    );
    check(
      'an undispositioned state is detected',
      uiuxStateGaps(fixture, dispositionOptions({ dispositions: [dispositionTable[0]] })).some((gap) =>
        gap.includes('uiux-state-undispositioned: T01 has no disposition entry'),
      ),
      uiuxStateGaps(fixture, dispositionOptions({ dispositions: [dispositionTable[0]] })).join('; '),
    );
    check(
      'a dangling disposition reference is detected',
      uiuxStateGaps(
        fixture,
        dispositionOptions({ dispositions: [{ ...dispositionTable[0], rows: ['UIUX-GONE-ROW'] }, dispositionTable[1]] }),
      ).some((gap) => gap.includes('uiux-state-dangling: T00 references unknown contract row UIUX-GONE-ROW')),
      uiuxStateGaps(
        fixture,
        dispositionOptions({ dispositions: [{ ...dispositionTable[0], rows: ['UIUX-GONE-ROW'] }, dispositionTable[1]] }),
      ).join('; '),
    );
    writeFixture(fixture, 'apps/vscode/src/dispositionFixture.ts', 'export const marker = "SOMETHING_ELSE";\n');
    check(
      'an enforced disposition whose production needle vanished is detected',
      uiuxStateGaps(fixture, dispositionOptions()).some((gap) =>
        gap.includes('uiux-state-disposition-stale: T00 is enforced via UIUX-DISPOSITION-FIXTURE-1'),
      ),
      uiuxStateGaps(fixture, dispositionOptions()).join('; '),
    );
    writeFixture(
      fixture,
      'apps/vscode/src/dispositionFixture.ts',
      'export const marker = "DISPOSITION_FIXTURE_MARKER";\nexport const proposed = "FIXTURE_PROPOSED_COPY: no host renders this sentence yet";\n',
    );
    check(
      'a stale proposal (the proposed copy now ships) is detected',
      uiuxStateGaps(fixture, dispositionOptions()).some(
        (gap) =>
          gap.includes('uiux-state-proposal-stale: T01') &&
          gap.includes('is reviewed as proposed but its mandated copy') &&
          gap.includes('now exists in apps/vscode/src/dispositionFixture.ts'),
      ),
      uiuxStateGaps(fixture, dispositionOptions()).join('; '),
    );
    writeDispositionFixture();
    check(
      'the disposition fixture is restored to its canonical shape',
      uiuxStateGaps(fixture, dispositionOptions()).length === 0,
      uiuxStateGaps(fixture, dispositionOptions()).join('; '),
    );
  });

  // The fixtures above prove the detectors work; this proves the current tree
  // is fully green. The UI/UX contract class (UIUX_CONTRACT rows + state
  // dispositions) is strict here too: any real-tree contract finding fails
  // the selftest exactly like --coverage, so the class can never silently
  // regress behind a note.
  const real = analyze(ROOT);
  const contractGaps = real.gaps.filter((gap) => gap.startsWith('uiux-contract-') || gap.startsWith('uiux-state-'));
  const otherGaps = real.gaps.filter((gap) => !gap.startsWith('uiux-contract-') && !gap.startsWith('uiux-state-'));
  check(`the real tree reports 0 UI/UX contract findings (${contractGaps.length})`, contractGaps.length === 0, contractGaps.slice(0, 10).join('; '));
  check(`the real tree reports 0 gate gaps outside the UI/UX contract class (${otherGaps.length})`, otherGaps.length === 0, otherGaps.slice(0, 10).join('; '));
  check(`the real tree reports 0 UI/UX violations (${real.violations.length})`, real.violations.length === 0, real.violations.slice(0, 10).join('; '));

  if (failures > 0) {
    console.error(`dogfood selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('dogfood selftest: PASS');
  return 0;
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

function main() {
  const args = process.argv.slice(2);
  if (args.includes('--selftest') || args.includes('selftest')) return selftest();
  if (args.length === 0 || args.includes('--coverage')) return runCoverage();
  console.error('usage: node scripts/dogfood.mjs --coverage | --selftest');
  return 2;
}

const isMain = process.argv[1] && process.argv[1].endsWith('dogfood.mjs');
if (isMain) {
  process.exit(main());
}
