// Server-side markup for the Revision 8 Work / Inspect / History shell and
// the editor-sized settings surfaces. Every screen, contract string, state
// and served row is rendered from the frozen registry and the typed read
// model. The webview script performs local interaction only (selection,
// staging, disclosure) and never invents a daemon value.

import { E8_CONTROLS, E8_SCREENS, type E8Control, type E8Screen } from '../generated/edition8Registry.ts';
import {
  controlView,
  descriptorValueText,
  destinationOf,
  screenView,
  screensFor,
  statusGlyph,
  type E8ControlView,
  type E8Destination,
  type E8PanelState,
  type E8ScreenView,
} from './view.ts';
import { microUsdInput, stringAt } from './contract.ts';
import { bindingFor8 } from './bindings.ts';
import { descriptorMap, type E8Descriptor } from './source.ts';

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

// Screens whose contract primary action is an actual host/control action.
// The labels are the user-facing verb, not the low-level binding label.
const PRIMARY_CONTROL: Readonly<Record<string, { readonly id: string; readonly label?: string; readonly decision?: 'allow' | 'deny'; readonly destructive?: boolean }>> = {
  S09: { id: 'C-03', label: 'Add context source' },
  S10: { id: 'K-03', label: 'Preview compaction' },
  S14: { id: 'Q-06', label: 'Allow once', decision: 'allow' },
  S17: { id: 'S-04', label: 'Install skill…' },
  S25: { id: 'V-05', label: 'Review restore…' },
  S30: { id: 'O-03', label: 'Preview export…' },
  S37: { id: 'C-05', label: 'Add attachment' },
  S59: { id: 'D-05', label: 'Save edit' },
  S63: { id: 'M-14', label: 'Start comparison' },
  S66: { id: 'K-11', label: 'Review memory…' },
  S70: { id: 'X-05', label: 'Stop run', destructive: true },
};

// Configuration surfaces whose primary action commits the staged draft.
const APPLY_SCREEN_LABEL: Readonly<Record<string, string>> = {
  S02: 'Apply model',
  S03: 'Save connection',
  S04: 'Save local inference',
  S05: 'Apply tuning',
  S06: 'Apply agent',
  S07: 'Save agent',
  S11: 'Apply changes',
  S12: 'Save memory',
  S13: 'Apply policy',
  S16: 'Save and start',
  S18: 'Save hook',
  S21: 'Apply defaults',
  S28: 'Apply efficiency settings',
  S29: 'Apply settings',
  S33: 'Use options',
  S57: 'Keep fail-closed',
  S60: 'Save resolved settings',
  S64: 'Apply settings',
};

const DESTRUCTIVE = new Set(['D-06', 'K-11', 'Q-09', 'V-12', 'X-05']);

// Low-level route bindings use generic labels ("Inspect served state") for
// several mutating endpoints. The surface names the effect instead.
const ACTION_LABEL: Readonly<Record<string, string>> = {
  'A-02': 'Save ask policy',
  'A-03': 'Save plan policy',
  'A-04': 'Save role instructions',
  'A-05': 'Save tool allowlist',
  'A-07': 'Save delegation policy',
  'A-08': 'Save child concurrency',
  'A-09': 'Save child budget',
  'C-03': 'Stage context source',
  'C-05': 'Stage attachment',
  'I-05': 'Save rule state',
  'K-03': 'Run compaction preview',
  'M-05': 'Save role model',
  'M-14': 'Run comparison',
  'M-06': 'Save fallback chain',
  'O-05': 'Check for updates',
  'O-08': 'Check managed locks',
  'P-12': 'Save proxy and TLS settings',
  'Q-06': 'Send decision',
  'Q-07': 'Save rule',
  'Q-12': 'Check policy locks',
  'S-06': 'Save commands',
  'S-09': 'Save hook filter',
  'T-14': 'Save server trust',
  'V-12': 'Restore checkpoint',
  'X-07': 'Validate URL',
};

const OPEN_COMPOSER_SCREENS = new Set(['S01', 'S36', 'S38', 'S40', 'S58']);

// Spec navigation that is a screen link rather than a control (S54
// "Account Settings… opens S55"). Read-only screens keep no primary.
const SECONDARY_SCREEN_LINKS: Readonly<
  Record<string, { readonly label: string; readonly screen: string }>
> = {
  S54: { label: 'Account Settings…', screen: 'S55' },
};

// S27 "Inspect Runs opens S53" is spec navigation, but no control carries it
// (M-13/K-12/O-06 are read-only sheets). Render the one specified primary as
// a screen action rather than inventing an export/run endpoint.
const PRIMARY_SCREEN_ACTIONS: Readonly<
  Record<string, { readonly label: string; readonly screen: string }>
> = {
  S27: { label: 'Inspect Runs', screen: 'S53' },
};

const HOST_PRIMARY: Readonly<Record<string, { readonly command: string; readonly label: string }>> = {
  S34: { command: 'faktor.startServer', label: 'Start service' },
  S35: { command: 'faktor.trustExecutable8', label: 'Confirm Path' },
  // Spec S55 primary: the optional control-plane sign-in. It opens the
  // endpoint/organization prompt; it never claims a connection before the
  // host has stored and verified the credential.
  S55: { command: 'faktor.controlPlaneSignIn', label: 'Connect Account' },
};

// The registry job text is design direction. The surface shows one plain
// sentence per group and keeps the full direction in the contract disclosure.
const GROUP_SUMMARY: Readonly<Record<string, string>> = {
  'Setup and start': 'Get the service, workspace and first task ready.',
  'Work': 'Run intent, model, effort and context beside the draft.',
  'Inspect and proof': 'Review the plan, changes, verification and finish proof.',
  'Agents': 'Delegate work and keep child runs bounded.',
  'History': 'Find, compare and switch durable conversations safely.',
  'Usage': 'See token, cost and outcome evidence.',
  'Models and providers': 'Choose models and keep provider connections healthy.',
  'Context': 'Add, inspect and compact the context a run may use.',
  'Instructions and memory': 'Review standing rules, precedence and memory.',
  'Permissions and tools': 'Review tool authority, MCP, skills and hooks.',
  'Settings and diagnostics': 'Tune budgets, efficiency, settings and recovery.',
};

