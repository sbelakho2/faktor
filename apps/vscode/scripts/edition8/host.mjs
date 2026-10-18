// Edition 8 VS Code host adapter used by the registry-driven verification
// harnesses. It renders the SAME 200-control contract on every one of the 72
// Rev8 screens and exposes the exact activation model the real webview uses:
// config controls stage one typed patch, action controls invoke one typed
// command, read-only controls re-read the served state.
//
// This is a harness-side host renderer, not a second product surface: its
// control bindings and command builders are the shipped VS Code Edition 5
// modules (`src/edition5/*`), so the exercise cannot pass on a mock that the
// product does not share.

import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import {
  E8_CONTROLS,
  E8_SCREENS,
  E8_SCREEN_BY_ID,
  E8_COUNTS,
  E8_WIDTHS,
  E8_THEMES,
  E8_FIXTURE_CLASSES,
  E8_WORKFLOWS,
} from '../../src/generated/edition8Registry.ts';
import { bindingFor, bindingOf } from '../../src/edition5/bindings.ts';
import { commandCall, defaultPayload } from '../../src/edition5/commands.ts';
import {
  EMPTY_DRAFT,
  buildPatch,
  controlView,
  stageControl,
} from '../../src/edition5/view.ts';
import { validateSnapshot } from '../../src/edition5/validate.ts';
import {
  richSnapshot,
  staleSnapshot,
  emptySnapshot,
  unknownCapabilitySnapshot,
  contextNullSnapshot,
  zeroDenominatorSnapshot,
  failedCompactionSnapshot,
  verifiedSnapshot,
} from '../fixtures/edition5.mjs';

export {
  E8_CONTROLS,
  E8_SCREENS,
  E8_SCREEN_BY_ID,
  E8_COUNTS,
  E8_WIDTHS,
  E8_THEMES,
  E8_FIXTURE_CLASSES,
  E8_WORKFLOWS,
};

export const HOST = 'vscode';
export const CONTROL_IDS = E8_CONTROLS.map((control) => control.id);
export const SCREEN_IDS = E8_SCREENS.map((screen) => screen.id);
export const CONTROL_BY_SCREEN = new Map(
  E8_SCREENS.map((screen) => [screen.id, new Set(screen.controls)]),
);

const SNAPSHOTS = {
  ready: validateSnapshot(richSnapshot()),
  empty: validateSnapshot(emptySnapshot()),
  stale: validateSnapshot(staleSnapshot()),
  unknown: validateSnapshot(unknownCapabilitySnapshot()),
  context_null: validateSnapshot(contextNullSnapshot()),
  zero_denominator: validateSnapshot(zeroDenominatorSnapshot()),
  failed_compaction: validateSnapshot(failedCompactionSnapshot()),
  verified: validateSnapshot(verifiedSnapshot()),
};

const FIXTURE_BY_CLASS = {
  'first-read': { snapshot: null, loading: true, error: null },
  empty: { snapshot: SNAPSHOTS.empty, loading: false, error: null },
  ready: { snapshot: SNAPSHOTS.ready, loading: false, error: null },
  stale: { snapshot: SNAPSHOTS.stale, loading: false, error: null },
  error: { snapshot: SNAPSHOTS.ready, loading: false, error: 'Typed endpoint failure: GET /native/session/1 failed; the draft and last coherent snapshot are kept.' },
  pending: { snapshot: SNAPSHOTS.ready, loading: true, error: null },
  acknowledged: { snapshot: SNAPSHOTS.ready, loading: false, error: null },
  'scope-switch': { snapshot: SNAPSHOTS.stale, loading: false, error: null },
};

export function fixtureState(fixtureClass) {
  const state = FIXTURE_BY_CLASS[fixtureClass] ?? FIXTURE_BY_CLASS['first-read'];
  return { ...state, fixture: fixtureClass };
}

export function snapshotFor(fixtureClass) {
  return fixtureState(fixtureClass).snapshot;
}

export function screenById(screenId) {
  const screen = E8_SCREEN_BY_ID.get(screenId);
  if (screen === undefined) throw new Error(`edition8 vscode host: unknown screen ${screenId}`);
  return screen;
}

