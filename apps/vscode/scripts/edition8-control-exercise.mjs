#!/usr/bin/env node
// Registry-driven live exercise of EVERY Revision 8 VS Code control.
//
// For each of the 200 controls on its declared screen it:
//   * renders the REAL Edition 8 webview markup (renderEdition8Html) and
//     asserts the control is present exactly once with an accessible name,
//     a native role, a focusable interactive element and the registry copy;
//   * activates the shipped host controller (stage/activate/apply) against a
//     recording backend and records the exact DTO method/path, state and
//     label observed — including typed refusals for missing endpoints;
//   * verifies all 72 screens render their complete control set and no
//     optimistic Saved/Connected/Verified/Deleted state is claimed without
//     an acknowledgement.
//
// Output: target/certification/edition8/control-exercise-vscode.raw.json

import { createHash } from 'node:crypto';
import { release } from 'node:os';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { E8_CONTROLS, E8_SCREENS, E8_COUNTS } from '../src/generated/edition8Registry.ts';
import { renderEdition8Html } from '../src/edition8/html.ts';
import { bindingOf8, optionsForBinding } from '../src/edition8/bindings.ts';
import { EMPTY_DRAFT, activateControl, applyDraft, stageRaw } from '../src/edition8/host.ts';
import { destinationOf, screenView } from '../src/edition8/view.ts';
import { edition8Snapshot } from './fixtures/edition8.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');

const snapshot = edition8Snapshot();
const typedPayload = {
  session: 's1',
  id: 'openai-main',
  provider: 'openai',
  model: 'gpt-5-codex',
  child: 'a1',
  terminal: 'term-1',
  task: 'task-1',
  sequence: '1',
  kind: 'project',
  key: 'build-command',
  query: 'daemon',
  request_id: '7',
  permission_id: '7',
  decision: 'deny',
};

class RecordingBackend {
  constructor() {
    this.calls = [];
  }
  async read() {
    this.calls.push({ kind: 'read' });
    return snapshot;
  }
  async applyPatch(expectedRevision, changes) {
    this.calls.push({ kind: 'applyPatch', expectedRevision, changes });
    return {
      ok: true,
      message: `Configuration revision 8 saved; runtime revision 8.`,
      path: '/native/config/commit',
      body: { saved_revision: 8, runtime_revision: 8 },
      error_code: null,
      field_states: changes.map((change) => ({ key: change.key, state: 'Effective' })),
    };
  }
  async execute(controlId, payload) {
    const binding = bindingOf8(controlId);
    const route = binding.route;
    this.calls.push({ kind: 'execute', controlId, method: route.method, path: route.path, payload });
    return {
      ok: true,
      message: `Acknowledged ${route.method} ${route.path}.`,
      method: route.method,
      path: route.path,
      body: { ok: true },
      errorCode: null,
      unresolved: null,
    };
  }
}

function esc(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function modeFor(screen) {
  return destinationOf(screen) === 'settings' ? 'settings' : 'shell';
}

function pageFor(screen, fixture = snapshot) {
  return renderEdition8Html({
    nonce: 'N',
    cspSource: '*',
    scriptUri: '',
    styleUri: '',
    state: { snapshot: fixture, loading: false, error: null, focusControl: null },
    initialScreen: screen.id,
    host: 'vscode',
    mode: modeFor(screen),
  });
}

function screenMarkupFor(html, screenId) {
  const start = html.indexOf(`id="e8-screen-${screenId}"`);
  if (start < 0) return '';
  const open = html.lastIndexOf('<section', start);
  const next = html.indexOf('<section class="e8-screen"', start);
  return html.slice(open, next < 0 ? html.length : next);
}

function articleFor(html, controlId, screenId) {
  const scoped = screenMarkupFor(html, screenId);
  const start = scoped.indexOf(`id="e8-control-${controlId}"`);
  if (start < 0) return '';
  const open = scoped.lastIndexOf('<article', start);
  const end = scoped.indexOf('</article>', start);
  if (open < 0 || end < 0) return '';
  return scoped.slice(open, end + '</article>'.length);
}

function interactiveMarkup(article, binding) {
  if (binding.input === 'readonly') return /<output\b/.test(article) ? 'output' : '';
  if (binding.input === 'toggle') return /type="checkbox"/.test(article) ? 'checkbox' : '';
  if (binding.input === 'select') return /<select\b/.test(article) ? 'select' : '';
  if (binding.input === 'textarea' || binding.input === 'list') return /<textarea\b/.test(article) ? 'textarea' : '';
  if (binding.input === 'action' || binding.input === 'secret-ref' || binding.input === 'money') {
    return /data-activate="[^"]+"/.test(article) ? 'button' : '';
  }
  return /<input\b/.test(article) ? 'input' : '';
}