function actionLabel(view: E8ControlView): string {
  return ACTION_LABEL[view.control.id] ?? view.binding.actionLabel;
}

type PrimaryAction =
  | { readonly kind: 'apply'; readonly label: string }
  | {
      readonly kind: 'control';
      readonly id: string;
      readonly label: string;
      readonly decision?: 'allow' | 'deny';
      readonly destructive: boolean;
    }
  | { readonly kind: 'none'; readonly label: string };

function primaryActionFor(screen: E8ScreenView): PrimaryAction {
  const mapped = PRIMARY_CONTROL[screen.screen.id];
  if (mapped !== undefined) {
    const view = screen.controls.find((entry) => entry.control.id === mapped.id);
    if (view !== undefined) {
      return {
        kind: 'control',
        id: mapped.id,
        label: mapped.label ?? view.binding.actionLabel,
        decision: mapped.decision,
        destructive: mapped.destructive === true || DESTRUCTIVE.has(mapped.id),
      };
    }
  }
  const applyLabel = APPLY_SCREEN_LABEL[screen.screen.id];
  if (applyLabel !== undefined) return { kind: 'apply', label: applyLabel };
  return { kind: 'none', label: screen.screen.action };
}

function controlInput(control: E8Control, view: E8ControlView): string {
  const binding = view.binding;
  const id = `e8-input-${control.id}`;
  const disabled = view.disabled ? ' disabled' : '';
  const describedBy = `e8-desc-${control.id}`;
  const common = `id="${id}" data-input="${control.id}" data-label="${esc(view.label)}" data-kind="${binding.input}" aria-describedby="${describedBy}"`;
  if (control.id === 'O-01') {
    return `<div class="e8-search">
      <input type="search" ${common} data-action-input="O-01" data-settings-search="O-01" placeholder="Filter settings by name, key or description" autocomplete="off" spellcheck="false"${disabled} />
      <p class="e8-search-count" data-settings-count aria-live="polite"></p>
    </div>`;
  }
  switch (binding.input) {
    case 'toggle':
      return `<label class="e8-toggle"><input type="checkbox" ${common}${view.configValue === true ? ' checked' : ''}${disabled} /> <span>Enabled</span></label>`;
    case 'select': {
      const options = view.options;
      const current = typeof view.configValue === 'string' ? view.configValue : '';
      const served =
        current !== '' && !options.includes(current)
          ? `<option value="${esc(current)}" selected>${esc(current)} (served value)</option>`
          : '';
      const optionMarkup =
        options.length === 0
          ? '<option value="" selected>No served choices — refresh the daemon read</option>'
          : `${served}<option value=""${current === '' ? ' selected' : ''}>Inherit (not configured)</option>${options
              .map((option) => `<option value="${esc(option)}"${option === current ? ' selected' : ''}>${esc(option)}</option>`)
              .join('')}`;
      return `<select ${common}${disabled}>${optionMarkup}</select>`;
    }
    case 'number':
      return `<input type="number" inputmode="numeric" min="${binding.min ?? 0}" step="${binding.step ?? 1}" ${common} value="${view.configValue !== null && typeof view.configValue === 'number' ? String(view.configValue) : ''}" placeholder="Inherit (not configured)"${disabled} />`;
    case 'money': {
      // B-02 is exact USD input with an advanced micro-USD readout. The
      // served value is micro-USD, so the editable field must show dollars;
      // pre-filling raw micro would stage a 1,000,000x larger cap.
      const value =
        control.id === 'B-02'
          ? microUsdInput(view.configValue)
          : view.configValue !== null && typeof view.configValue === 'string'
            ? view.configValue
            : '';
      return `<input type="text" inputmode="decimal" ${common} value="${esc(value)}" placeholder="Exact amount, e.g. 5.00"${disabled} />`;
    }
    case 'textarea':
      return `<textarea ${common} rows="3" placeholder="Inherit (not configured)"${disabled}>${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}</textarea>`;
    case 'list':
      return `<textarea ${common} rows="2" spellcheck="false" placeholder="One value per line"${disabled}>${Array.isArray(view.configValue) ? esc(view.configValue.map(String).join('\n')) : ''}</textarea>`;
    case 'secret-ref':
      return `<input type="text" ${common} value="${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="env:NAME or vault:NAME (never a secret value)" autocomplete="off" spellcheck="false"${disabled} />`;
    case 'readonly':
      return `<output ${common} class="e8-readonly">${esc(view.valueText.startsWith('Read-only served value') ? 'Read-only' : view.valueText)}</output>`;
    case 'action':
      return `<span ${common} class="e8-actionkind">Bounded action</span>`;
    default:
      return `<input type="text" ${common} value="${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="Inherit (not configured)" autocomplete="off" spellcheck="false"${disabled} />`;
  }
}

function decisionMarkup(view: E8ControlView): string {
  if (view.control.id !== 'Q-06') return '';
  const requests = view.approvalIds
    .map((id, index) => `<option value="${esc(id)}"${view.approvalIds.length === 1 && index === 0 ? ' selected' : ''}>Request #${esc(id)}</option>`)
    .join('');
  // No element ids here: Q-06 is rendered by more than one screen, so ids
  // would collide. The media query is scoped to the active screen instead.
  return `<div class="e8-decision">
      <span class="e8-field-label">Exact pending request</span>
      <select data-approval-request="${view.control.id}" aria-label="Exact pending request">
        <option value="">Choose the exact request…</option>
        ${requests}
      </select>
      <span class="e8-field-label">Decision</span>
      <select data-approval-decision="${view.control.id}" aria-label="Approval decision">
        <option value="">Choose a decision…</option>
        <option value="allow">Allow once</option>
        <option value="deny">Deny</option>
      </select>
    </div>`;
}