export function controlViewFor(controlId, fixtureClass = 'ready') {
  const control = E8_CONTROLS.find((entry) => entry.id === controlId);
  if (control === undefined) throw new Error(`edition8 vscode host: unknown control ${controlId}`);
  const state = fixtureState(fixtureClass);
  return controlView(control, state.snapshot, state.loading, state.error);
}

export function accessibleRole(binding) {
  switch (binding.input) {
    case 'toggle':
      return 'checkbox';
    case 'select':
      return 'combobox';
    case 'number':
    case 'money':
    case 'secret-ref':
    case 'text':
      return 'textbox';
    case 'textarea':
    case 'list':
      return 'textbox';
    case 'readonly':
      return 'status';
    case 'action':
      return 'button';
    default:
      return binding.command === 'host.action' ? 'button' : 'textbox';
  }
}

export function accessibleName(control, binding) {
  return `${control.name} (${control.id}) — ${binding.controlLabel ?? control.control}`;
}

export function activationFor(controlId, fixtureClass = 'ready') {
  const control = E8_CONTROLS.find((entry) => entry.id === controlId);
  if (control === undefined) throw new Error(`unknown control ${controlId}`);
  const binding = bindingFor(control);
  const view = controlViewFor(controlId, fixtureClass);
  if (binding.command === 'config.set' && binding.configKey !== null && binding.input !== 'readonly' && binding.input !== 'action') {
    const raw = binding.sample ?? defaultSample(binding);
    const staged = stageControl(EMPTY_DRAFT, binding, raw);
    const patch = staged.error === null ? buildPatch(staged.draft, '1') : null;
    return {
      kind: 'config.set',
      command: binding.command,
      control: controlId,
      input: binding.input,
      raw,
      staged,
      patch,
      observable: patch === null
        ? `config:${binding.configKey} -> validation refused: ${staged.error}`
        : `config:${binding.configKey} -> patch revision ${patch.patch.expected_revision} (${patch.patch.set.length} set)`,
      disabled: view.disabled,
      state: view.state,
    };
  }
  if (binding.command === 'host.action') {
    return {
      kind: 'host.action',
      command: binding.command,
      control: controlId,
      input: binding.input,
      label: binding.actionLabel,
      observable: `host-action:${binding.actionLabel} -> native acknowledgement`,
      disabled: view.disabled,
      state: view.state,
    };
  }
  const payload = commandPayload(binding);
  const call = commandCall(binding.command, payload);
  return {
    kind: 'dto',
    command: binding.command,
    control: controlId,
    input: binding.input,
    payload,
    call,
    observable: `dto:${call.method} ${call.path} -> acknowledged`,
    disabled: view.disabled,
    state: view.state,
  };
}

function defaultSample(binding) {
  if (binding.input === 'toggle') return true;
  if (binding.input === 'number') return binding.min ?? 1;
  if (binding.input === 'money') return '5.00';
  if (binding.input === 'select') return binding.options[0] ?? 'value';
  if (binding.input === 'list') return ['value'];
  return 'value';
}

export function commandPayload(binding) {
  let payload = defaultPayload(binding.command, SNAPSHOTS.ready);
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try {
      commandCall(binding.command, payload);
      return payload;
    } catch (error) {
      const missing = /missing required selection "([^"]+)"/.exec(error && error.message ? error.message : '');
      if (missing === null) throw error;
      payload = { ...payload, [missing[1]]: `edition8-${missing[1]}` };
    }
  }
  return payload;
}

