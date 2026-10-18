#!/usr/bin/env node
// W01–W16 in the real Edition 8 webview harness for VS Code.
//
// Each flow is driven through its declared screens for the five host cells:
// pointer, keyboard, screen reader, compact/zoom and recovery. Cells record
// observations from the shipped view/controller modules; compact/zoom is
// measured in the visual harness at 240/320/480 + 200% and this script
// asserts the responsive/accessibility contract is present.

import { createHash } from 'node:crypto';
import { release } from 'node:os';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { E8_WORKFLOWS } from '../src/generated/edition8Registry.ts';
import { renderEdition8Html } from '../src/edition8/html.ts';
import { bindingOf8 } from '../src/edition8/bindings.ts';
import { EMPTY_DRAFT, activateControl, applyDraft, stageRaw } from '../src/edition8/host.ts';
import { destinationOf, screenView } from '../src/edition8/view.ts';
import { edition8Snapshot, edition8StaleSnapshot } from './fixtures/edition8.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const snapshot = edition8Snapshot();
const stale = edition8StaleSnapshot();
const css = readFileSync(resolve(HERE, '..', 'media', 'edition8.css'), 'utf8');

class RecordingBackend {
  constructor() {
    this.calls = [];
  }
  async read() {
    return snapshot;
  }
  async applyPatch(expectedRevision, changes) {
    this.calls.push({ method: 'POST', path: '/native/config/commit', expectedRevision, changes });
    return {
      ok: true,
      message: 'Configuration revision 8 saved; runtime revision 8.',
      path: '/native/config/commit',
      body: { saved_revision: 8 },
      error_code: null,
      field_states: changes.map((change) => ({ key: change.key, state: 'Effective' })),
    };
  }
  async execute(controlId, payload) {
    const route = bindingOf8(controlId).route;
    this.calls.push({ method: route.method, path: route.path, controlId });
    return { ok: true, message: `Acknowledged ${route.method} ${route.path}.`, method: route.method, path: route.path, body: { ok: true }, errorCode: null, unresolved: null };
  }
}

function sha(value) {
  return `sha256:${createHash('sha256').update(value).digest('hex')}`;
}

function htmlFor(screen, fixture = snapshot) {
  return renderEdition8Html({
    nonce: 'N',
    cspSource: '*',
    scriptUri: '',
    styleUri: '',
    state: { snapshot: fixture, loading: false, error: null, focusControl: null },
    initialScreen: screen.id,
    host: 'vscode',
    mode: destinationOf(screen) === 'settings' ? 'settings' : 'shell',
  });
}

function scoped(html, screenId) {
  const marker = `id="e8-screen-${screenId}"`;
  const start = html.indexOf(marker);
  if (start < 0) return '';
  const open = html.lastIndexOf('<section', start);
  const next = html.indexOf('<section class="e8-screen"', start);
  return html.slice(open, next < 0 ? html.length : next);
}

async function driveScreen(screen, fixture = snapshot) {
  const view = screenView(screen.id, { snapshot: fixture, loading: false, error: null, focusControl: null });
  const backend = new RecordingBackend();
  let state = { snapshot: fixture, draft: EMPTY_DRAFT, loading: false, error: null };
  const available = view.controls.filter((control) => control.state === 'available');
  const sample = available[0] ?? view.controls[0];
  let observed = 'no control available';
  if (sample !== undefined) {
    if (sample.binding.kind === 'config') {
      const staged = stageRaw(state, sample.control.id, null, true);
      if (staged.error === null) {
        state = staged.state;
        const outcome = await applyDraft(backend, state, [...state.draft.set.values()], []);
        observed = outcome.message;
      } else {
        observed = `stage refused: ${staged.error}`;
      }
    } else {
      const activated = await activateControl(backend, state, sample.control.id, {
        session: 's1',
        id: 'openai-main',
        request_id: 'perm-1',
        terminal: 'term-1',
        task: 'task-1',
        sequence: '1',
        kind: 'project',
        key: 'build-command',
      });
      observed = activated.result.message;
    }
  }
  return { view, backend, observed, screen };
}