function actionButton(view: E8ControlView): string {
  const control = view.control;
  const binding = view.binding;
  if (binding.input === 'readonly') return '';
  if (binding.input === 'action') {
    const disabled = view.disabled ? ' disabled aria-disabled="true"' : '';
    const destructive = DESTRUCTIVE.has(control.id);
    const label = actionLabel(view);
    const confirm = destructive ? ` data-confirm="true" data-confirm-label="${esc(label)}"` : '';
    return `<button type="button" class="e8-secondary e8-action${destructive ? ' e8-danger' : ''}" data-activate="${control.id}"${confirm}${disabled}>${esc(label)}</button>`;
  }
  // Config controls are applied through the single screen/footer action. The
  // activation path remains for secret-reference/money controls so every
  // contract role still has a focusable element.
  if (control.id === 'O-01') {
    const disabled = view.disabled ? ' disabled aria-disabled="true"' : '';
    return `<button type="button" class="e8-secondary e8-action" data-activate="${control.id}"${disabled}>${esc(actionLabel(view))}</button>`;
  }
  if (binding.input === 'secret-ref' || binding.input === 'money') {
    const disabled = view.disabled ? ' disabled aria-disabled="true"' : '';
    return `<button type="button" class="e8-secondary e8-action e8-hidden-action" data-activate="${control.id}"${disabled}>${esc(actionLabel(view))}</button>`;
  }
  return '';
}

function inspectMarkup(view: E8ControlView): string {
  if (!view.disabled) return '';
  return `<button type="button" class="e8-secondary" data-inspect="${view.control.id}">Why disabled?</button>`;
}

function resetMarkup(view: E8ControlView): string {
  if (view.binding.configKey === null) return '';
  const disabled = view.disabled ? ' disabled' : '';
  return `<button type="button" class="e8-link-button" data-unstage="${view.control.id}"${disabled}>Reset this override</button>`;
}

function controlMarkup(view: E8ControlView): string {
  const control = view.control;
  const binding = view.binding;
  const status = view.applyState === null ? '' : `<span class="e8-apply-state" data-apply-state="${esc(view.applyState)}">${esc(view.applyState)}</span>`;
  const missing = binding.kind === 'missing' ? ' data-missing="true"' : '';
  const needsSeed = binding.input === 'action' || binding.input === 'secret-ref' || binding.input === 'money';
  const shownValue =
    binding.input === 'readonly' || binding.input === 'action' || view.valueText === '' || view.valueText === 'Inherit (not configured)'
      ? ''
      : view.valueText;
  return `<article class="e8-control" id="e8-control-${control.id}" data-control="${control.id}" data-config="${binding.configKey === null ? 'false' : 'true'}" data-state="${view.state}" data-kind="${binding.input}" data-route="${esc(binding.route.method)} ${esc(binding.route.path)}"${missing} tabindex="-1" aria-labelledby="e8-name-${control.id}">
    <div class="e8-control-main">
      <div class="e8-control-copy">
        <div class="e8-control-head">
          <h3 id="e8-name-${control.id}" class="e8-control-name">${esc(view.label)}</h3>
          ${status}
          <span class="e8-state-badge" data-state="${view.state}">${esc(view.state)}</span>
        </div>
        <p class="e8-descriptor">${esc(view.description)}</p>
        <p id="e8-desc-${control.id}" class="e8-help">${esc(shownValue)}</p>
        <p class="e8-control-state" data-control-state="${control.id}">${esc(view.stateText)}</p>
      </div>
      <div class="e8-control-value">
        ${binding.input === 'action' ? control.id === 'Q-06' ? '' : '<span class="e8-field-label">Action</span>' : binding.input === 'toggle' ? '' : `<label for="e8-input-${control.id}" class="e8-field-label">${binding.input === 'readonly' ? 'Served value' : control.id === 'O-01' ? 'Search settings' : 'Value'}</label>`}
        ${controlInput(control, view)}
        ${decisionMarkup(view)}
        ${actionButton(view)}
        ${inspectMarkup(view)}
      </div>
    </div>
    <details class="e8-control-details">
      <summary>Details and states</summary>
      <p class="e8-behavior">${esc(control.behavior)}</p>
      <dl class="e8-contract">
        <div><dt>Default</dt><dd>${esc(control.defaultState)}</dd></div>
        <div><dt>Scope</dt><dd>${esc(control.scope)}</dd></div>
        <div><dt>Apply timing</dt><dd>${esc(control.applyTiming)}</dd></div>
        <div><dt>Route</dt><dd class="e8-wrap">${esc(view.routeText)}</dd></div>
      </dl>
      ${needsSeed ? `<div class="e8-request"><label for="e8-action-${control.id}" class="e8-field-label">Request payload (optional)</label><textarea id="e8-action-${control.id}" data-action-input="${control.id}" rows="2" placeholder="Optional text or JSON fields; the served defaults are used when empty."></textarea></div>` : ''}
      <h4 class="e8-details-title">States</h4>
      <dl class="e8-states">
        <div><dt>Empty</dt><dd>${esc(control.emptyState)}</dd></div>
        <div><dt>Error</dt><dd>${esc(control.errorState)}</dd></div>
        <div><dt>Disabled</dt><dd>${esc(control.disabledState)}</dd></div>
        <div><dt>Loading</dt><dd>${esc(control.loadingState)}</dd></div>
        <div><dt>Stale</dt><dd>${esc(control.staleState)}</dd></div>
        <div><dt>Pending</dt><dd>${esc(control.pendingState)}</dd></div>
      </dl>
      ${resetMarkup(view)}
    </details>
  </article>`;
}