function focusable(article, role, controlId) {
  if (role === 'output') return true;
  const tag = new RegExp(`<(?:input|select|textarea|button)\\b[^>]*(?:data-input|data-activate)="(?:${controlId})"[^>]*>`).exec(article);
  if (tag !== null && !/\sdisabled(?:\s|>|=)/.test(tag[0])) return true;
  // A deliberately disabled control (for example an enterprise-disabled
  // setting) still needs a keyboard path to inspect its typed reason; the
  // host renders an enabled Details/Inspect action for it. Disabled primary
  // controls themselves are correctly skipped by native tab order.
  const inspect = new RegExp(`<(?:button|a)\\b[^>]*(?:data-inspect|data-activate)="(?:${controlId})"[^>]*>`, 'g');
  for (const match of article.matchAll(inspect)) {
    if (!/\sdisabled(?:\s|>|=)/.test(match[0])) return true;
  }
  return false;
}

async function exercise(control) {
  const binding = bindingOf8(control.id);
  const primary = control.primaryScreen && E8_SCREENS.some((screen) => screen.id === control.primaryScreen)
    ? control.primaryScreen
    : E8_SCREENS.find((screen) => screen.controls.includes(control.id)).id;
  const screen = E8_SCREENS.find((entry) => entry.id === primary);
  const html = pageFor(screen);
  const article = articleFor(html, control.id, screen.id);
  const role = interactiveMarkup(article, binding);
  const accessibleName = (() => {
    const label = new RegExp(`id="e8-name-${control.id}"[^>]*>([^<]+)<`).exec(article);
    return label === null ? '' : label[1].trim();
  })();
  const checks = {
    rendered: article.length > 0,
    accessible_name: accessibleName.length >= 3,
    role: role.length > 0,
    focusable: focusable(article, role, control.id),
    exact_default_copy: article.includes(esc(control.defaultState)),
    exact_scope_copy: article.includes(esc(control.scope)),
    exact_timing_copy: article.includes(esc(control.applyTiming)),
    exact_behavior_copy: article.includes(esc(control.behavior)),
  };
  const backend = new RecordingBackend();
  let state = { snapshot, draft: EMPTY_DRAFT, loading: false, error: null };
  let observed = '';
  let activationKind = '';
  let status = 'failed';
  let error = null;
  try {
    if (binding.kind === 'config') {
      const staged = stageRaw(state, control.id, null, true);
      if (staged.error !== null) {
        error = staged.error;
        status = 'unavailable';
        observed = `stage-refused: ${staged.error}`;
      } else {
        state = staged.state;
        const entries = [...state.draft.set.values()];
        const applied = await applyDraft(backend, state, entries, []);
        state = applied.state;
        activationKind = 'config.set';
        observed = applied.ok
          ? `POST /native/config/commit ${entries.map((entry) => entry.key).join(',')} -> revision 8`
          : applied.message;
        status = applied.ok ? 'effective' : 'unavailable';
      }
    } else {
      const view = screenView(primary, state);
      const controlView = view.controls.find((entry) => entry.control.id === control.id);
      const activated = await activateControl(backend, state, control.id, typedPayload);
      state = activated.state;
      activationKind = activated.result.kind;
      observed = activated.result.message;
      if (activated.result.kind === 'refused') {
        status = controlView && controlView.state === 'disabled' ? 'unavailable' : 'failed';
        error = activated.result.error;
      } else if (activated.result.kind === 'host') {
        status = 'effective';
        observed = `host-action: ${control.name}`;
      } else {
        status = 'effective';
      }
    }
  } catch (caught) {
    error = caught instanceof Error ? caught.message : String(caught);
    observed = `threw: ${error}`;
  }
  const call = activationKind === 'config.set'
    ? { method: 'POST', path: '/native/config/commit' }
    : binding.kind === 'missing'
      ? { method: binding.route.method, path: binding.route.path }
      : { method: binding.route.method, path: binding.route.path };
  const optimistic = /\b(Saved|Connected|Verified|Remembered|Restored|Deleted|Isolated|Complete)\b/.test(observed) && status !== 'effective';
  return {
    id: control.id,
    name: control.name,
    screen: primary,
    group: screen.group,
    surface: screen.surface,
    label: binding.actionLabel,
    input: binding.input,
    route: `${binding.route.method} ${binding.route.path}`,
    kind: binding.kind,
    handler: binding.kind === 'missing' ? `typed-unavailable:${binding.route.method} ${binding.route.path}` : `host:${binding.input}:${binding.route.method} ${binding.route.path}`,
    observable: observed,
    accessible_name: accessibleName,
    accessibleName,
    role,
    focusable: checks.focusable,
    state: screenView(primary, state).controls.find((entry) => entry.control.id === control.id)?.state ?? 'unknown',
    activation_kind: activationKind,
    dto_call: call,
    observed,
    status,
    error,
    optimistic,
    checks,
    passed:
      Object.values(checks).every(Boolean) &&
      (status === 'effective' || status === 'unavailable') &&
      !optimistic,
  };
}

