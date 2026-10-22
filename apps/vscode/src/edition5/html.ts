// Server-side markup for the Faktor Control Center (one editor tab). Every
// screen, control contract string, state and served row is rendered from the
// frozen registry and the typed read model. The webview script only performs
// local interaction (stage drafts, select screens, dispatch typed actions);
// it never invents a daemon value.

import { E5_CATEGORIES, E5_CONTROLS, E5_SCREENS, type Edition5Control } from '../generated/edition5Registry.ts';
import {
  destinationOf,
  screenView,
  screensFor,
  type E5Destination,
  type E5PanelState,
  type E5ScreenView,
} from './view.ts';

export interface Edition5HtmlOptions {
  readonly nonce: string;
  readonly cspSource: string;
  readonly scriptUri: string;
  readonly styleUri: string;
  readonly state: E5PanelState;
  readonly initialScreen: string;
  readonly host: 'vscode' | 'jetbrains';
}

function esc(value: string): string {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

const DESTINATIONS: readonly { key: E5Destination; label: string; hint: string }[] = [
  { key: 'work', label: 'Work', hint: 'Intent, models, context, approvals and the live run' },
  { key: 'inspect', label: 'Inspect', hint: 'Boundary, tools, terminal and review obligations' },
  { key: 'history', label: 'History', hint: 'Durable conversations, search and engineering outcomes' },
  { key: 'settings', label: 'Settings', hint: 'Providers, tuning, authority, routines and diagnostics' },
];

function screenTitle(screenId: string): string {
  const screen = E5_SCREENS.find((entry) => entry.id === screenId);
  return screen === undefined ? screenId : `${screen.id} · ${screen.title}`;
}

function destinationLabel(destination: E5Destination): string {
  return DESTINATIONS.find((entry) => entry.key === destination)?.label ?? destination;
}

function nav(destination: E5Destination, initialScreen: string): string {
  const screens = screensFor(destination);
  return `<nav class="e5-screens" aria-label="${esc(destinationLabel(destination))} screens">
    ${screens
      .map(
        (screen) =>
          `<button type="button" class="e5-screen-link" data-screen-link="${screen.id}" aria-current="${screen.id === initialScreen ? 'page' : 'false'}">
            <span class="e5-screen-id">${screen.id}</span>
            <span class="e5-screen-name">${esc(screen.title)}</span>
          </button>`,
      )
      .join('\n')}
  </nav>`;
}

function rowMarkup(row: { key: string; label: string; value: string; state: string }): string {
  return `<div class="e5-row" data-row-key="${esc(row.key)}" data-row-state="${row.state}">
    <span class="e5-row-label">${esc(row.label)}</span>
    <span class="e5-row-value" data-state="${row.state}">${esc(row.value)}</span>
  </div>`;
}

function controlInput(control: Edition5Control, view: E5ScreenView['controls'][number]): string {
  const binding = view.binding;
  const id = `e5-input-${control.id}`;
  const disabled = view.disabled ? ' disabled' : '';
  const describedBy = `e5-desc-${control.id}`;
  const common = `id="${id}" data-input="${control.id}" data-kind="${binding.input}" aria-describedby="${describedBy}"`;
  switch (binding.input) {
    case 'toggle':
      return `<label class="e5-toggle"><input type="checkbox" ${common}${view.configValue === true ? ' checked' : ''}${disabled} /> <span>Enabled</span></label>`;
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
      return `<output ${common} class="e5-readonly">${esc(view.valueText)}</output>`;
    case 'action':
      return `<span ${common} class="e5-actionkind">Bounded action</span>`;
    default:
      return `<input type="text" ${common} value="${view.configValue !== null && typeof view.configValue === 'string' ? esc(view.configValue) : ''}" placeholder="Inherit (not configured)" autocomplete="off" spellcheck="false"${disabled} />`;
  }
}

function controlMarkup(view: E5ScreenView['controls'][number]): string {
  const control = view.control;
  const binding = view.binding;
  const status = view.applyState === null ? '' : `<span class="e5-apply-state" data-apply-state="${esc(view.applyState)}">${esc(view.applyState)}</span>`;
  const disabledAction = view.disabled ? ' disabled aria-disabled="true"' : '';
  return `<article class="e5-control" id="e5-control-${control.id}" data-control="${control.id}" data-state="${view.state}" data-kind="${binding.input}" data-command="${binding.command}" tabindex="-1" aria-labelledby="e5-name-${control.id}">
    <header class="e5-control-head">
      <span class="e5-control-id">${control.id}</span>
      <h3 id="e5-name-${control.id}" class="e5-control-name">${esc(control.name)}</h3>
      <span class="e5-control-kind">${esc(control.control)}</span>
      ${status}
      <span class="e5-state-badge" data-state="${view.state}">${view.state}</span>
    </header>
    <p class="e5-behavior">${esc(control.behavior)}</p>
    <dl class="e5-contract">
      <div><dt>Default</dt><dd>${esc(control.defaultState)}</dd></div>
      <div><dt>Scope</dt><dd>${esc(control.scope)}</dd></div>
      <div><dt>Apply timing</dt><dd>${esc(control.applyTiming)}</dd></div>
      <div><dt>Config key</dt><dd class="e5-wrap">${esc(binding.configKey ?? 'No configuration key; bounded action or read')}</dd></div>
    </dl>
    <div class="e5-field">
      <label for="e5-input-${control.id}" class="e5-field-label">${binding.input === 'readonly' ? 'Served value' : binding.input === 'action' ? 'Action' : 'Value'}</label>
      ${controlInput(control, view)}
      <p id="e5-desc-${control.id}" class="e5-help">${esc(view.valueText)}${view.source === null ? '' : ` · source ${esc(view.source)}`}</p>
    </div>
    <div class="e5-control-actions">
      <button type="button" class="e5-primary" data-activate="${control.id}"${view.approvalRequestId === null ? '' : ` data-request-id="${esc(view.approvalRequestId)}"`}${disabledAction || ((control.id === 'Q-06' || control.id === 'Q-07') && view.approvalRequestId === null) ? ' disabled' : ''}>${esc(binding.actionLabel)}</button>
      <button type="button" class="e5-secondary" data-inspect="${control.id}">Details</button>
      <button type="button" class="e5-secondary" data-unstage="${control.id}"${binding.configKey === null ? ' disabled' : ''}>Reset this override</button>
    </div>
    <div class="e5-control-state" data-control-state="${control.id}">${esc(view.stateText)}</div>
    <details class="e5-states">
      <summary>Empty, error and disabled states</summary>
      <p><strong>Empty.</strong> ${esc(control.emptyState)}</p>
      <p><strong>Error.</strong> ${esc(control.errorState)}</p>
      <p><strong>Disabled.</strong> ${esc(control.disabledState)}</p>
      <p class="e5-trace">Trace: ${esc(control.classification)} · ${esc(control.currentStatus)} · ${control.acceptanceIds.join(', ')} · spec p.${control.specPage}</p>
    </details>
  </article>`;
}

function groupControls(views: E5ScreenView['controls']): string {
  const groups = new Map<string, E5ScreenView['controls'][number][]>();
  for (const view of views) {
    const list = groups.get(view.control.category) ?? [];
    list.push(view);
    groups.set(view.control.category, list);
  }
  return [...groups.entries()]
    .map(
      ([category, list]) => `<section class="e5-group" aria-label="${esc(category)}">
        <h3 class="e5-group-title">${esc(category)} <span class="e5-count">${list.length} controls</span></h3>
        <div class="e5-control-grid">${list.map(controlMarkup).join('\n')}</div>
      </section>`,
    )
    .join('\n');
}

function providerToolbar(view: E5ScreenView): string {
  const firstConnection = view.controls.find((entry) => entry.control.id === 'P-01');
  const canCreate = firstConnection?.state !== 'disabled';
  return `<section class="e5-toolbar" aria-label="Provider connection editor">
    <div class="e5-toolbar-head"><h3>Connection lifecycle</h3><span class="e5-muted">Stable instance ID, API family, endpoint and credential reference. Test stages run separately.</span></div>
    <div class="e5-inline-form">
      <label>Label <input type="text" data-toolbar="provider-label" placeholder="e.g. Work Ollama" autocomplete="off" /></label>
      <label>Family <select data-toolbar="provider-kind">
        <option value="ollama">Ollama</option>
        <option value="openai_chat">OpenAI Chat</option>
        <option value="openai_responses">OpenAI Responses</option>
        <option value="anthropic">Anthropic</option>
        <option value="google">Google</option>
        <option value="deepseek">DeepSeek</option>
        <option value="gateway">Gateway</option>
      </select></label>
      <label>Endpoint <input type="text" data-toolbar="provider-url" placeholder="http://127.0.0.1:11434" autocomplete="off" spellcheck="false" /></label>
      <label>Credential reference <input type="text" data-toolbar="provider-credential" placeholder="env:PROVIDER_KEY" autocomplete="off" spellcheck="false" /></label>
      <button type="button" class="e5-primary" data-action="provider.create"${canCreate ? '' : ' disabled'}>Add connection</button>
      <button type="button" class="e5-secondary" data-action="provider.test"${canCreate ? '' : ' disabled'}>Test staged</button>
      <button type="button" class="e5-secondary" data-action="provider.rotate"${canCreate ? '' : ' disabled'}>Rotate credential</button>
      <button type="button" class="e5-danger" data-action="provider.delete"${canCreate ? '' : ' disabled'}>Delete</button>
    </div>
    <p class="e5-help">A 401 offers Rotate credential; a blocked destination shows endpoint policy; discovery failure keeps manually declared model IDs intact.</p>
  </section>`;
}

function modelToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Model picker">
    <div class="e5-toolbar-head"><h3>Models and routing</h3><span class="e5-muted">Search by exact ID, provider or capability. Favorites and recents are user-local.</span></div>
    <div class="e5-inline-form">
      <label class="e5-grow">Search models <input type="search" data-toolbar="model-search" placeholder="Exact ID, provider or capability" autocomplete="off" spellcheck="false" /></label>
      <button type="button" class="e5-secondary" data-action="catalog.refresh">Refresh catalog</button>
      <button type="button" class="e5-secondary" data-action="catalog.probe">Probe exact model</button>
      <button type="button" class="e5-secondary" data-action="model.favorite">Favorite</button>
      <button type="button" class="e5-primary" data-action="model.select">Select pair</button>
    </div>
  </section>`;
}

function generationToolbar(view: E5ScreenView): string {
  const incompatible = view.rows.filter((row) => row.key.startsWith('incompatible-'));
  return `<section class="e5-toolbar" aria-label="Generation tuning">
    <div class="e5-toolbar-head"><h3>Generation bundle</h3><span class="e5-muted">Unset is distinct from zero. Preview shows the normalized request and apply timing.</span></div>
    <div class="e5-inline-form">
      <button type="button" class="e5-primary" data-action="generation.preview">Preview normalized request</button>
      <button type="button" class="e5-secondary" data-action="generation.reset">Reset all overrides</button>
    </div>
    ${incompatible.length === 0 ? '' : `<ul class="e5-warnings" aria-label="Incompatible parameters">${incompatible.map((row) => `<li>${esc(row.label)}: ${esc(row.value)}</li>`).join('')}</ul>`}
  </section>`;
}

function contextToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Context assembly">
    <div class="e5-toolbar-head"><h3>Context and capacity</h3><span class="e5-muted">Every included source carries URI, range, version and an inclusion reason. Reserves are not consumed input.</span></div>
    <div class="e5-inline-form">
      <label class="e5-grow">Mention a source <input type="text" data-toolbar="context-uri" placeholder="@file, selection, diagnostics, terminal, diff, URL or prior chat" autocomplete="off" spellcheck="false" /></label>
      <button type="button" class="e5-primary" data-action="context.add">Add source</button>
      <button type="button" class="e5-secondary" data-action="context.preview">Preview capacity</button>
      <button type="button" class="e5-secondary" data-action="context.export">Export manifest</button>
      <button type="button" class="e5-secondary" data-action="context.reindex">Reindex</button>
    </div>
  </section>`;
}

