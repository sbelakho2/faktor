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
  ['compiled-capability-tests', '§1', 'check-capability-tests-compiled.py'],
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
  ['dogfood-coverage', '§7', 'dogfood.mjs --coverage'],
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
    lanes: [['.woodpecker/trusted/trusted.yaml', 'check-capability-tests-compiled.py']],
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
    'scripts/install-matrix.mjs':
      'release install-matrix lane (clean-prefix extract + doctor + archive structure); requires built release artifacts and runs in the full certify-local profile.',
    'scripts/package-artifacts.sh':
      'release artifact packaging lane (certify.sh release-artifacts); requires built release binaries, out of scope for the local dogfood suite.',
    'scripts/soak.sh':
      'bounded soak driver; long [soak] runs execute in the nightly/soak-smoke lanes with wall-clock budgets, never in the interactive dogfood suite.',
    'scripts/supply-chain.sh':
      'static-lane supply-chain gate; needs SUPPLY_CHAIN_BUILD=1 and a release build, executed by the trusted static lane.',
    'scripts/update-iana-special-registry.sh':
      'operator data-refresh helper for the IANA special-registry table; requires network and the generated table is a frozen contract checked by cargo tests.',
    'scripts/windows-visual-baseline.ps1':
      'PowerShell implementation of the Windows visual baseline lane; runs on the self-hosted windows agent and is not executable in the linux dogfood suite.',
    'scripts/woodpecker/activate.sh':
      'operator helper for Woodpecker server/project setup (scripts/woodpecker/setup.md); not part of the repository build or test surface.',
    'scripts/woodpecker/verify-boundary.sh':
      'operator assertion for the trusted/untrusted Woodpecker project boundary; exercised during server setup, not by the dogfood suite.',
    'scripts/certification/fixtures/woodpecker-api/mock_server.py':
      'fixture mock Woodpecker API; spawned by scripts/certify.sh release gates and the certification selftests.',
    'scripts/certification/prove-all.mjs':
      'certificate aggregate binding the checkout bytes; its component selftests (discovery, workflow graph, publisher, visual, soak) each run in dogfood §2.',
    'scripts/certification/publish-status.py':
      'Python publisher implementation pinned to the same canonical payload vector as publish-status.mjs (whose selftest runs in dogfood §2).',
    'scripts/certification/lane-marker.py':
      'Python lane-marker writer; its byte-for-byte agreement with the node/shell writers is proven by the lane-marker selftest inside evidence.mjs selftest (dogfood §2).',
    'scripts/check-fault-seams.sh':
      'static fault-seam checker executed directly by the PR static lane and the trusted static lane (sh scripts/check-fault-seams.sh); it is a lane command, not a dogfood-plan step.',
    'scripts/mutations/vscode-ts-loader.mjs':
      'node --experimental-loader shim referenced by the VS Code mutation gate commands in scripts/mutations/*.json, executed by check-invariants --mutations (dogfood §3).',
    'scripts/mutation-run.mjs':
      'planted-mutation runner; tests/invariants.toml mutation_command invokes it per invariant and check-invariants --mutations (dogfood §3) executes that registry.',
    'scripts/generate-iana-network-table':
      'extensionless python generator for the frozen IANA table in crates/security/src/network.rs; invoked by scripts/update-iana-special-registry.sh (operator refresh) and the generated table is exercised by the security crate tests under §1 cargo test --workspace.',
    'scripts/dogfood.sh':
      'executor root of the DOGFOOD program: it IS the run plan every other script is classified against, so it cannot invoke itself; it is run explicitly by operators and its registered gates run in the certificate/PR lanes through scripts/dogfood.mjs.',
  }),
);

// docs/*.md not linked from README.md / another docs/*.md.
const DOCS_ALLOWLIST = new Map(
  Object.entries({
    'docs/acquire.md':
      'normative commerce acquisition contract; cited from crates/cli (acquire_certification, tools_market) and tests/static-authority, not part of the architecture index.',
    'docs/ci-enforcement.md':
      'operator CI enforcement runbook; linked from scripts/woodpecker/setup.md rather than the architecture index.',
    'docs/ui-terminology.md':
      'normative Work/Inspect/History vocabulary law; enforced by the dogfood internal-identifier label rule below and by the panel wording; not yet linked from the architecture index.',
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
  // ------------------------------------------------------------------ T10
  {
    id: 'UIUX-T10-VSC',
    spec: 'T10 [L1072-1074] "Faktor needs your approval" / "Review the action, target and stated reason." / "Allow / Deny"',
    host: 'vscode',
    must: [
      ['apps/vscode/media/chat.js', 'Faktor needs your approval'],
      ['apps/vscode/media/chat.js', 'Review the action, target and stated reason.'],
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
    tests: { jetbrains: ['approval renderer: authority reason only when present, exact fallback + payload'] },
  },
  // ------------------------------------------------------------------ T22
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
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreePanel.kt', 'Not integrated'],
      ['apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/TaskTreePanel.kt', 'Landed state matches verified state'],
    ],
    tests: { jetbrains: ['verification summary renders VERIFIED only under the strict verified proof', 'overview keeps proof, spend and completed states honest'] },
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
    tests: { jetbrains: ['NO automatic daemon start'] },
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
  const world = {
    crates,
    invariants: parseInvariants(readIf(join(root, REGISTRY_FILE))),
    coverage,
    discovered: discover(root),
    specs: loadMutationSpecs(join(root, 'scripts/mutations')),
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
    if (/\.(sh|mjs|js|cjs|ps1|py)$/.test(path)) return true;
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
 * Cargo.toml`; fixture corpora nested deeper are benchmark inputs, not
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
    items: UIUX_CONTRACT.length,
    gate: 'UIUX_CONTRACT production needles per host file + host test-corpus pins (contract-uiux.md I01-I10/T00-T33/§6/§8/§9)',
    gaps: uiuxContractGaps(root),
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
  console.log('UI/UX rules: raw color, terminal argv, calm card headers, internal-identifier labels, visual identity pins, host-matrix state coverage, UI/UX contract pins');
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

  // The fixtures above prove the detectors work; this proves the current tree
  // has no gaps OUTSIDE the UI/UX contract table. The contract table is a
  // live gate with its own surface class: its findings are the working list
  // for the product fix lanes and --coverage always reports them, so they are
  // surfaced here as an explicit note (count + rows) instead of being
  // re-asserted per selftest run.
  const real = analyze(ROOT);
  const contractGaps = real.gaps.filter((gap) => gap.startsWith('uiux-contract-'));
  const otherGaps = real.gaps.filter((gap) => !gap.startsWith('uiux-contract-'));
  if (contractGaps.length > 0) {
    console.log(`selftest note: the real tree carries ${contractGaps.length} uiux-contract finding(s) (reported by --coverage):`);
    for (const gap of contractGaps) console.log(`selftest note:   - ${gap}`);
  }
  check(`the real tree reports 0 gate gaps outside the UI/UX contract table (${otherGaps.length})`, otherGaps.length === 0, otherGaps.slice(0, 10).join('; '));
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