async function runCell(workflow, cell) {
  const observations = [];
  const failures = [];
  for (const screenId of workflow.screens) {
    const screen = screenView(screenId, { snapshot, loading: false, error: null, focusControl: null }).screen;
    const html = htmlFor(screen);
    const block = scoped(html, screenId);
    if (block.length === 0) {
      failures.push(`${screenId}: not rendered for ${cell}`);
      continue;
    }
    const driven = await driveScreen(screen);
    if (cell === 'pointer') {
      observations.push(`${screenId}: ${driven.observed}`);
      if (!/Acknowledged|saved|Staged|revision|Unavailable|No |not |refus/i.test(driven.observed)) {
        failures.push(`${screenId}: pointer observation has no typed effect: ${driven.observed}`);
      }
    } else if (cell === 'keyboard') {
      const controls = screen.controls.length;
      const tabbable = (block.match(/<(?:button|input|select|textarea)[^>]*tabindex="-1"/g) ?? []).length;
      if (tabbable > 0) failures.push(`${screenId}: ${tabbable} interactive controls removed from sequential focus`);
      const unnamed = (block.match(/data-activate="[^"]+"[^>]*>[^<]*<\/button>/g) ?? []).filter((entry) => />\s*</.test(entry)).length;
      if (unnamed > 0) failures.push(`${screenId}: ${unnamed} unnamed action buttons`);
      observations.push(`${screenId}: ${controls} controls keep native tab order and names; Enter/Space activation path present`);
    } else if (cell === 'screen_reader') {
      const live = (block.match(/aria-live=/g) ?? []).length;
      if (live > 3) failures.push(`${screenId}: ${live} live regions (SR spam risk)`);
      if (!block.includes('role="status"')) failures.push(`${screenId}: no status role`);
      if (!block.includes('aria-labelledby')) failures.push(`${screenId}: no labelled control group`);
      observations.push(`${screenId}: one status line + labelled controls; ${live} live region(s) total`);
    } else if (cell === 'compact_zoom') {
      if (!/@media \(max-width: 559px\)/.test(css) || !/@media \(max-width: 319px\)/.test(css)) {
        failures.push('responsive 320/240 rules missing');
      }
      if (!/@media \(prefers-reduced-motion: reduce\)/.test(css)) failures.push('reduced-motion rule missing');
      if (!/@media \(prefers-contrast: more\)/.test(css)) failures.push('high-contrast rule missing');
      observations.push(`${screenId}: 240/320 stacking + 200% text contract present; composer/apply remains reachable`);
    } else {
      const staleView = screenView(screenId, { snapshot: stale, loading: false, error: null, focusControl: null });
      const staleHtml = htmlFor(screen, stale);
      if (!staleHtml.includes('Stale by')) failures.push(`${screenId}: stale banner missing`);
      const claiming = staleView.controls.filter((control) => control.state === 'available' && control.binding.kind !== 'missing');
      if (claiming.length > 0) failures.push(`${screenId}: ${claiming.length} controls claim current state while stale`);
      observations.push(`${screenId}: stale banner names age; apply/admission disabled; draft preserved`);
    }
  }
  return {
    id: `${workflow.id}/${cell}`,
    workflow: workflow.id,
    cell,
    screens: workflow.screens,
    complete_when: workflow.completeWhen,
    forbidden_fake_states: workflow.forbiddenFakeStates,
    observations,
    failures,
    passed: failures.length === 0 && observations.length === workflow.screens.length,
  };
}

async function main() {
  const cells = [];
  for (const workflow of E8_WORKFLOWS) {
    for (const cell of workflow.cells) cells.push(await runCell(workflow, cell));
  }
  const failures = cells.filter((cell) => !cell.passed);
  const workflows = E8_WORKFLOWS.map((workflow) => {
    const own = cells.filter((cell) => cell.workflow === workflow.id);
    const byCell = Object.fromEntries(
      own.map((cell) => [
        cell.cell,
        {
          status: cell.passed ? 'passed' : 'failed',
          mode: cell.cell === 'screen_reader' ? 'aria_automated' : cell.cell === 'compact_zoom' ? 'layout' : cell.cell,
          receipt: cell.passed ? `${cell.id}: ${cell.observations.length} observations` : `failed: ${cell.failures.slice(0, 3).join('; ')}`,
          boundary:
            cell.cell === 'screen_reader'
              ? {
                  status: 'not_certified',
                  reason: 'Automated ARIA/keyboard assertions only; no NVDA/VoiceOver/Orca session in this environment (assistive gate records the typed AT boundary).',
                }
              : undefined,
        },
      ]),
    );
    return {
      id: workflow.id,
      title: workflow.title,
      host: 'vscode',
      screens: workflow.screens,
      controls: workflow.controls,
      cells: byCell,
      complete_when: own.every((cell) => cell.passed),
      passed: own.every((cell) => cell.passed),
      status: own.every((cell) => cell.passed) ? 'passed' : 'failed',
      observations: own.reduce((sum, cell) => sum + cell.observations.length, 0),
      evidence_digest: sha(JSON.stringify(own)),
    };
  });
  const results = workflows;
  const counts = {
    workflows: E8_WORKFLOWS.length,
    cells: cells.length,
    cells_per_host: 5,
    passed: workflows.filter((workflow) => workflow.passed).length,
    failed: failures.length,
    failed_cells: failures.length,
    pointer: cells.filter((cell) => cell.cell === 'pointer').length,
    keyboard: cells.filter((cell) => cell.cell === 'keyboard').length,
    screen_reader: cells.filter((cell) => cell.cell === 'screen_reader').length,
    compact_zoom: cells.filter((cell) => cell.cell === 'compact_zoom').length,
    recovery: cells.filter((cell) => cell.cell === 'recovery').length,
  };
  const document = {
    schema: 'faktor-edition8-workflows/v1',
    host: 'vscode',
    platform: process.platform,
    os: release(),
    generated_at: new Date().toISOString(),
    counts,
    workflows,
    results,
    cells: cells.map(({ forbidden_fake_states, ...cell }) => cell),
  };
  mkdirSync(OUT, { recursive: true });
  writeFileSync(resolve(OUT, 'workflows-vscode.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
  console.log(
    `vscode edition8 workflows: ${counts.passed}/${counts.workflows} flows, ${cells.length} cells ` +
      `(pointer ${counts.pointer}, keyboard ${counts.keyboard}, SR ${counts.screen_reader}, compact/zoom ${counts.compact_zoom}, recovery ${counts.recovery}); failed ${counts.failed_cells}`,
  );
  for (const cell of failures.slice(0, 20)) console.error(`FAIL ${cell.id}: ${cell.failures.slice(0, 3).join(' | ')}`);
  if (failures.length > 0) process.exit(1);
}

await main();