function compactionToolbar(view: E5ScreenView): string {
  const hasPlan = view.rows.some((row) => row.key === 'compaction-retained');
  return `<section class="e5-toolbar" aria-label="Compaction review">
    <div class="e5-toolbar-head"><h3>Continuity</h3><span class="e5-muted">A successful compaction must meet the configured minimum reduction. Pending approvals and unknown effects survive.</span></div>
    <div class="e5-inline-form">
      <button type="button" class="e5-primary" data-action="compaction.preview">Preview plan</button>
      <button type="button" class="e5-secondary" data-action="compaction.run"${hasPlan ? '' : ' disabled'}>Compact now</button>
      <button type="button" class="e5-secondary" data-action="compaction.cancel">Cancel</button>
    </div>
  </section>`;
}

function policyToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Permission policy">
    <div class="e5-toolbar-head"><h3>Effective authority</h3><span class="e5-muted">Preview one operation before applying a versioned rule. Managed denies cannot be overridden by text.</span></div>
    <div class="e5-inline-form">
      <label>Tool <input type="text" data-toolbar="policy-tool" placeholder="shell.run" /></label>
      <label class="e5-grow">Target / argv <input type="text" data-toolbar="policy-target" placeholder="git status" /></label>
      <button type="button" class="e5-primary" data-action="policy.preview">Test match</button>
      <button type="button" class="e5-secondary" data-action="policy.apply">Save rule</button>
      <button type="button" class="e5-secondary" data-action="policy.revoke">Revoke rule</button>
      <button type="button" class="e5-secondary" data-action="policy.audit">Decision log</button>
    </div>
  </section>`;
}

function approvalToolbar(view: E5ScreenView): string {
  const firstPending = view.rows.find((row) => row.key.startsWith('approval-') && !row.key.startsWith('approval-history-'));
  // Bind the buttons to the request id that produced THIS render. The panel
  // refuses when that exact request is no longer live instead of replying to
  // whatever request happens to be first at click time.
  const requestId = firstPending === undefined ? '' : firstPending.key.slice('approval-'.length);
  const allowOnce = view.controls.find((entry) => entry.control.id === 'Q-06');
  const canReply = allowOnce?.state !== 'disabled' && requestId.length > 0;
  const remember = view.controls.find((entry) => entry.control.id === 'Q-07');
  return `<section class="e5-toolbar" aria-label="Approval dock">
    <div class="e5-toolbar-head"><h3>Exact waiter</h3><span class="e5-muted">${firstPending === undefined ? 'No request is pending.' : 'One reply addresses this generation only.'}</span></div>
    <div class="e5-inline-form">
      <button type="button" class="e5-primary" data-action="approval.reply" data-decision="allow_once" data-request-id="${esc(requestId)}"${canReply ? '' : ' disabled'}>Allow once</button>
      <button type="button" class="e5-danger" data-action="approval.reply" data-decision="deny" data-request-id="${esc(requestId)}"${canReply ? '' : ' disabled'}>Deny</button>
      <button type="button" class="e5-secondary" data-action="approval.remember" data-request-id="${esc(requestId)}"${remember?.state === 'disabled' || requestId.length === 0 ? ' disabled' : ''}>Remember…</button>
      <button type="button" class="e5-secondary" data-action="approval.refresh">Refresh</button>
    </div>
  </section>`;
}

function mcpToolbar(): string {
  return `<section class="e5-toolbar" aria-label="MCP server editor">
    <div class="e5-toolbar-head"><h3>Trusted stdio server</h3><span class="e5-muted">A config file alone never starts an executable. Bounds are bytes, not character guesses.</span></div>
    <div class="e5-inline-form">
      <label>Name <input type="text" data-toolbar="mcp-name" placeholder="docs" /></label>
      <label class="e5-grow">Command <input type="text" data-toolbar="mcp-command" placeholder="/usr/local/bin/mcp-server" /></label>
      <label class="e5-grow">Args (one per line) <textarea data-toolbar="mcp-args" rows="2" placeholder="--stdio"></textarea></label>
      <button type="button" class="e5-primary" data-action="mcp.create">Save server</button>
      <button type="button" class="e5-secondary" data-action="mcp.start">Start</button>
      <button type="button" class="e5-secondary" data-action="mcp.stop">Stop</button>
      <button type="button" class="e5-secondary" data-action="mcp.restart">Restart</button>
      <button type="button" class="e5-secondary" data-action="mcp.test">Test</button>
      <button type="button" class="e5-danger" data-action="mcp.remove">Remove</button>
    </div>
  </section>`;
}

function hookToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Hook testing">
    <div class="e5-toolbar-head"><h3>Bounded lifecycle test</h3><span class="e5-muted">A timeout stops the owned process tree; partial stdout cannot authorize an Allow verdict.</span></div>
    <div class="e5-inline-form">
      <button type="button" class="e5-primary" data-action="hook.test">Run bounded test</button>
      <button type="button" class="e5-secondary" data-action="hook.save">Save version</button>
    </div>
  </section>`;
}

function agentsToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Child agent controls">
    <div class="e5-toolbar-head"><h3>Steer and lifecycle</h3><span class="e5-muted">Steer is acknowledgement-bound; empty or over-500-character text is rejected, never truncated.</span></div>
    <div class="e5-inline-form">
      <label class="e5-grow">Steer the live child <textarea data-toolbar="steer-text" rows="2" placeholder="Up to 500 characters"></textarea></label>
      <button type="button" class="e5-primary" data-action="steer.send">Send steer</button>
      <button type="button" class="e5-secondary" data-action="agent.pause">Pause child</button>
      <button type="button" class="e5-danger" data-action="agent.stop">Stop child</button>
    </div>
  </section>`;
}

function terminalToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Owned processes">
    <div class="e5-toolbar-head"><h3>Process ownership</h3><span class="e5-muted">A stale PID cannot authorize kill. Process stop, task stop and session abort are separate scopes.</span></div>
    <div class="e5-inline-form">
      <button type="button" class="e5-primary" data-action="terminal.background">Run in background</button>
      <button type="button" class="e5-danger" data-action="terminal.stop">Stop owned process</button>
      <button type="button" class="e5-secondary" data-action="terminal.reconcile">Reconcile unknown effect</button>
    </div>
  </section>`;
}