function groupControls(views: readonly E8ControlView[]): string {
  return `<div class="e8-control-grid">${views.map((view) => controlMarkup(view)).join('\n')}</div>`;
}

function rowMarkup(row: E8ScreenView['rows'][number]): string {
  const open =
    row.openScreen === undefined ? '' : ` data-open-screen="${esc(row.openScreen)}" role="button" tabindex="0"`;
  return `<div class="e8-row" data-row-key="${esc(row.key)}" data-row-state="${row.state}"${open}>
    <span class="e8-row-label">${esc(row.label)}</span>
    <span class="e8-row-value" data-state="${row.state}">${esc(row.value)}</span>
  </div>`;
}

function primaryMarkup(primary: PrimaryAction, screen: E8ScreenView, host: 'vscode' | 'jetbrains', mode: 'shell' | 'settings'): string {
  if (primary.kind === 'apply') {
    // Settings mode has ONE apply in the footer; a second per-screen primary
    // would compete with it and invite a partial commit.
    if (mode === 'settings') return '';
    return `<button type="button" class="e8-primary" data-primary-apply="true" disabled aria-keyshortcuts="Control+Enter Meta+Enter">${esc(primary.label)}</button>
      <span class="e8-action-note">Staged changes are not written until this commit.</span>`;
  }
  if (primary.kind === 'control') {
    const view = screen.controls.find((entry) => entry.control.id === primary.id);
    const disabled = view?.disabled === true ? ' disabled aria-disabled="true"' : '';
    const confirm = primary.destructive
      ? ` data-confirm="true" data-confirm-label="${esc(primary.label)}"`
      : '';
    const decision = primary.decision === undefined ? '' : ` data-primary-decision="${primary.decision}"`;
    return `<button type="button" class="e8-primary${primary.destructive ? ' e8-danger' : ''}" data-primary-control="${esc(primary.id)}"${decision}${confirm}${disabled}>${esc(primary.label)}</button>`;
  }
  const hostPrimary = host === 'vscode' ? HOST_PRIMARY[screen.screen.id] : undefined;
  if (hostPrimary !== undefined) {
    return `<button type="button" class="e8-primary" data-host-command="${esc(hostPrimary.command)}">${esc(hostPrimary.label)}</button>`;
  }
  const open =
    host === 'vscode' && OPEN_COMPOSER_SCREENS.has(screen.screen.id)
      ? '<button type="button" class="e8-primary" data-host-command="faktor.openChat">Open composer</button>'
      : '';
  return open;
}

function secondaryMarkup(screenId: string): string {
  // S34 is the Service diagnostics screen: keep the explicit stop control
  // next to the primary start action.
  const stop =
    screenId === 'S34'
      ? '<button type="button" class="e8-secondary" data-host-command="faktor.stopServer">Stop service</button>'
      : '';
  const link = SECONDARY_SCREEN_LINKS[screenId];
  if (link === undefined) return stop;
  return `${stop}<button type="button" class="e8-secondary" data-open-screen="${esc(link.screen)}">${esc(link.label)}</button>`;
}

function primaryScreenLinkMarkup(screenId: string): string {
  const link = PRIMARY_SCREEN_ACTIONS[screenId];
  if (link === undefined) return '';
  return `<button type="button" class="e8-primary" data-open-screen="${esc(link.screen)}">${esc(link.label)}</button>`;
}

function screenMarkup(screen: E8ScreenView, initial: boolean, host: 'vscode' | 'jetbrains', mode: 'shell' | 'settings'): string {
  const screenEntry = screen.screen;
  const primary = primaryActionFor(screen);
  const showControls = screen.controls.length > 0;
  const rowsMarkup =
    screen.rows.length === 0
      ? ''
      : `<h3 class="e8-section-title">Current state</h3><section class="e8-data" aria-label="Served ${esc(screenEntry.id)} state">${screen.rows.map(rowMarkup).join('\n')}</section>`;
  // S54 is read-only: served usage categories lead, and the read-only
  // control cards stay behind a collapsed provenance disclosure instead of
  // pushing every number below the 480px fold. Expanding it keeps the same
  // control contract (and focus order) one disclosure away.
  const rowsFirst = screenEntry.id === 'S53' || screenEntry.id === 'S27' || screenEntry.id === 'S54';
  const controlsMarkup = !showControls
    ? ''
    : rowsFirst
      ? `<details class="e8-provenance"><summary>Advanced diagnostics (read-only) · ${screen.controls.length}</summary>${groupControls(screen.controls)}</details>`
      : groupControls(screen.controls);
  return `<section class="e8-screen" id="e8-screen-${screenEntry.id}" data-screen="${screenEntry.id}" data-group="${esc(screenEntry.group)}" data-destination="${destinationOf(screenEntry)}" data-surface="${screenEntry.surface}" data-primary="${primary.kind}"${initial ? '' : ' hidden'} aria-labelledby="e8-screen-title-${screenEntry.id}">
    <header class="e8-screen-head">
      <div class="e8-kicker">${esc(screenEntry.group)}</div>
      <div class="e8-title-row">
        <h2 id="e8-screen-title-${screenEntry.id}" tabindex="-1">${esc(screenEntry.title)}</h2>
        <span class="e8-status" data-level="${screen.statusLevel}" role="status" aria-live="polite"><span aria-hidden="true">${statusGlyph(screen.statusLevel)}</span> <span data-status-text>${esc(screen.status)}</span></span>
      </div>
      <p class="e8-purpose">${esc(GROUP_SUMMARY[screenEntry.group] ?? screenEntry.job)}</p>
      <div class="e8-primary-row">${primaryMarkup(primary, screen, host, mode)}${primaryScreenLinkMarkup(screenEntry.id)}${secondaryMarkup(screenEntry.id)}</div>

    </header>
    ${screen.staleText === null ? '' : `<div class="e8-banner e8-warn" role="alert">${esc(screen.staleText)}</div>`}
    ${screen.emptyText === null || screen.emptyText.startsWith('No served state yet') ? '' : `<div class="e8-banner e8-empty" role="status">${esc(screen.emptyText)}</div>`}
    ${rowsFirst ? rowsMarkup : ''}
    ${controlsMarkup}
    ${rowsFirst ? '' : rowsMarkup}
  </section>`;
}