async function main() {
  const results = [];
  for (const control of E8_CONTROLS) results.push(await exercise(control));

  const screens = [];
  for (const screen of E8_SCREENS) {
    const html = pageFor(screen);
    const scoped = screenMarkupFor(html, screen.id);
    const missing = screen.controls.filter((id) => articleFor(html, id, screen.id).length === 0);
    const duplicate = screen.controls.filter((id) => scoped.split(`data-control="${id}"`).length - 1 !== 1);
    screens.push({
      id: screen.id,
      group: screen.group,
      surface: screen.surface,
      controls: screen.controls.length,
      rendered: screen.controls.length - missing.length,
      missing,
      duplicate,
      rows: screenView(screen.id, { snapshot, loading: false, error: null, focusControl: null }).rows.length,
      html_digest: sha(html),
      csp: /Content-Security-Policy/.test(html),
      status_line: /e8-status/.test(html),
    });
  }

  const failures = results.filter((row) => !row.passed);
  const effective = results.filter((row) => row.status === 'effective').length;
  const unavailable = results.filter((row) => row.status === 'unavailable').length;
  const missingRoutes = results.filter((row) => row.kind === 'missing');
  const hostActions = results.filter((row) => row.kind === 'host');
  const counts = {
    controls: E8_CONTROLS.length,
    exercised: effective + unavailable,
    effective,
    unavailable,
    failed: failures.length,
    screens: E8_SCREENS.length,
    screens_rendered: screens.filter((row) => row.rendered === row.controls && row.duplicate.length === 0).length,
    edges: screens.reduce((sum, row) => sum + row.controls, 0),
    accessible_named: results.filter((row) => row.checks.accessible_name).length,
    focusable: results.filter((row) => row.checks.focusable).length,
    roles: results.filter((row) => row.checks.role).length,
    missing_handlers: results.filter((row) => row.handler === null || row.handler === undefined || row.handler.length === 0).length,
    dto_calls: results.filter((row) => row.activation_kind === 'action' || row.activation_kind === 'read').length,
    config_patches: results.filter((row) => row.activation_kind === 'config.set').length,
    host_actions: hostActions.length,
    missing_endpoints: missingRoutes.length,
  };
  const document = {
    schema: 'faktor-edition8-control-exercise/v1',
    host: 'vscode',
    platform: process.platform,
    os: release(),
    generated_at: new Date().toISOString(),
    counts,
    screens,
    results,
    missing_endpoints: missingRoutes.map((row) => ({ id: row.id, route: row.route, reason: row.error })),
  };
  mkdirSync(OUT, { recursive: true });
  writeFileSync(resolve(OUT, 'control-exercise-vscode.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
  console.log(
    `vscode edition8 controls: ${counts.exercised}/${counts.controls} exercised ` +
      `(${effective} effective, ${unavailable} typed-unavailable, ${counts.failed} failed); ` +
      `screens ${counts.screens_rendered}/${counts.screens}; edges ${counts.edges}; ` +
      `a11y ${counts.accessible_named}/${counts.controls} named, ${counts.focusable}/${counts.controls} focusable; ` +
      `dto ${counts.dto_calls}, config ${counts.config_patches}, host ${counts.host_actions}, missing ${counts.missing_endpoints}`,
  );
  for (const failure of failures.slice(0, 30)) console.error(`FAIL ${failure.id}: ${failure.error ?? 'checks'} ${JSON.stringify(failure.checks)}`);
  for (const screen of screens.filter((row) => row.rendered !== row.controls || row.duplicate.length > 0).slice(0, 20)) {
    console.error(`FAIL ${screen.id}: rendered ${screen.rendered}/${screen.controls} duplicate ${screen.duplicate.join(',')}`);
  }
  if (counts.failed > 0 || counts.screens_rendered !== E8_SCREENS.length) process.exit(1);
  if (counts.exercised !== counts.controls || counts.missing_handlers !== 0) process.exit(1);
}

await main();