function historyToolbar(): string {
  return `<section class="e5-toolbar" aria-label="History search">
    <div class="e5-toolbar-head"><h3>History and Find</h3><span class="e5-muted">Title search is separate from transcript find. Archived is not deleted.</span></div>
    <div class="e5-inline-form">
      <label class="e5-grow">Search conversation titles <input type="search" data-toolbar="history-query" placeholder="Title contains…" autocomplete="off" spellcheck="false" /></label>
      <button type="button" class="e5-primary" data-action="search.titles">Search titles</button>
      <label class="e5-grow">Find in transcript <input type="search" data-toolbar="transcript-query" placeholder="Find across unloaded pages…" autocomplete="off" spellcheck="false" /></label>
      <button type="button" class="e5-secondary" data-action="search.transcript">Find</button>
      <button type="button" class="e5-secondary" data-action="session.rename">Rename</button>
      <button type="button" class="e5-secondary" data-action="session.pin">Pin</button>
      <button type="button" class="e5-secondary" data-action="session.archive">Archive</button>
      <button type="button" class="e5-secondary" data-action="session.export">Export</button>
    </div>
  </section>`;
}

function proofToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Finish proof">
    <div class="e5-toolbar-head"><h3>Finish level</h3><span class="e5-muted">Conversation completion proves none of acceptance, claims, landing equality or finish. The outcome stays Pending until all facts match.</span></div>
    <div class="e5-inline-form">
      <label>Finish <select data-toolbar="finish-level"><option value="local">Local</option><option value="commit">Commit</option><option value="push">Push</option><option value="pr">PR</option></select></label>
      <button type="button" class="e5-primary" data-action="finish.request">Request finish</button>
      <button type="button" class="e5-secondary" data-action="change.review">Open candidate diff</button>
      <button type="button" class="e5-secondary" data-action="restore.preview">Preview restore</button>
    </div>
  </section>`;
}

function settingsToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Settings search">
    <div class="e5-toolbar-head"><h3>Effective settings</h3><span class="e5-muted">Search explains the winning source. Apply is one atomic patch against a known revision.</span></div>
    <div class="e5-inline-form">
      <label class="e5-grow">Search settings <input type="search" data-toolbar="settings-query" placeholder="Key, value or source…" autocomplete="off" spellcheck="false" /></label>
      <button type="button" class="e5-primary" data-action="settings.search">Search</button>
      <button type="button" class="e5-secondary" data-action="settings.import">Import bundle</button>
      <button type="button" class="e5-secondary" data-action="settings.export">Export redacted</button>
    </div>
  </section>`;
}