/**
 * The one searchable settings surface. Rows come from the served descriptor
 * catalog, never from screen pages: one descriptor, one row, one value
 * control. Categorical grouping is a presentation concern; the registry
 * screen stays the contract owner for the control itself.
 */
interface SettingsDomain {
  readonly key: string;
  readonly label: string;
  readonly description: string;
}

const SETTINGS_DOMAINS: readonly SettingsDomain[] = [
  { key: 'providers', label: 'Providers and models', description: 'Connections, default models and generation tuning.' },
  { key: 'context', label: 'Context and compaction', description: 'Sources, attachments, memory, rules and compaction.' },
  { key: 'tools', label: 'Tools and MCP', description: 'Tool authority, MCP servers, skills, hooks and terminals.' },
  { key: 'permissions', label: 'Permissions', description: 'One effective policy, sandbox isolation and remembered rules.' },
  { key: 'billing', label: 'Billing and usage', description: 'Entitlements, quota, spend and the account connection.' },
  { key: 'appearance', label: 'Appearance and general', description: 'Display, telemetry, diagnostics and revision conflicts.' },
];

// Usage, diagnostics and account are real settings-destination screens but
// expose served state and host actions, not configurable descriptors. They
// stay reachable as single full-window pages; the config home never renders
// them beside the descriptor rows.
const SETTINGS_DETAIL_SCREENS = new Set(['S27', 'S30', 'S53', 'S55']);

const SETTINGS_DESCRIPTIONS: Readonly<Record<string, string>> = {
  'faktor.defaultProvider': 'Provider for new tasks until a task overrides it.',
  'faktor.defaultModel': 'Default model for new tasks.',
  'faktor.model.contextLimit': 'Context window used for admission and compaction.',
  'faktor.generation.maxOutput': 'Maximum tokens the provider may return.',
  'faktor.generation.temperature': 'Sampling randomness for model calls.',
  'faktor.generation.topP': 'Nucleus sampling threshold for model calls.',
  'faktor.context.compactionMinReduction': 'Minimum context reduction a compaction must achieve.',
  'faktor.efficiency.failureLearning': 'Reuse verified failure lessons in later runs.',
  'faktor.efficiency.ccr': 'Compress tool evidence before it enters context.',
  'faktor.efficiency.typedHandoff': 'Pass typed handoffs between agents.',
  'faktor.efficiency.semanticContext': 'Select context semantically instead of by position.',
  'faktor.efficiency.reworkGuard': 'Stop repeated failed edits from consuming the run.',
  'faktor.memory.captureMode': 'When durable memory is captured.',
  'faktor.permissions.preset': 'Baseline approval policy for tool calls.',
  'faktor.permissions.allowedTools': 'Tools the agent may call without a new decision.',
  'faktor.sandbox.shell': 'Terminal authority for commands.',
  'faktor.sandbox.network': 'Network access allowed to tools and providers.',
  'faktor.sandbox.allowLoopback': 'Permit loopback connections inside the sandbox.',
  'faktor.budget.maxTokens': 'Token ceiling for one task; zero means no limit.',
  'faktor.budget.maxCostMicro': 'Spending ceiling for one task.',
  'faktor.budget.warnAtPercent': 'Show a warning at this share of the ceiling.',
  'faktor.retry.maxAttempts': 'Total attempts before a retryable failure is final.',
  'faktor.retry.baseDelayMs': 'Base delay for exponential retry backoff.',
  'faktor.runtime.binaryPath': 'Daemon executable resolved by the host.',
  'faktor.telemetry.enabled': 'Share anonymous product telemetry.',
  'faktor.telemetry.categories': 'Telemetry categories that may be shared.',
  'faktor.retrieval.strategy': 'How context is retrieved for a run.',
  'faktor.retrieval.embeddingProvider': 'Provider used to embed retrieval content.',
};

interface SettingsRowModel {
  readonly key: string;
  readonly label: string;
  readonly description: string;
  readonly category: string | null;
  readonly domain: string;
  readonly control: E8Control | null;
  readonly view: E8ControlView | null;
  readonly advanced: boolean;
}

function settingsCategoryForControl(control: E8Control): string | null {
  switch (control.categoryCode) {
    case 'M':
      return 'models';
    case 'G':
      return 'generation';
    case 'C':
      return 'context';
    case 'K':
      return 'efficiency';
    case 'B':
      return 'budgets';
    case 'P':
      return 'sandbox';
    case 'Q':
      return 'permissions';
    case 'O':
      return 'privacy';
    default:
      return null;
  }
}

const SETTINGS_CONTROL_INDEX: ReadonlyMap<string, E8Control> = (() => {
  const index = new Map<string, E8Control>();
  for (const control of E8_CONTROLS) {
    const key = bindingFor8(control).configKey;
    if (key !== null && !index.has(key)) index.set(key, control);
  }
  return index;
})();

function settingsDomainForRow(row: { readonly key: string; readonly category: string | null }): string {
  if (row.key === 'faktor.permissions.allowedTools' || row.key === 'faktor.sandbox.shell') return 'tools';
  switch (row.category) {
    case 'models':
    case 'generation':
      return 'providers';
    case 'context':
    case 'efficiency':
    case 'memory':
      return 'context';
    case 'permissions':
    case 'sandbox':
      return 'permissions';
    case 'budgets':
    case 'reliability':
      return 'billing';
    default:
      return 'appearance';
  }
}

