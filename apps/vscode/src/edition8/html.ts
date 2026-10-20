// Server-side markup for the Revision 8 Work / Inspect / History shell and
// the editor-sized settings surfaces. Every screen, contract string, state
// and served row is rendered from the frozen registry and the typed read
// model. The webview script performs local interaction only (selection,
// staging, disclosure) and never invents a daemon value.

import { E8_SCREENS, type E8Control } from '../generated/edition8Registry.ts';
import {
  destinationOf,
  screenView,
  screensFor,
  statusGlyph,
  type E8ControlView,
  type E8Destination,
  type E8PanelState,
  type E8ScreenView,
} from './view.ts';

export interface Edition8HtmlOptions {
  readonly nonce: string;
  readonly cspSource: string;
  readonly scriptUri: string;
  readonly styleUri: string;
  readonly state: E8PanelState;
  readonly initialScreen: string;
  readonly host: 'vscode' | 'jetbrains';
  readonly mode?: 'shell' | 'settings';
}

function esc(value: string): string {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/// Serialize a value for an inline `<script>` body. `JSON.stringify` alone
/// leaves `<`, `>` and `&` literal, so a served provider/MCP/model/
/// diagnostics string containing `</script>` terminates the element and
/// injects markup into the panel (the nonce CSP blocks injected script
/// execution, but HTML/UI spoofing and link injection are still injection).
/// The U+2028/U+2029 escapes keep the result valid JavaScript.
export function scriptJson(value: unknown): string {
  return JSON.stringify(value)
    .replace(/</g, '\\u003c')
    .replace(/>/g, '\\u003e')
    .replace(/&/g, '\\u0026')
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029');
}

const DESTINATIONS: readonly { key: E8Destination; label: string; hint: string }[] = [
  { key: 'work', label: 'Work', hint: 'Intent, attention, transcript, composer and the live run' },
  { key: 'inspect', label: 'Inspect', hint: 'Plan, diff, verification, evidence and finish proof' },
  { key: 'history', label: 'History', hint: 'Durable conversations, Find and safe switching' },
  { key: 'settings', label: 'Settings & operations', hint: 'Models, context, tools, usage, settings and diagnostics' },
];

function destinationLabel(destination: E8Destination): string {
  return DESTINATIONS.find((entry) => entry.key === destination)?.label ?? destination;
}

function controlInput(control: E8Control, view: E8ControlView): string {
  const binding = view.binding;
  const id = `e8-input-${control.id}`;
  const disabled = view.disabled ? ' disabled' : '';
  const describedBy = `e8-desc-${control.id}`;
  const common = `id="${id}" data-input="${control.id}" data-kind="${binding.input}" aria-describedby="${describedBy}"`;
  switch (binding.input) {
    case 'toggle':
      return `<label class="e8-toggle"><input type="checkbox" ${common}${view.configValue === true ? ' checked' : ''}${disabled} /> <span>Enabled</span></label>`;
    case 'select': {
      const options = view.options;
      const current = typeof view.configValue === 'string' ? view.configValue : '';
      const optionMarkup =
        options.length === 0
          ? '<option value="" selected>No served choices — refresh the daemon read</option>'
          : `<option value=""${current === '' ? ' selected' : ''}>Inherit (not configured)</option>${options
              .map((option) => `<option value="${esc(option)}"${option === current ? ' selected' : ''}>${esc(option)}</option>`)
              .join('')}`;
      return `<select ${common}${disabled}>${optionMarkup}</select>`;
    }
    case 'number':
      return `<input type="number" inputmode="numeric" min="${binding.min ?? 0}" step="${binding.step ?? 1}" ${common} value="${view.configValue !== null && typeof view.configValue === 'number' ? String(view.configValue) : ''}" placeholder="Inherit (not configured)"${disabled} />`;
    case 'money':
      return `<input type="text" inputmode="decimal" ${common} value="${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="Exact amount, e.g. 5.00"${disabled} />`;
    case 'textarea':
      return `<textarea ${common} rows="3" placeholder="Inherit (not configured)"${disabled}>${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}</textarea>`;
    case 'list':
      return `<textarea ${common} rows="2" spellcheck="false" placeholder="One value per line"${disabled}>${Array.isArray(view.configValue) ? esc(view.configValue.map(String).join('\n')) : ''}</textarea>`;
    case 'secret-ref':
      return `<input type="text" ${common} value="${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="env:NAME or vault:NAME (never a secret value)" autocomplete="off" spellcheck="false"${disabled} />`;
    case 'readonly':
      return `<output ${common} class="e8-readonly">${esc(view.valueText)}</output>`;
    case 'action':
      return `<span ${common} class="e8-actionkind">Bounded action</span>`;
    default:
      return `<input type="text" ${common} value="${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="Inherit (not configured)" autocomplete="off" spellcheck="false"${disabled} />`;
  }
}

function controlMarkup(view: E8ControlView): string {
  const control = view.control;
  const binding = view.binding;
  const status = view.applyState === null ? '' : `<span class="e8-apply-state" data-apply-state="${esc(view.applyState)}">${esc(view.applyState)}</span>`;
  const disabledAction = view.disabled ? ' disabled aria-disabled="true"' : '';
  const missing = binding.kind === 'missing' ? ' data-missing="true"' : '';
  return `<article class="e8-control" id="e8-control-${control.id}" data-control="${control.id}" data-state="${view.state}" data-kind="${binding.input}" data-route="${esc(binding.route.method)} ${esc(binding.route.path)}"${missing} tabindex="-1" aria-labelledby="e8-name-${control.id}">
    <header class="e8-control-head">
      <span class="e8-control-id">${control.id}</span>
      <h3 id="e8-name-${control.id}" class="e8-control-name">${esc(control.name)}</h3>
      <span class="e8-control-kind">${esc(control.control)}</span>
      ${status}
      <span class="e8-state-badge" data-state="${view.state}">${esc(view.state)}</span>
    </header>
    <p class="e8-behavior">${esc(control.behavior)}</p>
    <dl class="e8-contract">
      <div><dt>Default</dt><dd>${esc(control.defaultState)}</dd></div>
      <div><dt>Scope</dt><dd>${esc(control.scope)}</dd></div>
      <div><dt>Apply timing</dt><dd>${esc(control.applyTiming)}</dd></div>
      <div><dt>Route</dt><dd class="e8-wrap">${esc(view.routeText)}</dd></div>
    </dl>
    <div class="e8-field">
      <label for="e8-input-${control.id}" class="e8-field-label">${binding.input === 'readonly' ? 'Served value' : binding.input === 'action' ? 'Action' : 'Value'}</label>
      ${controlInput(control, view)}
      ${binding.input === 'action' ? `<label for="e8-action-${control.id}" class="e8-field-label e8-action-label">Action value</label><textarea id="e8-action-${control.id}" data-action-input="${control.id}" rows="2" placeholder="Optional payload (text, query or JSON)"></textarea>` : ''}
      <p id="e8-desc-${control.id}" class="e8-help">${esc(view.valueText)}${view.source === null ? '' : ` · source ${esc(view.source)}`}</p>
    </div>
    <div class="e8-control-actions">
      <button type="button" class="e8-primary" data-activate="${control.id}"${disabledAction}>${esc(binding.actionLabel)}</button>
      <button type="button" class="e8-secondary" data-inspect="${control.id}">Details</button>
      <button type="button" class="e8-secondary" data-unstage="${control.id}"${binding.configKey === null ? ' disabled' : ''}>Reset this override</button>
    </div>
    <div class="e8-control-state" data-control-state="${control.id}">${esc(view.stateText)}</div>
    <details class="e8-states">
      <summary>Empty, error and disabled states</summary>
      <p><strong>Empty.</strong> ${esc(control.emptyState)}</p>
      <p><strong>Error.</strong> ${esc(control.errorState)}</p>
      <p><strong>Disabled.</strong> ${esc(control.disabledState)}</p>
      <p><strong>Loading.</strong> ${esc(control.loadingState)}</p>
      <p><strong>Stale.</strong> ${esc(control.staleState)}</p>
      <p><strong>Pending.</strong> ${esc(control.pendingState)}</p>
      <p class="e8-trace">Trace: ${esc(control.classification)} · ${esc(control.currentStatus)} · ${control.acceptanceIds.join(', ')} · spec p.${control.specPage}</p>
    </details>
  </article>`;
}

function groupControls(views: readonly E8ControlView[]): string {
  const groups = new Map<string, E8ControlView[]>();
  for (const view of views) {
    const list = groups.get(view.control.category) ?? [];
    list.push(view);
    groups.set(view.control.category, list);
  }
  return [...groups.entries()]
    .map(
      ([category, list]) => `<section class="e8-group" aria-label="${esc(category)}">
        <h3 class="e8-group-title">${esc(category)} <span class="e8-count">${list.length} controls</span></h3>
        <div class="e8-control-grid">${list.map(controlMarkup).join('\n')}</div>
      </section>`,
    )
    .join('\n');
}

function rowMarkup(row: E8ScreenView['rows'][number]): string {
  const open =
    row.openScreen === undefined ? '' : ` data-open-screen="${esc(row.openScreen)}" role="button" tabindex="0"`;
  return `<div class="e8-row" data-row-key="${esc(row.key)}" data-row-state="${row.state}"${open}>
    <span class="e8-row-label">${esc(row.label)}</span>
    <span class="e8-row-value" data-state="${row.state}">${esc(row.value)}</span>
  </div>`;
}

function surfaceNote(screen: E8ScreenView): string {
  const surface = screen.screen.surface;
  const notes: Record<string, string> = {
    sidebar: 'VS Code surface: Activity view webview.',
    settings: 'VS Code surface: editor-tab settings webview.',
    modal: 'VS Code surface: native modal message (host-ordered buttons).',
    quickpick: 'VS Code surface: native Quick Pick; this semantic card keeps the control contract inspectable.',
    'native-settings': 'VS Code surface: native Settings editor; persistent settings are never duplicated in a webview.',
    diff: 'VS Code surface: vscode.diff with immutable base/candidate titles (read-only).',
    terminal: 'VS Code surface: integrated terminal in the Panel; output stays bounded and is never redrawn here.',
    browser: 'VS Code surface: native Simple Browser; external page tokens are not imposed.',
    narrow: 'VS Code surface: 320 px sidebar reference; labels stack and the composer stays reachable.',
  };
  return notes[surface] ?? notes.sidebar;
}

function screenMarkup(screen: E8ScreenView, initial: boolean): string {
  const screenEntry = screen.screen;
  const showControls = screen.controls.length > 0;
  return `<section class="e8-screen" id="e8-screen-${screenEntry.id}" data-screen="${screenEntry.id}" data-group="${esc(screenEntry.group)}" data-destination="${destinationOf(screenEntry)}" data-surface="${screenEntry.surface}"${initial ? '' : ' hidden'} aria-labelledby="e8-screen-title-${screenEntry.id}">
    <header class="e8-screen-head">
      <div class="e8-kicker">${screenEntry.id} · ${esc(destinationLabel(destinationOf(screenEntry)))} · ${esc(screenEntry.group)}</div>
      <div class="e8-title-row">
        <h2 id="e8-screen-title-${screenEntry.id}">${esc(screenEntry.title)}</h2>
        <span class="e8-status" data-level="${screen.statusLevel}" role="status" aria-live="polite"><span aria-hidden="true">${statusGlyph(screen.statusLevel)}</span> ${esc(screen.status)}</span>
      </div>
      <p class="e8-purpose">${esc(screenEntry.job)}</p>
      <dl class="e8-screen-contract">
        <div><dt>Primary action</dt><dd>${esc(screenEntry.action)}</dd></div>
        <div><dt>Layout</dt><dd>${esc(screenEntry.layoutRegions)}</dd></div>
        <div><dt>Navigation</dt><dd>${esc(screenEntry.navigation)}</dd></div>
        <div><dt>States</dt><dd>${esc(screenEntry.states)}</dd></div>
      </dl>
      <details class="e8-plate"><summary>Host variant and traceability</summary><p>${esc(screenEntry.vscodeVariant)}</p><p class="e8-muted">${esc(screenEntry.jetbrainsVariant)}</p><p>${esc(surfaceNote(screen))}</p><p class="e8-trace">${esc(screenEntry.traceability)}</p></details>
    </header>
    ${screen.staleText === null ? '' : `<div class="e8-banner e8-warn" role="alert">${esc(screen.staleText)}</div>`}
    ${screen.emptyText === null ? '' : `<div class="e8-banner e8-empty" role="status">${esc(screen.emptyText)}</div>`}
    ${screen.rows.length === 0 ? '' : `<section class="e8-data" aria-label="Served ${esc(screenEntry.id)} state">${screen.rows.map(rowMarkup).join('\n')}</section>`}
    ${showControls ? groupControls(screen.controls) : ''}
  </section>`;
}

function nav(destination: E8Destination, initialScreen: string): string {
  const screens = screensFor(destination);
  return `<nav class="e8-screens" aria-label="${esc(destinationLabel(destination))} screens">
    ${screens
      .map(
        (screen) =>
          `<button type="button" class="e8-screen-link" data-screen-link="${screen.id}" aria-current="${screen.id === initialScreen ? 'page' : 'false'}">
            <span class="e8-screen-id">${screen.id}</span>
            <span class="e8-screen-name">${esc(screen.title)}</span>
          </button>`,
      )
      .join('\n')}
  </nav>`;
}

function destinationMarkup(destination: E8Destination, state: E8PanelState, initialScreen: string): string {
  const screens = screensFor(destination);
  const visible = screens.some((screen) => screen.id === initialScreen);
  return `<div class="e8-destination" data-destination-panel="${destination}"${visible ? '' : ' hidden'}>
    <div class="e8-destination-head">
      <h2>${esc(destinationLabel(destination))}</h2>
      <span class="e8-muted">${esc(DESTINATIONS.find((entry) => entry.key === destination)?.hint ?? '')}</span>
    </div>
    <div class="e8-split">
      ${nav(destination, initialScreen)}
      <div class="e8-screens-host">
        ${screens.map((screen) => screenMarkup(screenView(screen.id, state), screen.id === initialScreen && visible)).join('\n')}
      </div>
    </div>
  </div>`;
}

export function renderEdition8Html(options: Edition8HtmlOptions): string {
  const initial = E8_SCREENS.some((screen) => screen.id === options.initialScreen)
    ? options.initialScreen
    : 'S01';
  const mode = options.mode ?? (destinationOf(E8_SCREENS.find((screen) => screen.id === initial)!) === 'settings' ? 'settings' : 'shell');
  const destinations: readonly E8Destination[] =
    mode === 'settings' ? ['settings'] : ['work', 'inspect', 'history'];
  const status = options.state.snapshot === null ? 'Not loaded' : options.state.snapshot.stale ? 'Stale' : 'Ready';
  const statusLevel = options.state.snapshot === null ? 'quiet' : options.state.snapshot.stale ? 'waiting' : 'quiet';
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${esc(options.cspSource)} 'unsafe-inline'; script-src 'nonce-${options.nonce}'; img-src ${esc(options.cspSource)} data:;" />
<link rel="stylesheet" href="${esc(options.styleUri)}" />
<title>Faktor ${mode === 'settings' ? 'Settings' : 'Work'}</title>
</head>
<body data-host="${esc(options.host)}" data-mode="${mode}">
<div id="e8-app">
  <header class="e8-header">
    <div class="e8-brand"><span class="e8-mark" aria-hidden="true">F</span><span>Faktor</span></div>
    <nav id="e8-router" aria-label="Faktor destinations">
      ${destinations
        .map(
          (destination) =>
            `<button type="button" class="e8-tab" data-destination="${destination}" aria-current="${destinationOf(E8_SCREENS.find((screen) => screen.id === initial)!) === destination ? 'page' : 'false'}">${esc(destinationLabel(destination))}</button>`,
        )
        .join('')}
    </nav>
    <span class="e8-status" id="e8-header-status" data-level="${statusLevel}" aria-live="polite"><span aria-hidden="true">${statusGlyph(statusLevel)}</span> ${esc(status)}</span>
    <button type="button" class="e8-secondary" id="e8-refresh" aria-label="Refresh served state">Refresh</button>
  </header>
  ${
    mode === 'shell'
      ? `<div class="e8-commandbar" role="toolbar" aria-label="Screen commands">
          <button type="button" class="e8-secondary" data-open-screen="S34">Setup</button>
          <button type="button" class="e8-secondary" data-open-screen="S02">Models</button>
          <button type="button" class="e8-secondary" data-open-screen="S08">Context</button>
          <button type="button" class="e8-secondary" data-open-screen="S27">Usage</button>
          <button type="button" class="e8-secondary" data-open-screen="S29">Settings</button>
          <button type="button" class="e8-secondary" data-open-screen="S30">Diagnostics</button>
        </div>`
      : `<div class="e8-commandbar" role="toolbar" aria-label="Settings sections">
          <button type="button" class="e8-secondary" data-open-screen="S29">General</button>
          <button type="button" class="e8-secondary" data-open-screen="S03">Providers</button>
          <button type="button" class="e8-secondary" data-open-screen="S05">Tuning</button>
          <button type="button" class="e8-secondary" data-open-screen="S13">Permissions</button>
          <button type="button" class="e8-secondary" data-open-screen="S15">Tools & MCP</button>
          <button type="button" class="e8-secondary" data-open-screen="S30">Diagnostics</button>
        </div>`
  }
  ${options.state.error === null ? '' : `<div class="e8-banner e8-error" role="alert">${esc(options.state.error)}</div>`}
  <main id="e8-main">
    ${destinations
      .map((destination) => destinationMarkup(destination, options.state, initial))
      .join('\n')}
  </main>
  <footer class="e8-footer" aria-label="Draft and apply">
    <span id="e8-notice" role="status" aria-live="polite"></span>
    <span id="e8-staged-count">No staged changes</span>
    <div id="e8-patch-list" aria-label="Staged configuration"></div>
    <button type="button" class="e8-secondary" id="e8-reset">Reset draft</button>
    <button type="button" class="e8-secondary" id="e8-defaults">Restore defaults</button>
    <button type="button" class="e8-primary" id="e8-apply" disabled>Apply staged settings</button>
  </footer>
</div>
<script nonce="${options.nonce}">window.__faktorE8Initial = ${scriptJson(initial)};</script>
<script nonce="${options.nonce}" src="${esc(options.scriptUri)}"></script>
</body>
</html>`;
}