function diagnosticsToolbar(): string {
  return `<section class="e5-toolbar" aria-label="Diagnostics">
    <div class="e5-toolbar-head"><h3>Runtime trust and recovery</h3><span class="e5-muted">Checksum mismatch refuses start. A checksum match is not publisher-signature verification.</span></div>
    <div class="e5-inline-form">
      <button type="button" class="e5-secondary" data-action="diagnostics.preview">Preview redacted bundle</button>
      <button type="button" class="e5-primary" data-action="diagnostics.export">Export diagnostics</button>
      <button type="button" class="e5-secondary" data-action="diagnostics.lifecycle" data-lifecycle="start">Start runtime</button>
      <button type="button" class="e5-secondary" data-action="diagnostics.lifecycle" data-lifecycle="stop">Stop runtime</button>
      <button type="button" class="e5-secondary" data-action="binding.revalidate">Revalidate binding</button>
    </div>
  </section>`;
}

function screenToolbar(screenId: string, view: E5ScreenView): string {
  switch (screenId) {
    case 'S02':
      return modelToolbar();
    case 'S03':
      return providerToolbar(view);
    case 'S05':
      return generationToolbar(view);
    case 'S08':
    case 'S09':
      return contextToolbar();
    case 'S10':
      return compactionToolbar(view);
    case 'S13':
      return policyToolbar();
    case 'S14':
      return approvalToolbar(view);
    case 'S15':
    case 'S16':
      return mcpToolbar();
    case 'S18':
      return hookToolbar();
    case 'S19':
      return terminalToolbar();
    case 'S23':
      return agentsToolbar();
    case 'S26':
      return historyToolbar();
    case 'S29':
      return settingsToolbar();
    case 'S30':
    case 'S31':
      return diagnosticsToolbar();
    case 'S32':
      return proofToolbar();
    default:
      return '';
  }
}