function settingsDomainForScreen(screen: E8Screen): string {
  if (screen.group === 'Models and providers') return 'providers';
  if (screen.group === 'Context' || screen.group === 'Instructions and memory') return 'context';
  if (screen.id === 'S16' || screen.id === 'S17' || screen.id === 'S18' || screen.id === 'S19' || screen.id === 'S20' || screen.id === 'S15' || screen.id === 'S68') return 'tools';
  if (screen.id === 'S13' || screen.id === 'S57' || screen.id === 'S67') return 'permissions';
  if (screen.group === 'Usage') return 'billing';
  return 'appearance';
}

function settingsHasActions(screen: E8Screen): boolean {
  return screen.controls.some((id) => {
    const control = E8_CONTROLS.find((entry) => entry.id === id);
    return control !== undefined && bindingFor8(control).configKey === null;
  });
}

function settingsScreenView(screenId: string, state: E8PanelState): E8ScreenView {
  const view = screenView(screenId, state);
  return {
    ...view,
    controls: view.controls.filter(
      (control) => control.binding.configKey === null && control.control.id !== 'O-01',
    ),
    rows: view.rows.filter(
      (row) => !row.key.startsWith('config-') && !row.key.startsWith('faktor.') && !row.label.startsWith('faktor.'),
    ),
  };
}

function settingsIsAdvanced(descriptor: E8Descriptor | null): boolean {
  if (descriptor === null) return false;
  return (
    descriptor.writable === false || stringAt(descriptor.runtimeConsumer, 'state') === 'stored_only'
  );
}

function humanSettingKey(key: string): string {
  const tail = key.split('.').pop() ?? key;
  return tail.replace(/([a-z0-9])([A-Z])/g, '$1 $2').replace(/^./, (letter) => letter.toUpperCase());
}

function settingsRows(state: E8PanelState): readonly SettingsRowModel[] {
  const snapshot = state.snapshot;
  const descriptors = descriptorMap(snapshot?.descriptors ?? null);
  // The descriptor catalog is the single source of truth. A control-only
  // fallback keeps the surface usable while the first descriptor read is in
  // flight, but a served catalog is never widened by the registry.
  const keys = descriptors.size > 0 ? [...descriptors.keys()] : [...SETTINGS_CONTROL_INDEX.keys()];
  const rows: SettingsRowModel[] = [];
  const seen = new Set<string>();
  for (const key of keys) {
    if (seen.has(key)) continue;
    seen.add(key);
    const descriptor = descriptors.get(key) ?? null;
    const control = SETTINGS_CONTROL_INDEX.get(key) ?? null;
    if (descriptor === null && control === null) continue;
    const view = control === null ? null : controlView(control, snapshot, state.loading, state.error);
    const category = descriptor?.category ?? (control === null ? null : settingsCategoryForControl(control));
    const label = descriptor?.label ?? view?.label ?? control?.name ?? humanSettingKey(key);
    const description =
      SETTINGS_DESCRIPTIONS[key] ??
      descriptor?.description ??
      view?.description ??
      (category === null ? 'Persistent Faktor setting.' : `Persistent setting in the ${category} category.`);
    rows.push({
      key,
      label,
      description,
      category,
      domain: settingsDomainForRow({ key, category }),
      control,
      view,
      advanced: settingsIsAdvanced(descriptor),
    });
  }
  return rows;
}

function settingsValueMarkup(row: SettingsRowModel, state: E8PanelState): string {
  if (row.control !== null && row.view !== null) {
    const current = `<span id="e8-desc-${esc(row.control.id)}" class="sr-only" data-settings-current>${esc(row.view.valueText)}</span>`;
    return `${controlInput(row.control, row.view)}${current}`;
  }
  return `<output class="e8-readonly" data-settings-readonly="${esc(row.key)}">${esc(descriptorValueText(state.snapshot, row.key))}</output>`;
}

function settingsRowMarkup(row: SettingsRowModel, state: E8PanelState): string {
  const control = row.control;
  const controlAttrs =
    control === null
      ? ''
      : ` id="e8-control-${esc(control.id)}" data-state="${row.view?.state ?? 'available'}"`;
  const label =
    control === null
      ? `<span class="e8-settings-label">${esc(row.label)}</span>`
      : `<label class="e8-settings-label" for="e8-input-${esc(control.id)}">${esc(row.label)}</label>`;
  const stateText =
    row.view === null || row.view.stateText.length === 0
      ? ''
      : `<p class="e8-settings-state" data-settings-state${control === null ? '' : ` data-control-state="${esc(control.id)}"`}>${esc(row.view.stateText)}</p>`;
  const domain = SETTINGS_DOMAINS.find((entry) => entry.key === row.domain);
  const search = `${row.label} ${row.description} ${domain?.label ?? ''} ${domain?.description ?? ''} ${row.key}`.toLowerCase();
  const rowFocus = row.view?.disabled ? ' tabindex="0"' : '';
  return `<div class="e8-settings-row"${controlAttrs}${rowFocus} data-settings-row data-settings-key="${esc(row.key)}" data-settings-haystack="${esc(search)}"${row.advanced ? ' data-settings-advanced-row="true"' : ''}>
    <div class="e8-settings-copy">
      ${label}
      <p class="e8-settings-desc">${esc(row.description)}</p>
      ${stateText}
    </div>
    <div class="e8-settings-value">${settingsValueMarkup(row, state)}</div>
  </div>`;
}

function settingsDescription(screen: E8Screen): string {
  const first = screen.job.split('. ')[0].trim();
  return first.length === 0 ? screen.job : first;
}

function settingsActionRowMarkup(screen: E8Screen): string {
  const summary = settingsDescription(screen);
  const search = `${screen.title} ${screen.group} ${summary}`.toLowerCase();
  return `<button type="button" class="e8-settings-action-link" data-settings-open="${esc(screen.id)}" data-settings-link-row data-settings-haystack="${esc(search)}">
    <span class="e8-settings-action-title">${esc(screen.title)}</span>
    <span class="e8-settings-action-desc">${esc(summary)}</span>
  </button>`;
}