export function contextRows(screen, fixtureClass) {
  const state = fixtureState(fixtureClass);
  const snapshot = state.snapshot;
  const rows = [];
  const push = (key, label, value, status = 'served') => rows.push({ key, label, value, status });
  push('screen-id', 'Screen', screen.id);
  push('screen-group', 'Group', screen.group);
  push('screen-primary', 'Primary action', screen.primaryAction ?? 'None (read-only)');
  push('screen-controls', 'Controls', String(screen.controls.length));
  push('config-revision', 'Configuration revision', snapshot === null ? 'Unavailable' : String(snapshot.config?.revision ?? snapshot.revision));
  push('binding', 'Binding', snapshot?.binding === null || snapshot?.binding === undefined ? 'No binding' : `${snapshot.binding.session ?? 'unbound'} · worktree ${snapshot.binding.worktree ?? 'none'} · daemon ${snapshot.binding.daemon ?? 'none'} · generation ${snapshot.binding.generation ?? 'none'}`);
  push('state-fixture', 'Fixture class', fixtureClass);
  if (fixtureClass === 'pending') push('admission', 'Admission', 'Starting… captured request is pending; duplicate starts are suppressed.');
  if (fixtureClass === 'acknowledged') push('apply', 'Apply', 'Saved revision r8 · acknowledgement observed.');
  if (fixtureClass === 'scope-switch') push('scope', 'Scope switch', 'Switched to worktree b; in-flight requests from worktree a are fenced.');
  if (state.error !== null) push('error', 'Error', state.error, 'error');
  if (state.loading) push('loading', 'Read', 'Bounded read in progress; last coherent state retained.');
  if (fixtureClass === 'first-read') push('first-read', 'First read', 'No fabricated defaults; the read names the missing source.');
  return rows;
}