function screenMarkup(view: E5ScreenView, initial: boolean): string {
  const screen = view.screen;
  const toolbar = screenToolbar(screen.id, view);
  return `<section class="e5-screen" id="e5-screen-${screen.id}" data-screen="${screen.id}" data-destination="${destinationOf(screen.id)}"${initial ? '' : ' hidden'} aria-labelledby="e5-screen-title-${screen.id}">
    <header class="e5-screen-head">
      <div class="e5-kicker">${screen.id} · ${esc(destinationLabel(destinationOf(screen.id)))}</div>
      <h2 id="e5-screen-title-${screen.id}">${esc(screen.title)}</h2>
      <p class="e5-purpose">${esc(screen.purpose)}</p>
      <dl class="e5-screen-contract">
        <div><dt>Action</dt><dd>${esc(screen.action)}</dd></div>
        <div><dt>Recovery</dt><dd>${esc(screen.recovery)}</dd></div>
        <div><dt>Visual</dt><dd>${esc(screen.visual)}</dd></div>
        <div><dt>Navigation</dt><dd>${esc(screen.navigation)}</dd></div>
      </dl>
      <details class="e5-plate"><summary>Host variant and traceability</summary><p>${esc(screen.vscodeVariant)}</p><p class="e5-muted">${esc(screen.jetbrainsVariant)}</p><p class="e5-trace">${esc(screen.traceability)}</p></details>
    </header>
    ${view.staleText === null ? '' : `<div class="e5-banner e5-warn" role="alert">${esc(view.staleText)}</div>`}
    ${view.emptyText === null ? '' : `<div class="e5-banner e5-empty" role="status">${esc(view.emptyText)}</div>`}
    ${toolbar}
    ${view.rows.length === 0 ? '' : `<section class="e5-data" aria-label="Served ${esc(screen.id)} state">${view.rows.map(rowMarkup).join('\n')}</section>`}
    ${groupControls(view.controls)}
  </section>`;
}