function settingsHomeMarkup(state: E8PanelState): string {
  const rows = settingsRows(state);
  const domains = SETTINGS_DOMAINS.map((domain) => {
    const domainRows = rows.filter((row) => row.domain === domain.key);
    if (domainRows.length === 0) return '';
    const primary = domainRows.filter((row) => !row.advanced);
    const advanced = domainRows.filter((row) => row.advanced);
    const advancedMarkup =
      advanced.length === 0
        ? ''
        : `<details class="e8-settings-advanced" data-settings-advanced><summary>Advanced · ${advanced.length}</summary>${advanced
            .map((row) => settingsRowMarkup(row, state))
            .join('')}</details>`;
    const actionScreens = screensFor('settings').filter(
      (screen) => settingsDomainForScreen(screen) === domain.key && settingsHasActions(screen),
    );
    const actionsMarkup =
      actionScreens.length === 0
        ? ''
        : `<details class="e8-settings-actions" data-settings-actions><summary>Actions and diagnostics · ${actionScreens.length}</summary>${actionScreens
            .map((screen) => settingsActionRowMarkup(screen))
            .join('')}</details>`;
    return `<section class="e8-settings-domain" data-settings-domain="${domain.key}">
      <header class="e8-settings-domain-head">
        <h3>${esc(domain.label)}</h3>
        <p>${esc(domain.description)}</p>
      </header>
      <div class="e8-settings-rows">${primary.map((row) => settingsRowMarkup(row, state)).join('')}</div>
      ${advancedMarkup}
      ${actionsMarkup}
    </section>`;
  }).join('');
  return `<div class="e8-settings-route">
  <button type="button" id="e8-settings-back" class="e8-secondary e8-settings-back" data-settings-back hidden>Back to settings</button>
      <button type="button" class="e8-secondary" data-open-chat style="margin-left:auto;font-size:0.8em;">← Chat</button>
  <section id="e8-settings-home" class="e8-settings-home" data-settings-descriptor-count="${rows.length}" aria-labelledby="e8-settings-title">
    <header class="e8-settings-head">
      <div>
        <h2 id="e8-settings-title">Settings</h2>
        <p class="e8-settings-lead">One surface for every persistent setting. Values are staged locally; the one Apply action commits them together.</p>
      </div>
      <label class="e8-search e8-settings-search"><span class="sr-only">Search settings</span><input type="search" id="e8-settings-search" data-settings-global-search placeholder="Search settings by name, domain or description" autocomplete="off" spellcheck="false" /></label>
      <p class="e8-settings-search-count" data-settings-global-count role="status" aria-live="polite">${rows.length} settings</p>
    </header>
    <div class="e8-settings-save" role="status" aria-live="polite">
      <span id="e8-settings-save-state" data-state="clean">Autosave is off — no staged changes.</span>
      <button type="button" class="e8-secondary" data-settings-review>Review staged changes</button>
    </div>
    ${domains}
    <p class="e8-settings-empty" data-settings-empty hidden>No setting matches this search.</p>
  </section>
  </div>`;
}

function screenBrowser(destination: E8Destination, initialScreen: string): string {
  const screens = screensFor(destination);
  const groups = new Map<string, E8Screen[]>();
  for (const screen of screens) {
    const list = groups.get(screen.group) ?? [];
    list.push(screen);
    groups.set(screen.group, list);
  }
  return `<details class="e8-browser">
    <summary><span>Browse screens</span><span class="e8-count">${screens.length}</span></summary>
    <div class="e8-browser-body">
      <label class="e8-search"><span class="sr-only">Search screens</span><input type="search" data-screen-filter placeholder="Search screens by name or ID" autocomplete="off" spellcheck="false" /></label>
      <div class="e8-browser-groups">
        ${[...groups.entries()]
          .map(
            ([group, list]) => `<section class="e8-browser-group" data-screen-group>
              <h3>${esc(group)}</h3>
              <div class="e8-browser-links">${list
                .map(
                  (screen) =>
                    `<button type="button" class="e8-screen-link" data-screen-link="${screen.id}" data-screen-id="${screen.id}" data-screen-search="${esc(`${screen.id} ${screen.title} ${screen.group}`)}" aria-current="${screen.id === initialScreen ? 'page' : 'false'}"><span class="e8-screen-name">${esc(screen.title)}</span></button>`,
                )
                .join('')}</div>
            </section>`,
          )
          .join('')}
      </div>
      <p class="e8-filter-empty" hidden>No screens match this search.</p>
    </div>
  </details>`;
}

function destinationMarkup(destination: E8Destination, state: E8PanelState, initialScreen: string, host: 'vscode' | 'jetbrains', mode: 'shell' | 'settings'): string {
  // Settings renders descriptors, not screen pages. Keeping the 43 hidden
  // screens here was the legacy+new double render: every descriptor existed
  // once as a screen control and again as a served "Current state" row.
  if (mode === 'settings') {
    if (SETTINGS_DETAIL_SCREENS.has(initialScreen)) {
      const detail = screensFor('settings').find((screen) => screen.id === initialScreen);
      if (detail === undefined) return '';
      return `<div class="e8-destination" data-destination-panel="settings">
        <div class="e8-screens-host">
          ${screenMarkup(screenView(detail.id, state), true, host, mode)}
        </div>
      </div>`;
    }
    // Config descriptors live once in the home rows. The registry pages stay
    // reachable for their actions/read-only diagnostics, but every config
    // control and every config-key served row is filtered out so no knob can
    // be rendered twice.
    const actionScreens = screensFor('settings')
      .map((screen) => ({ screen, view: settingsScreenView(screen.id, state) }))
      .filter((entry) => entry.view.controls.length > 0 || entry.view.rows.length > 0);
    return `<div class="e8-destination" data-destination-panel="settings" hidden>
      <div class="e8-screens-host">
        ${actionScreens.map((entry) => screenMarkup(entry.view, false, host, mode)).join('\n')}
      </div>
    </div>`;
  }
  const screens = screensFor(destination);
  const visible = screens.some((screen) => screen.id === initialScreen);
  return `<div class="e8-destination" data-destination-panel="${destination}"${visible ? '' : ' hidden'}>
    ${screenBrowser(destination, initialScreen)}
    <div class="e8-screens-host">
      ${screens.map((screen) => screenMarkup(screenView(screen.id, state), screen.id === initialScreen && visible, host, mode)).join('\n')}
    </div>
  </div>`;
}