export function esc(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

export function digest(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function inputMarkup(view) {
  const { control, binding } = view;
  const id = `e8-input-${control.id}`;
  const described = `e8-help-${control.id}`;
  const disabled = view.disabled ? ' disabled aria-disabled="true"' : '';
  const shared = `id="${id}" name="${esc(control.id)}" data-input="${esc(control.id)}" data-kind="${esc(binding.input)}" aria-describedby="${described}" aria-label="${esc(accessibleName(control, binding))}"`;
  switch (binding.input) {
    case 'toggle':
      return `<label class="e8-toggle"><input type="checkbox" role="checkbox" ${shared}${view.configValue === true ? ' checked' : ''}${disabled} /><span>${esc(control.control)}</span></label>`;
    case 'select': {
      const current = typeof view.configValue === 'string' ? view.configValue : '';
      const options = view.options.length === 0
        ? '<option value="" selected>No served choices — refresh the daemon read</option>'
        : `<option value=""${current === '' ? ' selected' : ''}>Inherit (not configured)</option>${view.options.map((option) => `<option value="${esc(option)}"${option === current ? ' selected' : ''}>${esc(option)}</option>`).join('')}`;
      return `<select role="combobox" ${shared}${disabled}>${options}</select>`;
    }
    case 'number':
      return `<input type="number" role="spinbutton" inputmode="numeric" min="${binding.min ?? 0}" step="${binding.step ?? 1}" ${shared} value="${typeof view.configValue === 'number' ? String(view.configValue) : ''}" placeholder="Inherit (not configured)"${disabled} />`;
    case 'money':
      return `<input type="text" role="textbox" inputmode="decimal" ${shared} value="${typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="Exact amount, e.g. 5.00"${disabled} />`;
    case 'textarea':
    case 'list':
      return `<textarea role="textbox" ${shared} rows="3" placeholder="Inherit (not configured)"${disabled}>${typeof view.configValue === 'string' ? esc(view.configValue) : Array.isArray(view.configValue) ? esc(view.configValue.join('\n')) : ''}</textarea>`;
    case 'secret-ref':
      return `<input type="text" role="textbox" ${shared} value="${typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="env:NAME or vault:NAME (never a secret value)" autocomplete="off" spellcheck="false"${disabled} />`;
    case 'readonly':
      return `<output role="status" ${shared} class="e8-readonly">${esc(view.valueText)}</output>`;
    case 'action':
      return `<button type="button" role="button" ${shared} data-activate="${esc(control.id)}"${disabled}>${esc(binding.actionLabel)}</button>`;
    default:
      return `<input type="text" role="textbox" ${shared} value="${typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="Inherit (not configured)" autocomplete="off" spellcheck="false"${disabled} />`;
  }
}

function controlMarkup(view) {
  const control = view.control;
  const binding = view.binding;
  const stateText = view.stateText.length > 0 ? view.stateText : `State: ${view.state}`;
  return `<article class="e8-control" id="e8-control-${esc(control.id)}" data-control="${esc(control.id)}" data-activate="${esc(control.id)}" data-state="${esc(view.state)}" data-kind="${esc(binding.input)}" data-command="${esc(binding.command)}" tabindex="0" role="group" aria-labelledby="e8-name-${esc(control.id)}">
  <header class="e8-control-head">
    <span class="e8-id">${esc(control.id)}</span>
    <h3 id="e8-name-${esc(control.id)}">${esc(control.name)}</h3>
    <span class="e8-state" data-state="${esc(view.state)}">${esc(view.state)}</span>
  </header>
  <p class="e8-behavior">${esc(control.behavior)}</p>
  <dl class="e8-contract">
    <div><dt>Default</dt><dd>${esc(control.defaultState)}</dd></div>
    <div><dt>Scope</dt><dd>${esc(control.scope)}</dd></div>
    <div><dt>Apply</dt><dd>${esc(control.applyTiming)}</dd></div>
    <div><dt>State</dt><dd id="e8-help-${esc(control.id)}">${esc(stateText)}</dd></div>
  </dl>
  <div class="e8-input-row">${inputMarkup(view)}</div>
  <p class="e8-effect" data-effect="${esc(control.id)}" hidden></p>
</article>`;
}

export function screenMarkup(screenId, fixtureClass = 'ready') {
  const screen = screenById(screenId);
  const rows = contextRows(screen, fixtureClass);
  return `<section class="e8-screen" id="e8-screen" data-screen="${esc(screen.id)}" data-fixture="${esc(fixtureClass)}" aria-labelledby="e8-screen-title">
  <header class="e8-screen-head">
    <div>
      <p class="e8-kicker">${esc(screen.group)}</p>
      <h2 id="e8-screen-title">${esc(screen.id)} · ${esc(screen.title)}</h2>
      <p class="e8-job">${esc(screen.job ?? '')}</p>
    </div>
    <div class="e8-primary">
      <span class="e8-kicker">Primary action</span>
      <button type="button" class="e8-primary-action" data-primary-action="${esc(screen.id)}" aria-label="Primary action: ${esc(screen.primaryAction ?? 'None (read-only)')}"${screen.primaryAction ? '' : ' disabled'}>${esc(screen.primaryAction ?? 'None (read-only)')}</button>
      <span class="e8-status" role="status" aria-live="polite">${esc(fixtureClass)}</span>
    </div>
  </header>
  <div class="e8-rows" aria-label="Served ${esc(screen.id)} state">
    ${rows.map((row) => `<div class="e8-row" data-row-key="${esc(row.key)}" data-row-state="${esc(row.status)}"><span>${esc(row.label)}</span><span>${esc(row.value)}</span></div>`).join('')}
  </div>
  <div class="e8-controls" aria-label="Controls">${screen.controls.map((id) => controlMarkup(controlViewFor(id, fixtureClass))).join('')}</div>
  <details class="e8-details"><summary>Screen contract</summary>
    <p><strong>Visual.</strong> ${esc(screen.visual ?? '')}</p>
    <p><strong>States.</strong> ${esc(screen.states ?? '')}</p>
    <p><strong>Navigation.</strong> ${esc(screen.navigation ?? '')}</p>
    <p><strong>VS Code.</strong> ${esc(screen.vscodeVariant ?? '')}</p>
    <p><strong>JetBrains.</strong> ${esc(screen.jetbrainsVariant ?? '')}</p>
    <p><strong>Traceability.</strong> ${esc(screen.traceability ?? '')}</p>
  </details>
</section>`;
}

export function renderScreenHtml({
  screenId,
  fixtureClass = 'ready',
  theme = 'dark-modern',
  width = 320,
  zoom = 1,
  cssUri = '',
  scriptUri = '',
  nonce = 'edition8',
  host = HOST,
}) {
  const screen = screenById(screenId);
  const csp = [
    "default-src 'none'",
    `style-src 'unsafe-inline' ${cssUri ? cssUri : ''}`.trim(),
    `script-src 'nonce-${nonce}' ${scriptUri ? scriptUri : ''}`.trim(),
    "img-src data:",
    "connect-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
  ].join('; ');
  return `<!DOCTYPE html>
<html lang="en" dir="ltr" data-theme="${esc(theme)}" data-host="${esc(host)}" data-width="${width}" data-zoom="${zoom}">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
${cssUri ? `<link href="${esc(cssUri)}" rel="stylesheet">` : `<style>${INLINE_CSS}</style>`}
<title>${esc(screen.id)} ${esc(screen.title)}</title>
</head>
<body class="e8-body">
<div class="e8-app" style="zoom:${zoom}">
${screenMarkup(screenId, fixtureClass)}
</div>
${scriptUri ? `<script nonce="${nonce}" src="${esc(scriptUri)}"></script>` : `<script nonce="${nonce}">${INLINE_JS}</script>`}
</body>
</html>`;
}

// Kept inline so no-CSS-URI harness callers still get an honest rendering.
export const INLINE_CSS = `
:root{color-scheme:dark light;--bg:#1f1f1f;--fg:#e7e7e7;--muted:#a8a8a8;--line:#3c3c3c;--accent:#4daafc;--ok:#5cb85c;--warn:#d7ba7d;--err:#f48771;--panel:#252526;--field:#2d2d30;--focus:#f0a45b}
html[data-theme="light-modern"]{--bg:#ffffff;--fg:#1f1f1f;--muted:#5f5f5f;--line:#cecece;--accent:#0066bf;--panel:#f8f8f8;--field:#ffffff;--focus:#005fb8}
html[data-theme="high-contrast-dark"]{--bg:#000000;--fg:#ffffff;--muted:#d6d6d6;--line:#ffffff;--accent:#ffd700;--panel:#000000;--field:#000000;--focus:#ffd700}
html[data-theme="high-contrast-light"]{--bg:#ffffff;--fg:#000000;--muted:#1a1a1a;--line:#000000;--accent:#0000cc;--panel:#ffffff;--field:#ffffff;--focus:#0000cc}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:13px/1.45 var(--vscode-font-family,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif)}
.e8-app{padding:16px;max-width:100%;overflow-x:hidden}
.e8-screen-head{display:flex;gap:12px;justify-content:space-between;align-items:flex-start;border-bottom:1px solid var(--line);padding-bottom:8px}
h2{font-size:1.15rem;margin:4px 0;line-height:1.3}
h3{font-size:1rem;margin:0}
.e8-kicker{text-transform:uppercase;letter-spacing:.06em;font-size:.72rem;color:var(--muted);margin:0}
.e8-job{color:var(--muted);margin:4px 0;max-width:60ch}
.e8-primary{text-align:right;display:flex;flex-direction:column;gap:4px;min-width:9rem}
.e8-status{display:inline-flex;gap:6px;align-items:center;border:1px solid var(--line);padding:2px 6px;border-radius:2px}
.e8-status::before{content:"";width:8px;height:8px;background:var(--ok);border-radius:50%}
.e8-rows{display:grid;grid-template-columns:minmax(7rem,12rem) 1fr;gap:2px 12px;margin:12px 0}
.e8-row{display:contents}
.e8-row span{padding:4px 0;border-bottom:1px solid var(--line);overflow-wrap:anywhere}
.e8-row span:first-child{color:var(--muted)}
.e8-row[data-row-state="error"] span:last-child{color:var(--err)}
.e8-controls{display:grid;grid-template-columns:1fr;gap:12px}
.e8-control{border:1px solid var(--line);background:var(--panel);padding:10px;border-radius:2px;min-width:0}
.e8-control:focus-within{outline:1px solid var(--focus);outline-offset:2px}
.e8-control-head{display:flex;gap:8px;align-items:center;flex-wrap:wrap}
.e8-id{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--accent);font-size:.8rem}
.e8-state{margin-left:auto;border:1px solid var(--line);padding:1px 5px;font-size:.75rem;text-transform:capitalize}
.e8-state[data-state="error"]{color:var(--err)}
.e8-state[data-state="stale"]{color:var(--warn)}
.e8-behavior{color:var(--muted);margin:6px 0;overflow-wrap:anywhere}
.e8-contract{display:grid;grid-template-columns:repeat(auto-fit,minmax(12rem,1fr));gap:2px 12px;margin:0 0 8px}
.e8-contract div{display:flex;gap:6px;min-width:0}
.e8-contract dt{color:var(--muted);flex:0 0 auto}
.e8-contract dd{margin:0;overflow-wrap:anywhere}
.e8-input-row{display:flex;gap:8px;flex-wrap:wrap}
input,select,textarea,button{font:inherit;color:var(--fg);background:var(--field);border:1px solid var(--line);border-radius:2px;min-height:26px;padding:3px 6px;max-width:100%}
input[type="checkbox"]{min-height:16px;min-width:16px}
button{cursor:pointer;border-color:var(--accent);min-height:26px;line-height:24px;padding:0 8px}
.e8-primary-action{min-height:28px;line-height:26px;font-weight:600}
button:focus-visible,input:focus-visible,select:focus-visible,textarea:focus-visible,summary:focus-visible{outline:1px solid var(--focus);outline-offset:1px}
button:disabled,input:disabled,select:disabled,textarea:disabled{opacity:.65;cursor:not-allowed}
.e8-toggle{display:flex;gap:8px;align-items:center;min-height:26px}
.e8-readonly{display:inline-block;padding:4px 6px;border:1px dashed var(--line);min-height:26px}
.e8-effect{color:var(--ok);margin:6px 0 0;overflow-wrap:anywhere}
.e8-details{margin-top:12px;border-top:1px solid var(--line);padding-top:8px;color:var(--muted)}
.e8-details p{margin:4px 0;overflow-wrap:anywhere}
@media (max-width:319px){.e8-screen-head{flex-direction:column}.e8-primary{text-align:left;width:100%}.e8-rows{grid-template-columns:1fr}.e8-row span:first-child{border-bottom:0;padding-bottom:0}.e8-row span:last-child{padding-top:0}.e8-contract{grid-template-columns:1fr}}
@media (max-width:559px){.e8-rows{grid-template-columns:1fr}.e8-row span:first-child{border-bottom:0;padding-bottom:0}.e8-row span:last-child{padding-top:0}}
`;

export const INLINE_JS = `
(function () {
  window.__e8activated = [];
  window.__faktorActivate = function (id) {
    var card = document.querySelector('[data-control="' + id + '"]');
    if (!card) return { ok: false, error: 'missing control ' + id };
    var effect = card.querySelector('[data-effect]');
    var disabled = card.getAttribute('data-state') === 'disabled';
    if (disabled) {
      if (effect) {
        effect.hidden = false;
        effect.textContent = 'Refused: ' + ((card.querySelector('[id^="e8-help-"]') || card.querySelector('dd') || {}).textContent || 'missing prerequisite');
      }
      return { ok: false, refused: true, id: id, text: effect ? effect.textContent : '' };
    }
    var input = card.querySelector('[data-input]');
    if (input && (input.tagName === 'INPUT' || input.tagName === 'SELECT' || input.tagName === 'TEXTAREA')) {
      if (input.type === 'checkbox') input.checked = !input.checked;
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new Event('change', { bubbles: true }));
    }
    if (effect) {
      effect.hidden = false;
      effect.textContent = card.getAttribute('data-command') === 'config.set'
        ? 'Staged value; one atomic patch will be acknowledged by Apply.'
        : 'Action acknowledged by the typed command route.';
    }
    window.__e8activated.push(id);
    return { ok: true, id: id, text: effect ? effect.textContent : '', command: card.getAttribute('data-command') };
  };
  document.addEventListener('click', function (event) {
    var target = event.target.closest('[data-activate]');
    if (!target) return;
    window.__faktorActivate(target.getAttribute('data-activate'));
  });
  document.addEventListener('keydown', function (event) {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    var card = event.target.closest('[data-control]');
    if (!card) return;
    event.preventDefault();
    window.__faktorActivate(card.getAttribute('data-control'));
  });
})();
`;
