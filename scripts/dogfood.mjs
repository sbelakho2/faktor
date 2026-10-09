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
//   scripts            every scripts/**/*.{sh,mjs,js,cjs,ps1,py} must be
//                      invoked by the dogfood run plan (scripts/dogfood.sh),
//                      reachable from a planned script (text reference or
//                      relative import), or allowlisted HERE with a non-empty
//                      reason. Stale allowlist entries are gaps too.
//   docs               every docs/*.md must be referenced from README.md or
//                      another docs/*.md (the docs index), or allowlisted.
//   CI workflows       .woodpecker/**/*.yaml are gated by the workflow-graph
//                      and image-pin checkers registered in dogfood.sh.
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
  const file = join(root, 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsHostMatrixSmoke.kt');
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

function panelGaps(root) {
  const gaps = [];
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
    if (!claimed.has(entry)) {
      gaps.push(`stale-host-matrix-entry: the host matrix renders "${entry}" but no panel class or chat tab claims it`);
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
    gaps: panelGaps(root),
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
  console.log('UI/UX rules: raw color, terminal argv, calm card headers, internal-identifier labels');
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

  // The fixtures above prove the detectors work; this proves the current tree
  // has no gaps.
  const real = analyze(ROOT);
  check(`the real tree reports 0 gate gaps (${real.gaps.length})`, real.gaps.length === 0, real.gaps.slice(0, 10).join('; '));
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