export interface Edition8AppOptions {
  readonly state: E8PanelState;
  readonly initialScreen: string;
  readonly host: 'vscode' | 'jetbrains';
  readonly mode?: 'shell' | 'settings';
  /** Embedded in the Faktor window: the host refreshes immediately. */
  readonly omitEmptyBanner?: boolean;
}

/**
 * The body of the Edition 8 surface without the document wrapper. The VS Code
 * window renders this app inside its ONE editor panel next to Chat, so the
 * settings surface never becomes a competing editor tab or sidebar view.
 */
export function renderEdition8AppMarkup(options: Edition8AppOptions): string {
  const initial = E8_SCREENS.some((screen) => screen.id === options.initialScreen)
    ? options.initialScreen
    : 'S01';
  const mode = options.mode ?? (destinationOf(E8_SCREENS.find((screen) => screen.id === initial)!) === 'settings' ? 'settings' : 'shell');
  const destinations: readonly E8Destination[] =
    mode === 'settings' ? ['settings'] : ['work', 'inspect', 'history'];
  const loading = options.state.loading;
  const noSnapshot =
    options.omitEmptyBanner !== true && options.state.snapshot === null && !loading && options.state.error === null;
  return `<div id="e8-app">
  <header class="e8-header">
    <div class="e8-brand"><span class="e8-mark" aria-hidden="true">F</span><span>Faktor</span></div>
    <nav id="e8-router" aria-label="Faktor destinations">
      ${destinations
        .map(
          (destination) =>
            `<button type="button" class="e8-tab" data-destination="${destination}" aria-current="${destinationOf(E8_SCREENS.find((screen) => screen.id === initial)!) === destination ? 'page' : 'false'}" aria-keyshortcuts="${destination === 'work' ? 'Alt+1' : destination === 'inspect' ? 'Alt+2' : destination === 'history' ? 'Alt+3' : 'Alt+4'}">${esc(destinationLabel(destination))}</button>`,
        )
        .join('')}
    </nav>
    <div class="e8-header-actions">
      <button type="button" class="e8-secondary" id="e8-refresh" aria-label="Refresh served state" aria-keyshortcuts="Alt+R">Refresh</button>
    </div>
  </header>
  <div id="e8-flash" class="e8-flash" role="status" aria-live="polite" hidden></div>
  ${
    loading
      ? `<div class="e8-banner e8-loading" role="status">Reading served state. Controls are disabled until the daemon answers.</div>`
      : ''
  }
  ${
    options.state.error === null
      ? ''
      : `<div class="e8-banner e8-error" role="alert"><strong>Refresh failed.</strong> ${esc(options.state.error)} <button type="button" class="e8-secondary" data-retry="true">Retry</button></div>`
  }
  ${
    noSnapshot
      ? `<div class="e8-banner e8-empty" role="status"><strong>No served state yet.</strong> Faktor connects automatically; <button type="button" class="e8-link-button" data-retry="true">retry</button> if it does not. Nothing on this screen is saved or verified.</div>`
      : ''
  }
  <main id="e8-main">
    ${mode === 'settings' && !SETTINGS_DETAIL_SCREENS.has(initial) ? settingsHomeMarkup(options.state) : ''}
    ${destinations
      .map((destination) => destinationMarkup(destination, options.state, initial, options.host, mode))
      .join('\n')}
  </main>
  <footer class="e8-footer" id="e8-footer" aria-label="Draft and apply" hidden>
    <div class="e8-footer-status">
      <span id="e8-staged-count">No staged changes</span>
      <div id="e8-patch-list" aria-label="Staged configuration"></div>
    </div>
    <div class="e8-footer-actions">
      <details class="e8-footer-more">
        <summary>More</summary>
        <button type="button" class="e8-secondary" id="e8-defaults" data-confirm="true" data-confirm-label="Restore defaults">Restore defaults…</button>
      </details>
      <button type="button" class="e8-secondary" id="e8-reset" disabled>Discard changes</button>
      <button type="button" class="e8-primary" id="e8-apply" disabled aria-keyshortcuts="Control+Enter Meta+Enter">Apply changes</button>
    </div>
  </footer>
</div>`;
}

export function renderEdition8Html(options: Edition8HtmlOptions): string {
  const initial = E8_SCREENS.some((screen) => screen.id === options.initialScreen)
    ? options.initialScreen
    : 'S01';
  const mode = options.mode ?? (destinationOf(E8_SCREENS.find((screen) => screen.id === initial)!) === 'settings' ? 'settings' : 'shell');
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${esc(options.cspSource)} 'unsafe-inline'; script-src 'nonce-${options.nonce}'; img-src ${esc(options.cspSource)} data:; connect-src 'none'; base-uri 'none'; form-action 'none';" />
<link rel="stylesheet" href="${esc(options.styleUri)}" />
<title>Faktor ${mode === 'settings' ? 'Settings' : 'Work'}</title>
</head>
<body data-host="${esc(options.host)}" data-mode="${mode}">
${renderEdition8AppMarkup(options)}
<script nonce="${options.nonce}">window.__faktorE8Initial = ${scriptJson(initial)};</script>
<script nonce="${options.nonce}" src="${esc(options.scriptUri)}"></script>
</body>
</html>`;
}