function destinationMarkup(destination: E5Destination, state: E5PanelState, initialScreen: string): string {
  return `<div class="e5-destination" data-destination-panel="${destination}"${destinationOf(initialScreen) === destination ? '' : ' hidden'}>
    <div class="e5-destination-head">
      <h2>${esc(destinationLabel(destination))}</h2>
      <span class="e5-muted">${esc(DESTINATIONS.find((entry) => entry.key === destination)?.hint ?? '')}</span>
    </div>
    <div class="e5-split">
      ${nav(destination, initialScreen)}
      <div class="e5-screens-host">
        ${screensFor(destination)
          .map((screen) => screenMarkup(screenView(screen.id, state), screen.id === initialScreen))
          .join('\n')}
      </div>
    </div>
  </div>`;
}

export function renderEdition5Html(options: Edition5HtmlOptions): string {
  const state = options.state;
  const snapshot = state.snapshot;
  const initialDestination = destinationOf(options.initialScreen);
  const stale = snapshot?.stale === true;
  const controlCount = E5_CONTROLS.length;
  const screenCount = E5_SCREENS.length;
  const categoryCount = E5_CATEGORIES.length;
  const csp = [
    "default-src 'none'",
    `style-src ${options.cspSource} 'nonce-${options.nonce}'`,
    `font-src ${options.cspSource}`,
    `img-src ${options.cspSource} data:`,
    `script-src 'nonce-${options.nonce}'`,
    "connect-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
  ].join('; ');
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${options.styleUri}" rel="stylesheet">
<title>Faktor Control Center</title>
</head>
<body>
<div id="e5-app" data-host="${options.host}" data-stale="${stale ? 'true' : 'false'}">
  <header class="e5-header">
    <div class="e5-brand"><span class="e5-mark" aria-hidden="true">F</span><span>Faktor Control Center</span></div>
    <div class="e5-header-status" role="status" aria-live="polite">
      <span class="e5-dot${stale ? ' e5-dot-warn' : ''}" aria-hidden="true"></span>
      <span data-field="revision">${snapshot === null ? 'No served state' : `Config r${esc(snapshot.config?.revision ?? snapshot.revision)}`}</span>
      <span class="e5-muted" data-field="binding">${snapshot?.binding === null || snapshot?.binding === undefined ? 'No binding' : esc(snapshot.binding.session ?? 'unbound')}</span>
    </div>
    <nav class="e5-router" id="e5-router" role="tablist" aria-label="Faktor destinations">
      ${DESTINATIONS.map(
        (entry) =>
          `<button type="button" class="e5-router-tab" role="tab" id="e5-nav-${entry.key}" data-destination="${entry.key}" aria-selected="${entry.key === initialDestination ? 'true' : 'false'}" tabindex="${entry.key === initialDestination ? '0' : '-1'}" aria-controls="e5-destination-${entry.key}">${entry.label}</button>`,
      ).join('')}
    </nav>
    <button type="button" class="e5-secondary" data-action="inspect" id="e5-refresh">Refresh</button>
  </header>
  <div id="e5-notice" class="e5-notice" role="status" aria-live="polite"></div>
  <div id="e5-error" class="e5-error" role="alert"${state.error === null ? ' hidden' : ''}>${state.error === null ? '' : esc(state.error)}</div>
  <main id="e5-main">
    ${DESTINATIONS.map((entry) => destinationMarkup(entry.key, state, options.initialScreen)).join('\n')}
  </main>
  <footer class="e5-footer" id="e5-footer">
    <div class="e5-footer-meta">
      <span data-field="screen">${esc(screenTitle(options.initialScreen))}</span>
      <span class="e5-muted">${controlCount} controls · ${screenCount} screens · ${categoryCount} categories</span>
      <span id="e5-staged-count" class="e5-staged">No staged changes</span>
    </div>
    <div class="e5-footer-patch" id="e5-patch-list" aria-label="Staged changes"></div>
    <div class="e5-footer-actions">
      <button type="button" class="e5-secondary" id="e5-reset">Reset</button>
      <button type="button" class="e5-secondary" id="e5-defaults">Restore defaults</button>
      <button type="button" class="e5-primary" id="e5-apply" disabled>Apply changes</button>
    </div>
    <div id="e5-apply-state" class="e5-apply" role="status" aria-live="polite"></div>
  </footer>
</div>
<script nonce="${options.nonce}" src="${options.scriptUri}"></script>
</body>
</html>`;
}
